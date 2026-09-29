//! A block on the Satin engine.

use alloy_consensus::transaction::Recovered;
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::U256;
use mega_evm::{
    alloy_evm::{block::BlockExecutor, EvmEnv},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{result::ExecutionResult, BlockEnv, CfgEnv},
        database::State,
        inspector::NoOpInspector,
        primitives::eip4844,
    },
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaSpecId,
    MegaTxEnvelope,
};

use super::{
    exec::{ExecStatus, ExecutedBlock, ReceiptData, TxResult},
    inputs::{HeaderFields, TxInput},
    state::{BlockState, SatinDb},
};
use crate::common::{
    decode_revert_reason, format_halt_reason, satin_schedule, EvmeError, EvmeExternalEnvs,
    LimitsOverride, Result, SatinReport,
};

/// Executes the block `header` describes, with `transactions`, on Satin over `state`.
///
/// The block runs as a validator runs it: under the schedule [`satin_schedule`] gives — the
/// chain's own when it runs Satin at the block, otherwise the engine's fallback, a counterfactual
/// — held to the protocol limits that schedule carries, or `limits_override` over them, with no
/// building policy, and with the
/// block's gas limit from its header. A transaction the engine refuses is reported as refused and
/// left out, and the block goes on.
pub(super) fn execute(
    chain_id: u64,
    header: &HeaderFields,
    transactions: &[TxInput],
    bucket_capacities: &[(u32, u64)],
    limits_override: Option<&LimitsOverride>,
    state: &mut BlockState,
) -> Result<ExecutedBlock> {
    let hardforks = satin_schedule(chain_id, header.timestamp, limits_override)?;
    let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
    cfg.chain_id = chain_id;
    let mut block_env = BlockEnv {
        number: U256::from(header.number),
        beneficiary: header.beneficiary,
        timestamp: U256::from(header.timestamp),
        gas_limit: header.gas_limit,
        basefee: header.base_fee,
        difficulty: header.difficulty,
        prevrandao: Some(header.mix_hash),
        blob_excess_gas_and_price: None,
        slot_num: 0,
    };
    block_env.set_blob_excess_gas_and_price(
        header.excess_blob_gas,
        eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
    );
    let mut envs = EvmeExternalEnvs::new();
    for &(bucket, capacity) in bucket_capacities {
        envs = envs.with_bucket_capacity(bucket, capacity);
    }
    let factory = MegaBlockExecutorFactory::new(
        OpAlloyReceiptBuilder::default(),
        &hardforks,
        MegaEvmFactory::new().with_schedule(hardforks.clone()).with_external_env_factory(envs),
    );
    let block_ctx = MegaBlockExecutionCtx::new(
        header.parent_hash,
        header.parent_beacon_block_root,
        header.extra_data.clone(),
        BlockLimits::default(),
    );

    let result = {
        let mut db = State::builder().with_database(SatinDb(state)).with_bundle_update().build();
        let mut executor = factory.create_executor_with_trusted_inspector(
            &mut db,
            EvmEnv::new(cfg, block_env),
            block_ctx,
            NoOpInspector,
        );
        let mut run = || -> Result<ExecutedBlock> {
            executor
                .apply_pre_execution_changes()
                .map_err(|e| EvmeError::Other(format!("pre-block changes: {e}")))?;
            let mut results = Vec::with_capacity(transactions.len());
            for tx in transactions {
                let envelope = MegaTxEnvelope::decode_2718(&mut tx.envelope.as_ref())
                    .map_err(|e| EvmeError::Other(format!("decoding {}: {e}", tx.hash)))?;
                let recovered = Recovered::new_unchecked(envelope, tx.sender);
                let outcome = match executor.run_transaction(recovered.as_recovered_ref()) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        results.push(TxResult::Refused { reason: e.to_string() });
                        continue;
                    }
                };
                let satin = SatinReport::of(&outcome.inner);
                let (status, reason) = match &outcome.inner.result {
                    ExecutionResult::Success { .. } => (ExecStatus::Success, None),
                    ExecutionResult::Revert { output, .. } => {
                        (ExecStatus::Revert, Some(decode_revert_reason(output)))
                    }
                    ExecutionResult::Halt { reason, .. } => {
                        (ExecStatus::Halt, Some(format_halt_reason(reason)))
                    }
                };
                if let Err(e) = executor.commit_transaction_outcome(outcome) {
                    results.push(TxResult::Refused { reason: e.to_string() });
                    continue;
                }
                let receipt =
                    executor.receipts().last().expect("a committed transaction has a receipt");
                results.push(TxResult::Included {
                    status,
                    reason,
                    receipt: ReceiptData {
                        encoded: receipt.encoded_2718().into(),
                        success: receipt.status(),
                        cumulative_gas_used: receipt.cumulative_gas_used(),
                        logs: receipt.logs().to_vec(),
                    },
                    satin: Some(satin),
                });
            }
            Ok(ExecutedBlock { transactions: results })
        };
        run()
    };
    // A read the state could not serve fails the block, whatever the engine made of it.
    if let Some(miss) = &state.miss {
        return Err(EvmeError::Other(format!("block {}: {miss}", header.number)));
    }
    result
}
