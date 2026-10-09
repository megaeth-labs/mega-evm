//! Shared setup of the Satin tests.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use mega_evm::{
    active_satin_prices,
    test_utils::{note_price_guard, op_transaction, zero_fee_l1_block_info},
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
/// Each test that returns early leaves a note (`note_price_guard`), which the byte-price
/// grid counts.
pub(crate) fn runs_at_measurement_prices() -> bool {
    if active_satin_prices().is_constants() {
        return false;
    }
    note_price_guard("MEGA_SATIN_CPSB or MEGA_SATIN_CPHB fixed prices other than the spec's");
    true
}

/// Whether a state byte costs nothing at the prices in effect: every state-gas entry of the
/// schedule is zero.
///
/// Only a measurement build arranges that, with `MEGA_SATIN_CPSB` at 0 or at a price every entry
/// rounds to nothing. A test whose scenario is state gas — a limit to cross with it, a bucket to
/// scale it, a charge to give back — has nothing to run then, and returns early, with a note like
/// [`runs_at_measurement_prices`]'s.
pub(crate) fn state_is_free() -> bool {
    let params = mega_evm::satin_gas_params();
    if !mega_evm::STATE_GAS_REPRICED.iter().all(|&(id, _)| params.get(id()) == 0) {
        return false;
    }
    note_price_guard("MEGA_SATIN_CPSB prices a state byte at nothing");
    true
}

/// Whether a history byte costs nothing at the prices in effect.
///
/// Only a measurement build arranges that, with `MEGA_SATIN_CPHB` at 0. A test whose scenario is
/// history gas has nothing to run then, and returns early, with a notice like
/// [`state_is_free`]'s.
pub(crate) fn history_is_free() -> bool {
    if active_satin_prices().cphb.milli_gas() != 0 {
        return false;
    }
    note_price_guard("MEGA_SATIN_CPHB prices a history byte at nothing");
    true
}

/// Whether a history byte costs a fraction of a gas at the prices in effect.
///
/// Only a measurement build arranges that, as the grid's 0.001 does. Each history charge is then
/// rounded to the nearest gas on its own, so a small charge — a body, a record — can cost nothing,
/// and the history of bytes charged apart is not the history of their sum. No test is held to
/// its history figures there: a test whose figures add history over several charges, or need a
/// record to cost something, returns early, or leaves those figures out, with a note like
/// [`history_is_free`]'s.
pub(crate) fn history_rounds() -> bool {
    if active_satin_prices().cphb.milli_gas().is_multiple_of(1_000) {
        return false;
    }
    note_price_guard("MEGA_SATIN_CPHB prices a history byte at a fraction of a gas");
    true
}

/// The state gas one fresh storage slot costs at the byte prices in effect, in the minimum
/// bucket.
pub(crate) fn slot_state_gas() -> u64 {
    mega_evm::satin_gas_params().get(revm::context_interface::cfg::GasId::sstore_set_state_gas())
}

/// The state gas one new account costs at the byte prices in effect, in the minimum bucket.
pub(crate) fn account_state_gas() -> u64 {
    mega_evm::satin_gas_params().get(revm::context_interface::cfg::GasId::new_account_state_gas())
}

/// The history gas `bytes` bytes cost at the byte prices in effect, by hand rather than through
/// the engine's pricing: the bytes times `COST_PER_HISTORY_BYTE`, 88 gas, at the spec's price.
///
/// A measurement build may price a history byte with a fraction of a gas, kept in thousandths of
/// a gas; the charge for the bytes is then rounded to the nearest gas, halves up.
pub(crate) fn history(bytes: u64) -> u64 {
    use mega_evm::constants::COST_PER_HISTORY_BYTE;
    let milli_gas = mega_evm::active_satin_prices().cphb.milli_gas();
    if milli_gas == COST_PER_HISTORY_BYTE * 1_000 {
        return bytes * COST_PER_HISTORY_BYTE;
    }
    let milli_gas = u128::from(bytes) * u128::from(milli_gas);
    u64::try_from((milli_gas + 500) / 1_000).expect("the history of the bytes fits a u64")
}

/// The history gas the body of a transaction carrying `calldata_len` bytes of calldata, and no
/// access list or authorization, pays at the byte prices in effect, by hand: `TX_BODY_SIZE`, 310
/// bytes, and one byte per byte of calldata.
pub(crate) fn body_history(calldata_len: u64) -> u64 {
    history(mega_evm::TX_BODY_SIZE + calldata_len)
}
