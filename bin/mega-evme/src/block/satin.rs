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
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory,
    MegaHardforkConfig, MegaHardforks, MegaSpecId, MegaTxEnvelope, ProtocolLimits,
};

use super::{
    exec::{ExecStatus, ExecutedBlock, ReceiptData, TxResult},
    inputs::{HeaderFields, TxInput},
    state::{BlockState, SatinDb},
};
use crate::common::{
    decode_revert_reason, format_halt_reason, satin_limits, satin_schedule, EvmeError,
    EvmeExternalEnvs, LimitsOverride, Result, SatinReport,
};

/// Executes the block `header` describes, with `transactions`, on Satin over `state`.
///
/// The block runs as a validator runs it: under the schedule [`satin_schedule`] gives — the
/// chain's own when it runs Satin at the block, otherwise the engine's fallback, a counterfactual
/// — held to Satin's protocol limits, or `limits_override` over them, a counterfactual, with no
/// building policy, and with the block's gas limit from its header. Whether the block admits only
/// deposits is worked out from that schedule and `parent_timestamp`, the parent block's timestamp
/// ([`admits_only_deposits`]). A transaction the engine refuses is reported as refused and left
/// out, and the block goes on.
pub(super) fn execute(
    chain_id: u64,
    header: &HeaderFields,
    parent_timestamp: Option<u64>,
    transactions: &[TxInput],
    bucket_capacities: &[(u32, u64)],
    limits_override: Option<&LimitsOverride>,
    state: &mut BlockState,
) -> Result<ExecutedBlock> {
    let hardforks = satin_schedule(chain_id, header.timestamp)?;
    let limits_override: Option<ProtocolLimits> =
        limits_override.map(|limits| satin_limits(Some(limits))).transpose()?;
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
        MegaEvmFactory::new().with_external_env_factory(envs),
    );
    let only_deposits =
        admits_only_deposits(&hardforks, header.number, parent_timestamp, header.timestamp)?;
    // Without the parent's timestamp the block admits every transaction only where no fork the
    // schedule activates after genesis is active at it, so Satin does not activate in it either.
    let activates_satin = parent_timestamp
        .is_some_and(|parent| hardforks.is_satin_activation_block(parent, header.timestamp));
    let block_ctx = MegaBlockExecutionCtx::new(
        header.parent_hash,
        header.parent_beacon_block_root,
        header.extra_data.clone(),
        BlockLimits::default(),
    )
    .with_no_user_tx_activation_block(only_deposits)
    .with_satin_activation_block(activates_satin);

    let result = {
        let mut db = State::builder().with_database(SatinDb(state)).with_bundle_update().build();
        let mut executor = factory.create_executor_with_trusted_inspector(
            &mut db,
            EvmEnv::new(cfg, block_env),
            block_ctx,
            NoOpInspector,
        );
        if let Some(limits) = limits_override {
            executor = executor.with_protocol_limits(limits);
        }
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
            Ok(ExecutedBlock { transactions: results, limits_override })
        };
        run()
    };
    // A read the state could not serve fails the block, whatever the engine made of it.
    if let Some(miss) = &state.miss {
        return Err(EvmeError::Other(format!("block {}: {miss}", header.number)));
    }
    result
}

/// Whether running the block of chain `chain_id` at `timestamp` on Satin, under the schedule
/// [`satin_schedule`] gives it, needs its parent's timestamp: a fork that schedule activates after
/// timestamp 0 is active at the block, so whether the fork activates in this block depends on when
/// its parent came ([`admits_only_deposits`]).
pub(super) fn needs_parent_timestamp(chain_id: u64, timestamp: u64) -> Result<bool> {
    let hardforks = satin_schedule(chain_id, timestamp)?;
    Ok(hardforks.admits_only_deposits(0, timestamp))
}

/// Whether block `number`, at `timestamp`, admits only deposits under `hardforks`, as a node works
/// it out: a fork the schedule activates is active at the block's timestamp and not at its
/// parent's ([`MegaHardforks::admits_only_deposits`]).
///
/// Without the parent's timestamp the answer is known only where no parent could change it: no
/// fork the schedule activates after timestamp 0 is active at the block, so the block admits every
/// transaction whatever its parent's timestamp. Any other block is refused, since the tool cannot
/// tell whether a fork activates in it, and the refusal says how to recover: one replay with an
/// RPC endpoint writes the timestamp to the block cache.
fn admits_only_deposits(
    hardforks: &MegaHardforkConfig,
    number: u64,
    parent_timestamp: Option<u64>,
    timestamp: u64,
) -> Result<bool> {
    match parent_timestamp {
        Some(parent_timestamp) => Ok(hardforks.admits_only_deposits(parent_timestamp, timestamp)),
        None if !hardforks.admits_only_deposits(0, timestamp) => Ok(false),
        None => Err(EvmeError::Other(format!(
            "block {number}: a fork its schedule activates is active at its timestamp, and \
             whether the fork activates in this block depends on its parent's timestamp, which \
             the block cache does not hold; replay it once with '--rpc', which fetches the \
             timestamp and writes it to the block cache, and it then replays offline"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use mega_evm::{
        all_activated_hardforks, alloy_hardforks::ForkCondition, MegaHardfork, MegaHardforkConfig,
    };

    use super::admits_only_deposits;

    const ACTIVATION: u64 = 1_800_000_000;

    fn satin_at(timestamp: u64) -> MegaHardforkConfig {
        MegaHardforkConfig::default().with(MegaHardfork::Satin, ForkCondition::Timestamp(timestamp))
    }

    /// [S1.8] [S21.12] [S1] With the parent's timestamp, the block admits only deposits exactly
    /// where a fork activates in it: active at the block, not at the parent.
    ///
    /// Expected values: independent (the schedule's own timestamps and the rule).
    #[test]
    fn test_the_parent_timestamp_decides_the_activation_block() {
        let schedule = satin_at(ACTIVATION);
        assert!(admits_only_deposits(&schedule, 7, Some(ACTIVATION - 1), ACTIVATION).unwrap());
        assert!(admits_only_deposits(&schedule, 7, Some(ACTIVATION - 9), ACTIVATION + 1).unwrap());
        assert!(!admits_only_deposits(&schedule, 7, Some(ACTIVATION), ACTIVATION + 1).unwrap());
        assert!(!admits_only_deposits(&schedule, 7, Some(0), ACTIVATION - 1).unwrap());
    }

    /// [S1.8] [S1] Without it, a block no parent could make an activation block admits every
    /// transaction: every fork active from genesis, or none active yet. Any other block is
    /// refused, naming the block and how to recover.
    ///
    /// Expected values: independent (the schedule's own timestamps and the rule).
    #[test]
    fn test_without_the_parent_timestamp_only_an_undecidable_block_is_refused() {
        assert!(!admits_only_deposits(&all_activated_hardforks(), 7, None, ACTIVATION).unwrap());
        assert!(!admits_only_deposits(&satin_at(0), 7, None, ACTIVATION).unwrap());
        assert!(!admits_only_deposits(&satin_at(ACTIVATION), 7, None, ACTIVATION - 1).unwrap());
        for timestamp in [ACTIVATION, ACTIVATION + 1] {
            let error = admits_only_deposits(&satin_at(ACTIVATION), 7, None, timestamp)
                .expect_err("a fork activated at or before the block, after genesis")
                .to_string();
            assert!(error.contains("block 7: "), "{error}");
            assert!(error.contains("parent's timestamp"), "{error}");
            assert!(error.contains("replay it once with '--rpc'"), "the way back: {error}");
        }
    }
}
