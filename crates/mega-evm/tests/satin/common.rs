//! Shared setup of the Satin tests.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use mega_evm::{
    active_satin_prices,
    test_utils::{op_transaction, zero_fee_l1_block_info},
    LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionOutcome,
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

/// A call from `caller` to `to` carrying `value` and `data`.
pub(crate) fn call_with_value_and_data(
    caller: Address,
    to: Address,
    value: U256,
    data: Bytes,
    gas_limit: u64,
) -> MegaTransaction {
    tx(caller, TxKind::Call(to), data, value, gas_limit)
}

/// A creation from `caller` running `init_code`.
pub(crate) fn create(caller: Address, init_code: Bytes, gas_limit: u64) -> MegaTransaction {
    tx(caller, TxKind::Create, init_code, U256::ZERO, gas_limit)
}

fn tx(caller: Address, kind: TxKind, data: Bytes, value: U256, gas_limit: u64) -> MegaTransaction {
    OpTx(op_transaction(TxEnv { caller, kind, data, value, gas_limit, ..Default::default() }))
}

/// A type-4 call from `caller` to `to` carrying `value`, with one authorization per
/// `(authority, nonce)` delegating the authority to `delegate`, valid on any chain.
pub(crate) fn authorizing_call(
    caller: Address,
    to: Address,
    value: U256,
    gas_limit: u64,
    delegate: Address,
    authorizations: &[(Address, u64)],
) -> MegaTransaction {
    use revm::{
        context::transaction::TransactionType,
        context_interface::{
            either::Either,
            transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
        },
    };
    let authorization_list = authorizations
        .iter()
        .map(|(authority, nonce)| {
            Either::Right(RecoveredAuthorization::new_unchecked(
                Authorization { chain_id: U256::ZERO, address: delegate, nonce: *nonce },
                RecoveredAuthority::Valid(*authority),
            ))
        })
        .collect();
    OpTx(op_transaction(TxEnv {
        tx_type: TransactionType::Eip7702 as u8,
        caller,
        kind: TxKind::Call(to),
        value,
        gas_limit,
        gas_priority_fee: Some(0),
        authorization_list,
        ..Default::default()
    }))
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

/// Runs `tx` on a fresh Satin EVM over `db` and returns its outcome: the result and state, the
/// gas by ledger and what the common execution layer counted.
pub(crate) fn execute<DB: alloy_evm::Database>(
    db: DB,
    tx: MegaTransaction,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db)).execute_transaction(tx).expect("the transaction is valid")
}

/// Whether this process prices bytes at something other than the constants the spec fixes.
///
/// Only a measurement build can arrange that — the `satin-price-override` feature, through
/// `install_satin_prices` or the `MEGA_SATIN_CPSB` / `MEGA_SATIN_CPHB` variables — and it moves
/// every state-gas number. A test that asserts one of those numbers returns early instead of
/// failing on a price the developer asked for.
///
/// The notice goes straight to stderr and only once: the harness captures the print macros of a
/// test that passes, so a message written with them would never be read.
pub(crate) fn runs_at_measurement_prices() -> bool {
    use std::io::Write;

    if active_satin_prices().is_constants() {
        return false;
    }
    static NOTICE: std::sync::Once = std::sync::Once::new();
    NOTICE.call_once(|| {
        let _ = writeln!(
            std::io::stderr(),
            "note: skipping the tests that assert the spec's byte prices, because \
             MEGA_SATIN_CPSB or MEGA_SATIN_CPHB fixed other ones; unset them to run those tests"
        );
    });
    true
}
