//! Replay logic for mega-evm v1.0.1

use std::time::Instant;

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{Bytes, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Block;
use alloy_rpc_types_trace::geth::GethDefaultTracingOptions;
use mega_evm::{
    alloy_evm::{block::BlockExecutor, EvmEnv},
    alloy_hardforks::{EthereumHardfork, ForkCondition},
    alloy_op_hardforks::{EthereumHardforks, OpHardfork, OpHardforks},
    revm::{
        context::{cfg::CfgEnv, result::ExecutionResult, BlockEnv as RevmBlockEnv, ContextTr},
        database::StateBuilder,
        DatabaseRef,
    },
    MegaSpecId,
};

// Import v1.0.1 types
use mega_evm_v1_0_1 as mega_evm_v1;
use op_alloy_rpc_types::Transaction;
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use crate::{
    common::{op_receipt_to_tx_receipt, EvmeOutcome},
    run::{self, TracerType},
};

use super::{ReplayError, ReplayOutcome, Result};

#[derive(Debug, Clone, Copy)]
struct FixedHardforkV1 {
    #[allow(dead_code)]
    spec: mega_evm_v1::MegaSpecId,
}

impl EthereumHardforks for FixedHardforkV1 {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        if fork <= EthereumHardfork::Prague {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}

impl OpHardforks for FixedHardforkV1 {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        if fork <= OpHardfork::Isthmus {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}

/// Execute transactions with block executor and optional tracing (using mega-evm v1.0.1)
pub(super) async fn execute_transactions_v1_0_1<P>(
    database: &mut run::EvmeState<op_alloy_network::Optimism, P>,
    parent_block: &Block<Transaction>,
    block: &Block<Transaction>,
    evm_env: EvmEnv<MegaSpecId>,
    provider: &P,
    preceding_transactions: Vec<B256>,
    target_tx: &Transaction,
    spec_id: MegaSpecId,
    tracer: Option<TracerType>,
    trace_disable_storage: bool,
    trace_disable_memory: bool,
    trace_disable_stack: bool,
    trace_enable_return_data: bool,
) -> Result<ReplayOutcome>
where
    P: Provider<op_alloy_network::Optimism> + std::fmt::Debug,
{
    // Convert MegaSpecId to v1.0.1 equivalent
    let spec_v1 = match spec_id {
        MegaSpecId::EQUIVALENCE => mega_evm_v1::MegaSpecId::EQUIVALENCE,
        MegaSpecId::MINI_REX => mega_evm_v1::MegaSpecId::MINI_REX,
        _ => {
            return Err(ReplayError::ExecutionError(format!(
                "Unsupported spec id for v1.0.1: {:?}",
                spec_id
            )))
        }
    };

    // Note: FixedHardfork is shared between versions, but spec types differ
    // We need to create a separate FixedHardfork for v1.0.1
    let chain_spec_v1 = FixedHardforkV1 { spec: spec_v1 };
    let external_env_factory_v1: mega_evm_v1::DefaultExternalEnvs =
        mega_evm_v1::DefaultExternalEnvs::new();
    let evm_factory_v1 = mega_evm_v1::MegaEvmFactory::new(external_env_factory_v1);
    let block_executor_factory_v1 = mega_evm_v1::MegaBlockExecutorFactory::new(
        chain_spec_v1,
        evm_factory_v1,
        mega_evm::alloy_op_evm::block::OpAlloyReceiptBuilder::default(),
    );

    let block_limits_v1 = match spec_v1 {
        mega_evm_v1::MegaSpecId::EQUIVALENCE => {
            mega_evm_v1::BlockLimits::no_limits().fit_equivalence()
        }
        mega_evm_v1::MegaSpecId::MINI_REX => mega_evm_v1::BlockLimits::no_limits().fit_mini_rex(),
    };

    let block_ctx_v1 = mega_evm_v1::MegaBlockExecutionCtx::new(
        parent_block.hash(),
        block.header.parent_beacon_block_root(),
        block.header.extra_data().clone(),
        block_limits_v1,
    );

    // Execute transactions with inspector (trace will be generated only if enabled)
    let start = Instant::now();

    // Setup tracing inspector
    let config = TracingInspectorConfig::all();
    let mut inspector = TracingInspector::new(config);

    // Create state and block executor with inspector using v1.0.1
    let mut state_v1 = StateBuilder::new().with_database(database).with_bundle_update().build();

    // Convert EvmEnv to match what v1.0.1 expects
    let mut cfg_v1 = CfgEnv::default();
    cfg_v1.chain_id = evm_env.cfg_env.chain_id;
    cfg_v1.spec = spec_v1;

    let evm_env_v1 = EvmEnv::new(
        cfg_v1,
        RevmBlockEnv {
            number: evm_env.block_env.number,
            beneficiary: evm_env.block_env.beneficiary,
            timestamp: evm_env.block_env.timestamp,
            gas_limit: evm_env.block_env.gas_limit,
            basefee: evm_env.block_env.basefee,
            difficulty: evm_env.block_env.difficulty,
            prevrandao: evm_env.block_env.prevrandao,
            blob_excess_gas_and_price: evm_env.block_env.blob_excess_gas_and_price,
        },
    );

    let mut block_executor_v1 = block_executor_factory_v1.create_executor_with_inspector(
        &mut state_v1,
        block_ctx_v1,
        evm_env_v1,
        &mut inspector,
    );

    // Apply pre-execution changes
    block_executor_v1
        .apply_pre_execution_changes()
        .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;

    // Execute preceding transactions
    for tx_hash in preceding_transactions {
        let tx = provider
            .get_transaction_by_hash(tx_hash)
            .await
            .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?
            .ok_or(ReplayError::TransactionNotFound(tx_hash))?;
        let tx = tx.as_recovered();
        let outcome = block_executor_v1
            .execute_mega_transaction(tx)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        block_executor_v1
            .commit_execution_outcome(outcome)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
    }

    // Execute target transaction
    let pre_execution_nonce = block_executor_v1
        .evm()
        .db_ref()
        .basic_ref(target_tx.as_recovered().signer())?
        .map(|acc| acc.nonce)
        .unwrap_or(0);
    block_executor_v1.inspector_mut().fuse();
    let outcome = block_executor_v1
        .execute_mega_transaction(target_tx.as_recovered())
        .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
    let exec_result = outcome.inner.result.clone();
    let evm_state = outcome.inner.state.clone();
    block_executor_v1
        .commit_execution_outcome(outcome)
        .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;

    let duration = start.elapsed();

    // Obtain receipt envelope
    let block_result = block_executor_v1
        .apply_post_execution_changes()
        .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
    let receipt_envelope = block_result.receipts.last().unwrap().clone();

    // Generate trace only if tracing is enabled
    let trace_data = if matches!(tracer, Some(TracerType::Trace)) {
        // Generate GethTrace
        let geth_builder = inspector.geth_builder();

        // Create GethDefaultTracingOptions based on CLI arguments
        let opts = GethDefaultTracingOptions {
            disable_storage: Some(trace_disable_storage),
            disable_memory: Some(trace_disable_memory),
            disable_stack: Some(trace_disable_stack),
            enable_return_data: Some(trace_enable_return_data),
            ..Default::default()
        };

        // Get output for trace generation
        let output = match &exec_result {
            ExecutionResult::Success { output, .. } => output.data().to_vec(),
            ExecutionResult::Revert { output, .. } => output.to_vec(),
            _ => Vec::new(),
        };

        // Generate the geth trace
        let geth_trace =
            geth_builder.geth_traces(exec_result.gas_used(), Bytes::from(output), opts);

        // Format as JSON
        Some(
            serde_json::to_string_pretty(&geth_trace)
                .unwrap_or_else(|e| format!("Error serializing trace: {}", e)),
        )
    } else {
        None
    };

    // Convert ExecutionResult from v1.0.1 to current version
    // The structures are the same, but the HaltReason type parameter differs
    let exec_result_current = match exec_result {
        ExecutionResult::Success { reason, gas_used, gas_refunded, logs, output } => {
            ExecutionResult::Success { reason, gas_used, gas_refunded, logs, output }
        }
        ExecutionResult::Revert { gas_used, output } => {
            ExecutionResult::Revert { gas_used, output }
        }
        ExecutionResult::Halt { reason, gas_used } => {
            // Convert MegaHaltReason from v1.0.1 to current version
            // We need to map the halt reasons appropriately
            use mega_evm::EthHaltReason;
            use mega_evm_v1::MegaHaltReason as MegaHaltReasonV1;

            let reason_current = match reason {
                MegaHaltReasonV1::Base(op_reason) => {
                    use mega_evm_v1::OpHaltReason as OpHaltReasonV1;
                    match op_reason {
                        OpHaltReasonV1::Base(eth_reason) => {
                            use mega_evm_v1::EthHaltReason as EthHaltReasonV1;
                            let eth_current = match eth_reason {
                                EthHaltReasonV1::OutOfGas(og) => EthHaltReason::OutOfGas(og),
                                EthHaltReasonV1::OpcodeNotFound => EthHaltReason::OpcodeNotFound,
                                EthHaltReasonV1::InvalidFEOpcode => EthHaltReason::InvalidFEOpcode,
                                EthHaltReasonV1::InvalidJump => EthHaltReason::InvalidJump,
                                EthHaltReasonV1::NotActivated => EthHaltReason::NotActivated,
                                EthHaltReasonV1::StackUnderflow => EthHaltReason::StackUnderflow,
                                EthHaltReasonV1::StackOverflow => EthHaltReason::StackOverflow,
                                EthHaltReasonV1::OutOfOffset => EthHaltReason::OutOfOffset,
                                EthHaltReasonV1::CreateCollision => EthHaltReason::CreateCollision,
                                EthHaltReasonV1::PrecompileError => EthHaltReason::PrecompileError,
                                EthHaltReasonV1::NonceOverflow => EthHaltReason::NonceOverflow,
                                EthHaltReasonV1::CreateContractSizeLimit => {
                                    EthHaltReason::CreateContractSizeLimit
                                }
                                EthHaltReasonV1::CreateContractStartingWithEF => {
                                    EthHaltReason::CreateContractStartingWithEF
                                }
                                EthHaltReasonV1::CreateInitCodeSizeLimit => {
                                    EthHaltReason::CreateInitCodeSizeLimit
                                }
                                EthHaltReasonV1::OverflowPayment => EthHaltReason::OverflowPayment,
                                EthHaltReasonV1::StateChangeDuringStaticCall => {
                                    EthHaltReason::StateChangeDuringStaticCall
                                }
                                EthHaltReasonV1::CallNotAllowedInsideStatic => {
                                    EthHaltReason::CallNotAllowedInsideStatic
                                }
                                EthHaltReasonV1::OutOfFunds => EthHaltReason::OutOfFunds,
                                EthHaltReasonV1::CallTooDeep => EthHaltReason::CallTooDeep,
                                // These variants exist in v1.0.1 but not in current version,
                                // fall back to a generic error
                                #[allow(unreachable_patterns)]
                                _ => EthHaltReason::PrecompileError,
                            };
                            mega_evm::MegaHaltReason::Base(mega_evm::OpHaltReason::Base(
                                eth_current,
                            ))
                        }
                        OpHaltReasonV1::FailedDeposit => {
                            mega_evm::MegaHaltReason::Base(mega_evm::OpHaltReason::FailedDeposit)
                        }
                    }
                }
                // Any other MegaHaltReason variants in v1.0.1 map to compute gas limit exceeded
                #[allow(unreachable_patterns)]
                _ => mega_evm::MegaHaltReason::ComputeGasLimitExceeded { limit: 0, actual: 0 },
            };
            ExecutionResult::Halt { reason: reason_current, gas_used }
        }
    };

    // Convert OpReceiptEnvelope to TransactionReceipt
    let from = target_tx.inner.inner.signer();
    let to = target_tx.inner.inner.to();
    let contract_address = if to.is_none() && receipt_envelope.is_success() {
        Some(from.create(pre_execution_nonce))
    } else {
        None
    };
    let receipt = op_receipt_to_tx_receipt(
        &receipt_envelope,
        block.number(),
        block.header.timestamp(),
        from,
        to,
        contract_address,
        target_tx.inner.effective_gas_price.unwrap_or(0),
    );

    Ok(ReplayOutcome {
        outcome: EvmeOutcome {
            pre_execution_nonce,
            exec_result: exec_result_current,
            state: evm_state,
            exec_time: duration,
            trace_data,
        },
        original_tx: target_tx.clone(),
        receipt,
    })
}
