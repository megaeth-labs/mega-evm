//! Shared helpers for the REX7 test suite.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction, MegaTransactionNew as _, TestExternalEnvs,
};
use revm::{
    context::{result::ExecutionResult, tx::TxEnvBuilder, TxEnv},
    handler::EvmTr,
    state::EvmState,
};

/// Transaction sender.
pub(crate) const CALLER: Address = address!("0000000000000000000000000000000000300000");
/// Contract invoked by the transaction; its code exercises the opcodes under test.
pub(crate) const CONTRACT: Address = address!("0000000000000000000000000000000000300001");
/// A second contract, used as the target of internal CALL-family frames.
pub(crate) const CALLEE: Address = address!("0000000000000000000000000000000000300002");
/// A spare empty address used as a value-transfer / SELFDESTRUCT target.
pub(crate) const EMPTY_TARGET: Address = address!("0000000000000000000000000000000000300003");

/// One ether, in wei.
pub(crate) const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// The post-transaction readings compared across specs.
pub(crate) struct Outcome {
    pub(crate) result: ExecutionResult<MegaHaltReason>,
    /// Post-tx compute-gas tracker reading (`get_usage().compute_gas`).
    pub(crate) compute_gas: u64,
    /// Post-tx data-size tracker reading (`get_usage().data_size`).
    pub(crate) data_size: u64,
    /// Post-tx KV-update tracker reading (`get_usage().kv_updates`).
    pub(crate) kv_updates: u64,
    /// Post-tx state-growth tracker reading (`get_usage().state_growth`).
    pub(crate) state_growth: u64,
    /// Receipt `gas_used` (combined compute + storage EVM gas).
    pub(crate) gas_used: u64,
    /// Post-tx detained compute gas limit — equal to the configured TX limit unless volatile
    /// access lowered it.
    pub(crate) detained_compute_gas_limit: u64,
    /// The state the transaction produced.
    pub(crate) state: EvmState,
}

impl Outcome {
    pub(crate) fn is_success(&self) -> bool {
        self.result.is_success()
    }

    /// The halt reason, or a panic with `label` when the transaction did not halt.
    pub(crate) fn halt_reason(&self, label: &str) -> &MegaHaltReason {
        match &self.result {
            ExecutionResult::Halt { reason, .. } => reason,
            other => panic!("{label}: expected a halt, got {other:?}"),
        }
    }

    /// Reads a storage slot out of the produced state, defaulting to zero when the transaction
    /// never touched it.
    pub(crate) fn storage_value(&self, address: Address, slot: U256) -> U256 {
        self.state
            .get(&address)
            .and_then(|account| account.storage.get(&slot))
            .map(|value| value.present_value())
            .unwrap_or_default()
    }
}

/// The transaction gas limit [`transact`] runs with — high enough that EVM gas is never the
/// binding constraint.
pub(crate) const DEFAULT_TX_GAS_LIMIT: u64 = 100_000_000;

/// Runs a single transaction that calls [`CONTRACT`] under `spec` with the given DB and runtime
/// limits, returning the execution result plus the post-tx tracker readings and `gas_used`.
pub(crate) fn transact(
    spec: MegaSpecId,
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
) -> Outcome {
    transact_with_gas_limit(spec, db, limits, DEFAULT_TX_GAS_LIMIT)
}

/// [`transact`] with an explicit transaction gas limit, for cases that need EVM gas itself to run
/// out.
pub(crate) fn transact_with_gas_limit(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    gas_limit: u64,
) -> Outcome {
    let mut context = MegaContext::new(&mut db, spec).with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(gas_limit).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let (usage, detained_compute_gas_limit) = {
        let additional_limit = evm.ctx_ref().additional_limit.borrow();
        (additional_limit.get_usage(), additional_limit.detained_compute_gas_limit())
    };
    let gas_used = result.result.tx_gas_used();
    Outcome {
        result: result.result,
        compute_gas: usage.compute_gas,
        data_size: usage.data_size,
        kv_updates: usage.kv_updates,
        state_growth: usage.state_growth,
        gas_used,
        detained_compute_gas_limit,
        state: result.state,
    }
}

/// Runs [`transact`] with the spec's default runtime limits.
pub(crate) fn transact_default(spec: MegaSpecId, db: MemoryDatabase) -> Outcome {
    transact(spec, db, EvmTxRuntimeLimits::from_spec(spec))
}

/// The external environment [`transact_tx`] runs with when a test does not need SALT buckets or
/// oracle storage of its own. Equivalent to the empty environment the other helpers use: every
/// bucket reports the minimum capacity and the oracle has no data.
pub(crate) fn default_envs() -> TestExternalEnvs {
    TestExternalEnvs::new()
}

/// The general entry point: an explicit transaction and an explicit external environment.
///
/// The other helpers in this module fix the transaction to a plain call into [`CONTRACT`]; the
/// shapes that need a different one — EIP-7702 authorizations, system-originated callers, direct
/// calls into a system contract — build their own `TxEnv` and come through here. `envs` is borrowed
/// so a test can read back what execution recorded into it (oracle hints, for instance).
pub(crate) fn transact_tx(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: TxEnv,
    envs: &TestExternalEnvs,
) -> Outcome {
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let (usage, detained_compute_gas_limit) = {
        let additional_limit = evm.ctx_ref().additional_limit.borrow();
        (additional_limit.get_usage(), additional_limit.detained_compute_gas_limit())
    };
    let gas_used = result.result.tx_gas_used();
    Outcome {
        result: result.result,
        compute_gas: usage.compute_gas,
        data_size: usage.data_size,
        kv_updates: usage.kv_updates,
        state_growth: usage.state_growth,
        gas_used,
        detained_compute_gas_limit,
        state: result.state,
    }
}

/// Asserts that two outcomes are indistinguishable: same execution result, same four-dimension
/// usage, same receipt `gas_used`, and the same detained compute-gas limit.
///
/// This is the precision invariant in assertion form — what a transaction that stays inside every
/// per-tx limit must produce under both accounting models.
pub(crate) fn assert_outcomes_identical(label: &str, r6: &Outcome, r7: &Outcome) {
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "{label}: execution result must be identical; REX6={:?} REX7={:?}",
        r6.result,
        r7.result
    );
    assert_eq!(
        r6.compute_gas, r7.compute_gas,
        "{label}: compute gas must be identical; REX6={} REX7={}",
        r6.compute_gas, r7.compute_gas
    );
    assert_eq!(
        r6.gas_used, r7.gas_used,
        "{label}: receipt gas_used must be identical; REX6={} REX7={}",
        r6.gas_used, r7.gas_used
    );
    assert_eq!(
        (r6.data_size, r6.kv_updates, r6.state_growth),
        (r7.data_size, r7.kv_updates, r7.state_growth),
        "{label}: the non-compute dimensions must be identical",
    );
    assert_eq!(
        r6.detained_compute_gas_limit, r7.detained_compute_gas_limit,
        "{label}: the detained compute-gas limit must be identical; REX6={} REX7={}",
        r6.detained_compute_gas_limit, r7.detained_compute_gas_limit
    );
}

/// [`transact`] with every SALT bucket reporting `bucket_capacity`.
///
/// The SALT-scaled storage-gas charges (`SSTORE` set, new account, contract creation) are
/// `base × (capacity / MIN_BUCKET_SIZE − 1)`, so only a capacity above
/// [`mega_evm::MIN_BUCKET_SIZE`] makes them non-zero and exercises the paths that have to
/// exclude them from the compute-gas window.
pub(crate) fn transact_with_bucket_capacity(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    bucket_capacity: u64,
) -> Outcome {
    let envs = TestExternalEnvs::default().with_default_bucket_capacity(bucket_capacity);
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let (usage, detained_compute_gas_limit) = {
        let additional_limit = evm.ctx_ref().additional_limit.borrow();
        (additional_limit.get_usage(), additional_limit.detained_compute_gas_limit())
    };
    let gas_used = result.result.tx_gas_used();
    Outcome {
        result: result.result,
        compute_gas: usage.compute_gas,
        data_size: usage.data_size,
        kv_updates: usage.kv_updates,
        state_growth: usage.state_growth,
        gas_used,
        detained_compute_gas_limit,
        state: result.state,
    }
}
