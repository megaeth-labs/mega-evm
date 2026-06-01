//! Shared helpers for the REX6 gas-metering-order test suite.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction,
};
use revm::{
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    handler::EvmTr,
};

/// Transaction sender.
pub(crate) const CALLER: Address = address!("0000000000000000000000000000000000200000");
/// Contract invoked by the transaction; its code exercises the opcode under test.
pub(crate) const CONTRACT: Address = address!("0000000000000000000000000000000000200001");
/// A spare empty address used as a CALL / SELFDESTRUCT target.
pub(crate) const EMPTY_TARGET: Address = address!("0000000000000000000000000000000000200002");

/// The post-transaction readings compared across specs.
pub(crate) struct Outcome {
    pub(crate) result: ExecutionResult<MegaHaltReason>,
    /// Post-tx compute-gas tracker reading (`get_usage().compute_gas`).
    pub(crate) compute_gas: u64,
    /// Receipt `gas_used` (combined compute + storage EVM gas).
    pub(crate) gas_used: u64,
}

impl Outcome {
    pub(crate) fn is_success(&self) -> bool {
        self.result.is_success()
    }
}

/// Runs a single transaction that calls [`CONTRACT`] under `spec` with the given DB and runtime
/// limits, returning the execution result plus the post-tx compute-gas reading and `gas_used`.
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
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    let gas_used = result.result.gas_used();
    Outcome { result: result.result, compute_gas, gas_used }
}

/// Runs [`transact`] with the spec's default runtime limits.
pub(crate) fn transact_default(spec: MegaSpecId, db: MemoryDatabase) -> Outcome {
    transact(spec, db, EvmTxRuntimeLimits::from_spec(spec))
}
