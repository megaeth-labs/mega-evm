//! Shared setup of the Satin tests.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
#[cfg(debug_assertions)]
use mega_evm::test_utils::{LedgerDivergence, LedgerFigure};
use mega_evm::{
    active_satin_prices,
    test_utils::{note_price_guard, op_transaction, zero_fee_l1_block_info, BytecodeBuilder},
    EvmTxRuntimeLimits, LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionOutcome,
};
#[cfg(debug_assertions)]
use revm::interpreter::{PoolAmounts, PoolId};
use revm::{
    bytecode::opcode::{PUSH0, RETURN},
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
    let usage = evm.ctx().limit_usage();
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

/// Runs `tx` on a fresh Satin EVM over `db` held to `limits` and returns its outcome.
pub(crate) fn execute_under<DB: alloy_evm::Database>(
    db: DB,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(tx)
        .expect("the transaction is valid")
}

/// 98% of `remaining`, the share a child frame is given.
pub(crate) fn share(remaining: u64) -> u64 {
    (u128::from(remaining) * u128::from(mega_evm::FRAME_DATA_SHARE_NUMERATOR) /
        u128::from(mega_evm::FRAME_DATA_SHARE_DENOMINATOR)) as u64
}

/// The regular gas a call or creation forwards of the `remaining` its caller holds at the forward
/// when it asks for all of it: all but the caller's 2 per cent, the division truncating.
pub(crate) const fn forward_cap(remaining: u64) -> u64 {
    remaining - remaining * 2 / 100
}

/// The most regular gas a caller can hold at the forward of a call or creation that asks for all of
/// it and keep only `kept` once [`forward_cap`] is taken: a caller keeps a fiftieth of what it
/// holds, rounded down.
pub(crate) const fn holding_to_keep(kept: u64) -> u64 {
    50 * kept + 49
}

/// The reservoir a child frame is given of the `remaining` its caller has left: all but the
/// caller's 2 per cent, the division truncating, so a caller keeps nothing of a reservoir below 50.
pub(crate) const fn reservoir_share(remaining: u64) -> u64 {
    remaining - remaining * 2 / 100
}

/// The smallest reservoir a caller can hold that hands a child exactly `share`
/// ([`reservoir_share`] takes every value, rising by one or by none at each step).
pub(crate) fn reservoir_giving(share: u64) -> u64 {
    let mut remaining = share + share / 50;
    while reservoir_share(remaining) > share {
        remaining -= 1;
    }
    while reservoir_share(remaining) < share {
        remaining += 1;
    }
    while remaining > 0 && reservoir_share(remaining - 1) == share {
        remaining -= 1;
    }
    remaining
}

/// `tx` as a user's deposit: no fee, and a source hash a user's transaction could carry.
pub(crate) fn deposit(mut tx: MegaTransaction) -> MegaTransaction {
    tx.0.deposit.source_hash = B256::repeat_byte(0x11);
    tx.0.base.gas_price = 0;
    tx
}

/// Init code that returns `len` bytes of zeros as the deployed code.
pub(crate) fn deploying(len: u64) -> Bytes {
    BytecodeBuilder::default().push_number(len).append_many([PUSH0, RETURN]).build()
}

/// Init code that returns `size` zero bytes as the deployed contract.
pub(crate) fn constructor_returning(size: u64) -> Bytes {
    BytecodeBuilder::default().push_number(size).push_number(0_u8).append(RETURN).build()
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

/// Room for the state gas of several new accounts, including in a frame that resumes after a
/// child burned the 98/100 it was forwarded: at least 50,000,000, and 64 times what a value call
/// to a new account costs its caller at the byte prices in effect, more than the 50 times the
/// fiftieth it keeps needs.
pub(crate) fn value_call_room() -> u64 {
    (64 * (account_state_gas() + history(4 * mega_evm::WRITE_RECORD_SIZE) + 50_000)).max(50_000_000)
}

/// `usage` with `growth` new accounts and new slots kept: its state growth.
pub(crate) const fn grown(usage: LimitUsage, growth: u64) -> LimitUsage {
    LimitUsage { state_growth: growth, ..usage }
}

/// What a transaction with an empty calldata keeps: its body, `records` write records and the
/// transfer logs of `moves` value movements.
pub(crate) const fn records_and_moves(records: u64, moves: u64) -> LimitUsage {
    LimitUsage {
        data_size: mega_evm::TX_BODY_SIZE +
            records * mega_evm::WRITE_RECORD_SIZE +
            moves * mega_evm::TRANSFER_LOG_SIZE,
        write_records: records,
        state_growth: 0,
    }
}

/// The history gas the body of a transaction carrying `calldata_len` bytes of calldata, and no
/// access list or authorization, pays at the byte prices in effect, by hand: `TX_BODY_SIZE`, 310
/// bytes, and one byte per byte of calldata.
pub(crate) fn body_history(calldata_len: u64) -> u64 {
    history(mega_evm::TX_BODY_SIZE + calldata_len)
}

/// The pools the transaction's own frame starts with under `limits` when `tx` counts nothing
/// before it but its body: what each limit leaves once the body is counted, the data size under the
/// frame cap.
#[cfg(debug_assertions)]
pub(crate) fn first_frame_pools(limits: &EvmTxRuntimeLimits, tx: &MegaTransaction) -> PoolAmounts {
    let data_size = limits.tx_data_size_limit.saturating_sub(mega_evm::transaction_body_bytes(tx));
    PoolAmounts::ZERO
        .with(PoolId::new(0), data_size.min(limits.frame_data_size_limit))
        .with(PoolId::new(1), limits.tx_kv_update_limit)
        .with(PoolId::new(2), limits.tx_state_growth_limit)
}

/// The share of each of `caller`'s pools a frame it starts begins with: 98/100 of each, rounded
/// down.
#[cfg(debug_assertions)]
pub(crate) fn frame_shares(caller: PoolAmounts) -> PoolAmounts {
    let share = |amount: u64| {
        let share = u128::from(amount) * u128::from(mega_evm::FRAME_DATA_SHARE_NUMERATOR) /
            u128::from(mega_evm::FRAME_DATA_SHARE_DENOMINATOR);
        u64::try_from(share).expect("a share is less than the pool")
    };
    PoolId::ALL.into_iter().fold(caller, |shares, pool| shares.with(pool, share(caller.get(pool))))
}

/// What a debug build's start ledger records where an inspector answered start `start`, at
/// `depth`, on gas that carries the shares its input was forwarded, `shares`, as the engine's
/// `untouched_call_gas` and `untouched_create_gas` build it: an answer with pools the inspector
/// set, which the owner accepted as an API removal. The ledger settles an inspector's answer at
/// unlimited pools, so it parts from the fork on the answer's own pools wherever a share is
/// limited, and on its caller's once it settled, wherever the caller, which the fork gives back
/// the shares and so leaves with what it held before the start, `caller`, is limited.
#[cfg(debug_assertions)]
pub(crate) fn answered_on_its_shares(
    start: u32,
    depth: usize,
    caller: PoolAmounts,
    shares: PoolAmounts,
) -> Vec<LedgerDivergence> {
    let parts = |amounts: PoolAmounts, figure: fn(PoolId) -> LedgerFigure| {
        PoolId::ALL.into_iter().filter(move |pool| amounts.get(*pool) != u64::MAX).map(
            move |pool| LedgerDivergence {
                start,
                depth,
                figure: figure(pool),
                ledger: u64::MAX.into(),
                fork: amounts.get(pool).into(),
            },
        )
    };
    parts(shares, LedgerFigure::FrameRemaining)
        .chain(parts(caller, LedgerFigure::CallerRemaining))
        .collect()
}
