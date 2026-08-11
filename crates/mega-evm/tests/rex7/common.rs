//! Shared helpers for the REX7 test suite.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction, MegaTransactionNew as _, TestExternalEnvs,
};
use revm::{
    context::{result::ExecutionResult, tx::TxEnvBuilder},
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
    /// The state the transaction produced.
    pub(crate) state: EvmState,
}

impl Outcome {
    pub(crate) fn is_success(&self) -> bool {
        self.result.is_success()
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

/// Runs a single transaction that calls [`CONTRACT`] under `spec` with the given DB and runtime
/// limits, returning the execution result plus the post-tx tracker readings and `gas_used`.
pub(crate) fn transact(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
) -> Outcome {
    let mut context = MegaContext::new(&mut db, spec).with_tx_runtime_limits(limits);
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
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    let gas_used = result.result.tx_gas_used();
    Outcome {
        result: result.result,
        compute_gas: usage.compute_gas,
        data_size: usage.data_size,
        kv_updates: usage.kv_updates,
        state_growth: usage.state_growth,
        gas_used,
        state: result.state,
    }
}

/// Runs [`transact`] with the spec's default runtime limits.
pub(crate) fn transact_default(spec: MegaSpecId, db: MemoryDatabase) -> Outcome {
    transact(spec, db, EvmTxRuntimeLimits::from_spec(spec))
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
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    let gas_used = result.result.tx_gas_used();
    Outcome {
        result: result.result,
        compute_gas: usage.compute_gas,
        data_size: usage.data_size,
        kv_updates: usage.kv_updates,
        state_growth: usage.state_growth,
        gas_used,
        state: result.state,
    }
}
