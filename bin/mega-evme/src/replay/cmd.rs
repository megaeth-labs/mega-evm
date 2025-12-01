use std::time::{Duration, Instant};

use alloy_consensus::BlockHeader;
use alloy_primitives::{Bytes, B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::Block;
use alloy_rpc_types_trace::geth::GethDefaultTracingOptions;
use clap::Parser;
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use mega_evm::{
    alloy_evm::{block::BlockExecutor, EvmEnv},
    alloy_hardforks::{EthereumHardfork, ForkCondition},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    alloy_op_hardforks::{EthereumHardforks, OpHardfork, OpHardforks},
    revm::{
        context::{result::ExecutionResult, BlockEnv},
        context_interface::block::BlobExcessGasAndPrice,
        database::StateBuilder,
        state::EvmState,
    },
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaSpecId,
    TestExternalEnvs,
};
use op_alloy_consensus::OpReceiptEnvelope;
use op_alloy_rpc_types::Transaction;

use crate::run;

use super::{ReplayError, Result};

/// Replay a transaction from RPC
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Transaction hash to replay
    #[arg(value_name = "TX_HASH")]
    pub tx_hash: B256,

    /// RPC URL to fetch transaction from
    #[arg(long = "rpc", default_value = "http://localhost:8545")]
    pub rpc: String,

    // Shared argument groups
    /// Environment configuration
    #[command(flatten)]
    pub env_args: run::EnvArgs,

    /// State dump configuration
    #[command(flatten)]
    pub dump_args: run::StateDumpArgs,

    /// Trace configuration
    #[command(flatten)]
    pub trace_args: run::TraceArgs,
}

/// Execution result with optional trace data and state
struct ReplayResult {
    exec_result: ExecutionResult<mega_evm::MegaHaltReason>,
    state: EvmState,
    exec_time: Duration,
    original_tx: Transaction,
    receipt: OpReceiptEnvelope,
    trace_data: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct FixedHardfork {
    pub spec: MegaSpecId,
}

impl FixedHardfork {
    pub fn new(spec: MegaSpecId) -> Self {
        Self { spec }
    }
}

impl EthereumHardforks for FixedHardfork {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        if fork <= EthereumHardfork::Prague {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}

impl OpHardforks for FixedHardfork {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        if fork <= OpHardfork::Isthmus {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}

impl Cmd {
    /// Execute the replay command
    pub async fn run(&self) -> Result<()> {
        // Step 1: Fetch transaction from RPC
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<op_alloy_network::Optimism>()
            .connect_http(self.rpc.parse().map_err(|e| {
                ReplayError::RpcError(format!("Invalid RPC URL '{}': {}", self.rpc, e))
            })?);

        eprintln!("Fetching transaction {} from RPC {}", self.tx_hash, self.rpc);

        // Step x: fetch transaction
        let target_tx = provider
            .get_transaction_by_hash(self.tx_hash)
            .await
            .map_err(|e| ReplayError::RpcError(format!("Failed to fetch transaction: {}", e)))?
            .ok_or_else(|| ReplayError::TransactionNotFound(self.tx_hash.to_string()))?;

        eprintln!("Transaction found in block {:?}", target_tx.block_number);

        // Step x: determine block number to execute the transaction
        let (state_base_block_number, block_number, is_pending) =
            if let Some(block_number) = target_tx.block_number {
                (block_number - 1, block_number, false)
            } else {
                let latest_block_number =
                    provider.get_block_number().await.map_err(ReplayError::RpcTransportError)?;
                (latest_block_number, latest_block_number, true)
            };

        // Step 3: Setup initial state by forking from the parent block
        let prestate = Default::default();
        let storage = Default::default();
        let mut database = run::create_initial_state(
            Some(provider.clone()),
            Some(state_base_block_number),
            prestate,
            storage,
        )
        .await?;

        // Step 4: Setup BlockEnv and CfgEnv
        let parent_block = provider
            .get_block_by_number(state_base_block_number.into())
            .await
            .map_err(ReplayError::RpcTransportError)?
            .ok_or(ReplayError::BlockNotFound(state_base_block_number))?;
        let block = provider
            .get_block_by_number(block_number.into())
            .await
            .map_err(ReplayError::RpcTransportError)?
            .ok_or(ReplayError::BlockNotFound(block_number))?;
        let block_env = self.retrieve_block_env(&block).await?;
        let cfg_env = run::setup_cfg_env(&self.env_args);
        let evm_env = EvmEnv::new(cfg_env, block_env);

        // Step x: fetch preceeding transactions
        let mut preceeding_transactions = vec![];
        if !is_pending {
            for tx in block.transactions.hashes() {
                if tx == self.tx_hash {
                    break;
                }
                preceeding_transactions.push(tx);
            }
        }

        // Step x: Setup EVM and Block executor
        let chain_spec = FixedHardfork::new(self.env_args.spec_id());
        let external_env_factory = TestExternalEnvs::default();
        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_env_factory);
        let block_executor_factory = MegaBlockExecutorFactory::new(
            chain_spec,
            evm_factory,
            OpAlloyReceiptBuilder::default(),
        );
        let block_limits = match self.env_args.spec_id() {
            MegaSpecId::EQUIVALENCE => BlockLimits::no_limits().fit_equivalence(),
            MegaSpecId::MINI_REX => BlockLimits::no_limits().fit_mini_rex(),
            MegaSpecId::REX => BlockLimits::no_limits().fit_rex(),
            _ => panic!("Unsupported spec id: {:?}", self.env_args.spec_id()),
        };
        let block_ctx = MegaBlockExecutionCtx::new(
            parent_block.hash(),
            block.header.parent_beacon_block_root(),
            block.header.extra_data().clone(),
            block_limits,
        );
        // Step 5: Execute transactions with inspector (trace will be generated only if enabled)
        let start = Instant::now();

        // Setup tracing inspector
        let config = TracingInspectorConfig::all();
        let mut inspector = TracingInspector::new(config);

        // Create state and block executor with inspector
        let mut state =
            StateBuilder::new().with_database(&mut database).with_bundle_update().build();
        let mut block_executor =
            block_executor_factory.create_executor_with_inspector(&mut state, block_ctx, evm_env, &mut inspector);

        // Apply pre-execution changes
        block_executor
            .apply_pre_execution_changes()
            .map_err(ReplayError::BlockExecutionError)?;

        // Execute preceding transactions
        for tx_hash in preceeding_transactions {
            let tx = provider
                .get_transaction_by_hash(tx_hash)
                .await
                .map_err(ReplayError::RpcTransportError)?
                .ok_or(ReplayError::TransactionNotFound(tx_hash.to_string()))?;
            let tx = tx.as_recovered();
            let outcome = block_executor
                .execute_mega_transaction(tx)
                .map_err(ReplayError::BlockExecutionError)?;
            block_executor
                .commit_execution_outcome(outcome)
                .map_err(ReplayError::BlockExecutionError)?;
        }

        // Execute target transaction
        let outcome = block_executor
            .execute_mega_transaction(target_tx.as_recovered())
            .map_err(ReplayError::BlockExecutionError)?;
        let exec_result = outcome.inner.result.clone();
        let evm_state = outcome.inner.state.clone();
        block_executor
            .commit_execution_outcome(outcome)
            .map_err(ReplayError::BlockExecutionError)?;

        let duration = start.elapsed();

        // Obtain receipt
        let block_result = block_executor
            .apply_post_execution_changes()
            .map_err(ReplayError::BlockExecutionError)?;
        let receipt = block_result.receipts.last().unwrap().clone();

        // Generate trace only if tracing is enabled
        let trace_data = if matches!(self.trace_args.tracer, Some(crate::run::TracerType::Trace)) {
            // Generate GethTrace
            let geth_builder = inspector.geth_builder();

            // Create GethDefaultTracingOptions based on CLI arguments
            let opts = GethDefaultTracingOptions {
                disable_storage: Some(self.trace_args.trace_disable_storage),
                disable_memory: Some(self.trace_args.trace_disable_memory),
                disable_stack: Some(self.trace_args.trace_disable_stack),
                enable_return_data: Some(self.trace_args.trace_enable_return_data),
                ..Default::default()
            };

            // Get output for trace generation
            let output = match &exec_result {
                ExecutionResult::Success { output, .. } => output.data().to_vec(),
                ExecutionResult::Revert { output, .. } => output.to_vec(),
                _ => Vec::new(),
            };

            // Generate the geth trace
            let geth_trace = geth_builder.geth_traces(exec_result.gas_used(), Bytes::from(output), opts);

            // Format as JSON
            Some(serde_json::to_string_pretty(&geth_trace)
                .unwrap_or_else(|e| format!("Error serializing trace: {}", e)))
        } else {
            None
        };

        let result = ReplayResult {
            exec_result,
            state: evm_state,
            exec_time: duration,
            original_tx: target_tx,
            receipt,
            trace_data,
        };

        // Step 6: Output results
        self.output_results(&result)?;

        Ok(())
    }

    async fn retrieve_block_env(&self, block: &Block<Transaction>) -> Result<BlockEnv> {
        Ok(BlockEnv {
            number: U256::from(block.number()),
            beneficiary: block.header.beneficiary(),
            timestamp: U256::from(block.header.timestamp()),
            gas_limit: block.header.gas_limit(),
            basefee: block.header.base_fee_per_gas().unwrap_or_default(),
            difficulty: block.header.difficulty(),
            prevrandao: block.header.mix_hash(),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            }),
        })
    }

    /// Output execution results
    fn output_results(&self, result: &ReplayResult) -> Result<()> {
        // Serialize and print receipt as JSON
        let receipt_json = serde_json::to_string_pretty(&result.receipt).map_err(|e| {
            ReplayError::RunError(run::RunError::ExecutionError(format!(
                "Failed to serialize receipt: {}",
                e
            )))
        })?;
        println!("{}", receipt_json);

        // Print execution time to stderr
        eprintln!();
        eprintln!("execution time:  {:?}", result.exec_time);

        // Output trace data if available
        if let Some(ref trace) = result.trace_data {
            if let Some(ref output_file) = self.trace_args.trace_output_file {
                // Write trace to file
                std::fs::write(output_file, trace).map_err(|e| {
                    ReplayError::RunError(run::RunError::ExecutionError(format!(
                        "Failed to write trace to file: {}",
                        e
                    )))
                })?;
                eprintln!();
                eprintln!("Trace written to: {}", output_file.display());
            } else {
                // Print trace to console
                eprintln!();
                eprintln!("=== Execution Trace ===");
                eprintln!("{}", trace);
            }
        }

        // Dump state if requested
        if self.dump_args.dump {
            run::dump_state(&result.state, &self.dump_args).map_err(ReplayError::RunError)?;
        }

        Ok(())
    }
}
