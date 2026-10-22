//! Shared setup of the Satin tests.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{op_transaction, zero_fee_l1_block_info},
    LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    context::{result::ResultAndState, BlockEnv, TxEnv},
    Database,
};

/// A block with room for any transaction the tests run.
pub(crate) fn block() -> BlockEnv {
    BlockEnv { number: U256::from(1), gas_limit: 10_000_000_000, ..Default::default() }
}

/// A Satin context over `db` with zero L1 fees.
pub(crate) fn context<DB: Database>(db: DB) -> MegaContext<DB> {
    MegaContext::new(db, MegaSpecId::SATIN).with_block(block()).with_chain(zero_fee_l1_block_info())
}

/// A call from `caller` to `to` with zero gas price.
pub(crate) fn call(caller: Address, to: Address, value: U256, gas_limit: u64) -> MegaTransaction {
    tx(caller, TxKind::Call(to), Bytes::new(), value, gas_limit)
}

/// A call from `caller` to `to` carrying `data`.
pub(crate) fn call_with_data(
    caller: Address,
    to: Address,
    data: Bytes,
    gas_limit: u64,
) -> MegaTransaction {
    tx(caller, TxKind::Call(to), data, U256::ZERO, gas_limit)
}

/// A creation from `caller` running `init_code`.
pub(crate) fn create(caller: Address, init_code: Bytes, gas_limit: u64) -> MegaTransaction {
    tx(caller, TxKind::Create, init_code, U256::ZERO, gas_limit)
}

fn tx(caller: Address, kind: TxKind, data: Bytes, value: U256, gas_limit: u64) -> MegaTransaction {
    OpTx(op_transaction(TxEnv { caller, kind, data, value, gas_limit, ..Default::default() }))
}

/// Runs `tx` on a fresh Satin EVM over `db` and returns its result and what the common execution
/// layer counted.
pub(crate) fn run<DB: alloy_evm::Database>(
    db: DB,
    tx: MegaTransaction,
) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    let mut evm = MegaEvm::new(context(db));
    let result = evm.transact_raw(tx).expect("the transaction is valid");
    let usage = evm.ctx().additional_limit().usage();
    (result, usage)
}
