//! The history allowance a value-transferring call grants its callee, and the three ways it must
//! not leak.
//!
//! EVM's own `CALL_STIPEND` buys the recipient of a transfer enough computation to notice it.
//! On a chain that prices the bytes a log appends it buys no log at all, so a `receive()` hook
//! that emits an event would be unreachable through Solidity's `transfer()`. The allowance is
//! that stipend's counterpart on the history ledger: 160 bytes — one three-topic event carrying
//! one word — at the cost per history byte, and nothing more.
//!
//! What makes it safe is that it is not gas. It never enters the frame's `Gas`, so no settlement
//! can hand it back; it pays history charges and only those, so it cannot buy computation or a
//! write record; and it belongs to the frame it was granted to, so a frame answered without
//! running neither takes one nor leaves one behind. The three leak paths below are the three
//! places where a frame's allowance could escape the frame: an interceptor answering in its
//! place, a transaction-level stop unwinding it, and the frame returning to its caller.
//!
//! The transfer log a value call leaves is not the callee's event and draws nothing from the
//! allowance: it costs no history at all, so the event still has the whole allowance to itself.
//! The events below are counted apart from it.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::COST_PER_HISTORY_BYTE,
    storage_call_stipend,
    system::{IMegaAccessControl, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE},
    test_utils::{is_transfer_log, transfer_log, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitKind, MegaEvm, MegaTransaction, MegaTransactionOutcome,
    STORAGE_CALL_STIPEND_BYTES, TRANSFER_LOG_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::bytecode::opcode::{
    ADDRESS, CALL, CALLCODE, CALLDATASIZE, DELEGATECALL, DUP1, GAS, ISZERO, JUMPDEST, JUMPI, LOG3,
    POP, PUSH0, PUSH1, PUSH2, SSTORE, STATICCALL, STOP, SUB, SWAP1,
};

use crate::common::{call, context, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000500000");
const SENDER: Address = address!("0000000000000000000000000000000000500001");
const RECEIVER: Address = address!("0000000000000000000000000000000000500002");

const GAS_LIMIT: u64 = 30_000_000;

/// What Solidity's `transfer()` forwards: the EVM's own `CALL_STIPEND` and not one gas more.
const TRANSFER_GAS: u64 = 2_300;

/// The history a transaction's body costs, which every transaction here pays.
const fn body() -> u64 {
    TX_BODY_SIZE * COST_PER_HISTORY_BYTE
}

/// The history one write record costs.
const fn record() -> u64 {
    WRITE_RECORD_SIZE * COST_PER_HISTORY_BYTE
}

/// A three-topic event over one word of memory, the event the allowance is sized for.
fn event() -> BytecodeBuilder {
    BytecodeBuilder::default()
        .push_number(1u64)
        .push_number(2u64)
        .push_number(3u64)
        .push_number(32u64)
        .push_number(0u64)
        .append(LOG3)
}

/// `scheme(gas, target, value, 0, 0, 0, 0); POP`, followed by whatever comes next.
///
/// `DELEGATECALL` and `STATICCALL` take no value word at all, so the stack they are given is one
/// shorter.
fn call_with(scheme: u8, target: Address, value: u64, gas: u64) -> BytecodeBuilder {
    let empty_args = BytecodeBuilder::default().append_many([PUSH0, PUSH0, PUSH0, PUSH0]);
    let code = if matches!(scheme, DELEGATECALL | STATICCALL) {
        empty_args
    } else {
        empty_args.push_number(value)
    };
    code.push_address(target).push_number(gas).append(scheme).append(POP)
}

fn db(sender_code: Bytes, receiver_code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(SENDER, U256::from(10u64.pow(9)))
        .account_code(SENDER, sender_code)
        .account_balance(RECEIVER, U256::from(1))
        .account_code(RECEIVER, receiver_code)
}

/// The events the contracts emitted: the logs without the transfer logs of the value moved.
fn events(outcome: &MegaTransactionOutcome) -> Vec<&alloy_primitives::Log> {
    outcome.result.logs().iter().filter(|log| !is_transfer_log(log)).collect()
}

fn run(db: MemoryDatabase, tx: MegaTransaction) -> MegaTransactionOutcome {
    MegaEvm::new(context(db)).execute_transaction(tx).expect("the transaction is valid")
}

fn run_limited(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: MegaTransaction,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(tx)
        .expect("the transaction is valid")
}

/* ---------- what the allowance is, and what it buys ---------- */

/// The allowance is one three-topic event carrying a word, at the price of a history byte.
#[test]
fn test_the_allowance_is_one_three_topic_event_of_history() {
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(STORAGE_CALL_STIPEND_BYTES, 160);
    assert_eq!(storage_call_stipend(), 160 * COST_PER_HISTORY_BYTE);
}

/// The boundary the allowance exists for: a `transfer()` — 2,300 gas and nothing else — reaches a
/// `receive()` hook that emits one event, and the event's history costs the sender nothing beyond
/// the two write records the transfer itself writes.
///
/// Without the allowance the event's 160 bytes would have to come out of the 2,300, which does
/// not cover them at any price a history byte is worth.
#[test]
fn test_a_transfer_can_emit_one_event_because_of_the_allowance() {
    if runs_at_measurement_prices() {
        return;
    }
    let sender = call_with(CALL, RECEIVER, 1, TRANSFER_GAS).append(STOP).build();
    let receiver = event().append(STOP).build();
    let outcome = run(db(sender, receiver), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(events(&outcome).len(), 1, "the hook emitted its event");
    assert_eq!(
        outcome.result.logs()[0],
        transfer_log(SENDER, RECEIVER, U256::from(1)),
        "after the transfer's own log, which took nothing from the allowance",
    );
    assert_eq!(
        outcome.gas.history,
        body() + 2 * record(),
        "the body and the transfer's two records; the event's bytes the allowance paid for",
    );
}

/// One event is all it buys: a second one has to come out of the 2,300 gas, which cannot cover
/// it, so the hook runs out of gas and the transfer fails.
#[test]
fn test_the_allowance_buys_one_event_and_no_more() {
    if runs_at_measurement_prices() {
        return;
    }
    // The caller records whether the transfer succeeded, then stops.
    let sender = call_with(CALL, RECEIVER, 1, TRANSFER_GAS).append(STOP).build();
    let two_events = event().append_many(event().build().iter().copied()).append(STOP).build();
    let outcome = run(db(sender, two_events), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));

    assert!(outcome.result.is_success(), "the caller survives the hook's failure");
    assert!(outcome.result.logs().is_empty(), "the hook ran out and kept neither event");
    assert_eq!(outcome.gas.history, body(), "and the transfer's records came back with it");
}

/// A sender that makes one value transfer to [`RECEIVER`], forwarding `gas` beside the 2,300 the
/// transfer's stipend adds, and returns the call's success flag.
fn transfer_returning_its_flag(gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(1u64)
        .push_address(RECEIVER)
        .push_number(gas)
        .append(CALL)
        .return_top()
        .build()
}

/// Whether the transfer of [`transfer_returning_its_flag`] succeeded, read off the sender's output.
fn transfer_succeeded(outcome: &MegaTransactionOutcome) -> bool {
    assert!(outcome.result.is_success(), "the sender survives its hook: {:?}", outcome.result);
    let output = outcome.result.output().expect("the sender returns the flag");
    U256::from_be_slice(output) == U256::from(1)
}

/// A hook of `gas` gas in all — the forward and the transfer's stipend — running `hook`, with
/// `slot_zero` already in its slot 0.
fn run_hook(hook: Bytes, gas: u64, slot_zero: U256) -> MegaTransactionOutcome {
    let forward = gas.checked_sub(TRANSFER_GAS).expect("the hook gets at least the stipend");
    let db = db(transfer_returning_its_flag(forward), hook).account_storage(
        RECEIVER,
        U256::ZERO,
        slot_zero,
    );
    run(db, call(CALLER, SENDER, U256::ZERO, GAS_LIMIT))
}

/// A hook that counts `n` down to zero and stops: `PUSH2 n`, then `n` turns of `JUMPDEST; PUSH1 1;
/// SWAP1; SUB; DUP1; PUSH1 3; JUMPI`.
fn countdown(n: u16) -> Bytes {
    let [high, low] = n.to_be_bytes();
    Bytes::from(vec![PUSH2, high, low, JUMPDEST, PUSH1, 1, SWAP1, SUB, DUP1, PUSH1, 3, JUMPI, STOP])
}

/// What [`countdown`] costs, by hand from the Osaka opcode prices: `PUSH2` 3, and per turn
/// `JUMPDEST` 1, `PUSH1` 3, `SWAP1` 3, `SUB` 3, `DUP1` 3, `PUSH1` 3 and `JUMPI` 10.
const fn countdown_gas(n: u64) -> u64 {
    3 + 26 * n
}

/// The history gas one write record costs at the spec's price: 40 bytes at 88 gas a byte.
const RECORD_HISTORY: u64 = 3_520;

/// The allowance at the spec's price: 160 bytes at 88 gas a byte.
const ALLOWANCE: u64 = 14_080;

/// The allowance cannot pay for computation: a hook given one gas less than its computation costs
/// runs out of gas, though the 14,080 of allowance beside it would cover that gas many times over,
/// and given exactly the cost it completes. An allowance that entered the frame's gas, or that
/// computation could fall back on, would carry the short hook through.
///
/// Rule [S8.4]. Expected values `independent`: the hook's cost is summed by hand from the opcode
/// prices, the allowance and a record's history from the spec's 160 and 40 bytes at 88 gas a byte,
/// and the exact run is the control that the sum is the hook's whole cost.
#[test]
fn test_the_allowance_cannot_be_spent_on_computation() {
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(storage_call_stipend(), ALLOWANCE);
    let turns: u16 = 200;
    let cost = countdown_gas(u64::from(turns));

    let short = run_hook(countdown(turns), cost - 1, U256::ZERO);
    assert!(!transfer_succeeded(&short), "one gas short of the computation is out of gas");
    assert_eq!(short.gas.history, body(), "the failed hook's transfer records came back");

    let exact = run_hook(countdown(turns), cost, U256::ZERO);
    assert!(transfer_succeeded(&exact), "the computation's own cost is enough");
    assert_eq!(exact.gas.history, body() + 2 * RECORD_HISTORY, "the transfer's two records");
}

/// The hook's `SSTORE(0, 2)`: `PUSH1 2; PUSH0; SSTORE; STOP`.
fn writes_slot_zero() -> Bytes {
    Bytes::from(vec![PUSH1, 2, PUSH0, SSTORE, STOP])
}

/// The allowance cannot pay a write record: a hook that changes a slot it already holds, given the
/// write's regular gas and one gas less than its record's history, runs out of gas and keeps no
/// write, though the allowance beside it would cover the record's 3,520 four times over. Given the
/// record in full it keeps the write.
///
/// The slot is not fresh, so the write charges no state gas: the record's history is the only
/// charge past the regular gas, and the only one the allowance could have paid.
///
/// Rule [S8.3]. Expected values `independent`: the write's regular gas is the schedule's table by
/// hand — `PUSH1` 3, `PUSH0` 2, and an `SSTORE` that resets a cold slot, 100 static, 2,800 for the
/// reset and 2,100 for the cold load — and the record's history 40 bytes at 88 gas a byte.
#[test]
fn test_the_allowance_cannot_be_spent_on_a_write_record() {
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(mega_evm::write_record_history_gas(1), Some(RECORD_HISTORY));
    let regular = 3 + 2 + 100 + 2_800 + 2_100;

    let short = run_hook(writes_slot_zero(), regular + RECORD_HISTORY - 1, U256::from(1));
    assert!(!transfer_succeeded(&short), "one gas short of the record is out of gas");
    assert_eq!(short.usage.write_records, 0, "the hook kept no write");
    assert_eq!(short.gas.history, body(), "and the transfer's records came back with it");

    let exact = run_hook(writes_slot_zero(), regular + RECORD_HISTORY, U256::from(1));
    assert!(transfer_succeeded(&exact), "the record paid from the hook's own gas");
    assert_eq!(exact.usage.write_records, 3, "the transfer's two accounts and the slot");
    assert_eq!(exact.gas.history, body() + 3 * RECORD_HISTORY);
}

/// The allowance cannot pay state gas either: a hook that fills a fresh slot, given the write's
/// regular gas and one gas less than the slot's state gas, runs out of gas and keeps no write.
/// An allowance that paid the state gas would carry it through: drawn first, its 14,080 would leave
/// the hook more than the record's 3,520 to pay from its own gas. Given the state gas and the
/// record in full it keeps the write.
///
/// Rule [S8.5]. Expected values `independent`: the write's regular gas is the schedule's table by
/// hand — `PUSH1` 3, `PUSH0` 2, and an `SSTORE` that fills a cold fresh slot, 100 static, 19,900
/// for the fresh slot and 2,100 for the cold load — the slot's state gas is the spec's 97,920 at
/// the minimum bucket, and the record's history 40 bytes at 88 gas a byte.
#[test]
fn test_the_allowance_cannot_be_spent_on_state() {
    if runs_at_measurement_prices() {
        return;
    }
    let state = 97_920;
    assert_eq!(crate::common::slot_state_gas(), state);
    let regular = 3 + 2 + 100 + 19_900 + 2_100;

    let short = run_hook(writes_slot_zero(), regular + state - 1, U256::ZERO);
    assert!(!transfer_succeeded(&short), "one gas short of the state gas is out of gas");
    assert_eq!(short.usage.write_records, 0, "the hook kept no write");
    assert_eq!(short.gas.state, 0, "and no state gas");

    let exact = run_hook(writes_slot_zero(), regular + state + RECORD_HISTORY, U256::ZERO);
    assert!(transfer_succeeded(&exact), "the slot paid from the hook's own gas");
    assert_eq!(exact.gas.state, state);
    assert_eq!(exact.usage.write_records, 3, "the transfer's two accounts and the slot");
}

/* ---------- who is granted one ---------- */

/// A transaction's own frame is granted nothing: its sender chose the gas limit, so a value
/// transaction whose recipient emits an event pays for that event itself.
#[test]
fn test_a_top_level_transfer_gets_no_allowance() {
    if runs_at_measurement_prices() {
        return;
    }
    let receiver = event().append(STOP).build();
    let mut tx = call(CALLER, RECEIVER, U256::from(1), GAS_LIMIT);
    tx.0.base.value = U256::from(1);
    let outcome = run(db(Bytes::new(), receiver), tx);

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.gas.history,
        body() + record() + STORAGE_CALL_STIPEND_BYTES * COST_PER_HISTORY_BYTE,
        "the body, the recipient's record, and the event the transaction paid for itself",
    );
}

/// A call that carries no value is granted nothing, whatever it forwards: the allowance follows
/// the transfer, not the call.
#[test]
fn test_a_call_without_value_gets_no_allowance() {
    if runs_at_measurement_prices() {
        return;
    }
    let receiver = event().append(STOP).build();
    let with_value = call_with(CALL, RECEIVER, 1, 1_000_000).append(STOP).build();
    let without = call_with(CALL, RECEIVER, 0, 1_000_000).append(STOP).build();
    let event_history = STORAGE_CALL_STIPEND_BYTES * COST_PER_HISTORY_BYTE;

    let paid = run(db(without, receiver.clone()), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));
    let granted = run(db(with_value, receiver), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));

    assert!(paid.result.is_success() && granted.result.is_success());
    assert_eq!(paid.gas.history, body() + event_history, "no transfer, no allowance");
    assert_eq!(granted.gas.history, body() + 2 * record(), "the allowance paid for the event");
}

/// `CALLCODE` transfers value and is granted one; `DELEGATECALL` carries none and is not.
/// `CALLCODE` runs the callee's code in the caller's own account, so its transfer writes one
/// account rather than two.
///
/// `STATICCALL` carries no value either, and a static frame may not emit a log at all, so there
/// is nothing an allowance could buy it.
#[test]
fn test_callcode_is_granted_one_and_delegatecall_is_not() {
    if runs_at_measurement_prices() {
        return;
    }
    let receiver = event().append(STOP).build();
    let event_history = STORAGE_CALL_STIPEND_BYTES * COST_PER_HISTORY_BYTE;
    let at = |scheme, value| {
        let sender = call_with(scheme, RECEIVER, value, 1_000_000).append(STOP).build();
        run(db(sender, receiver.clone()), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT))
    };

    let call_code = at(CALLCODE, 1);
    assert!(call_code.result.is_success(), "{:?}", call_code.result);
    assert_eq!(call_code.gas.history, body() + record(), "one record, and the allowance paid");

    let delegate = at(DELEGATECALL, 0);
    assert!(delegate.result.is_success(), "{:?}", delegate.result);
    assert_eq!(
        delegate.gas.history,
        body() + event_history,
        "no value, no allowance, so the frame pays for its own event",
    );
}

/* ---------- the three leak paths ---------- */

/// A value call a system contract's interceptor answers neither takes the caller's allowance nor
/// leaves one behind: the answer is a frame that never ran, and the allowance stack stays aligned
/// with the frames.
///
/// The receiver calls `MegaAccessControl`, whose interceptor answers it without a frame, and then
/// emits its one event. It is forwarded far less gas than the event's history costs, so the event
/// is affordable only out of the allowance: if the answer had taken the receiver's allowance the
/// event would be unaffordable, and if it had left a second one behind, a second event would be
/// free.
#[test]
fn test_an_intercepted_call_neither_takes_nor_adds_an_allowance() {
    if runs_at_measurement_prices() {
        return;
    }
    // The intercepted call, then one event; the second program adds a second event after it.
    let intercepted_then_event = || {
        BytecodeBuilder::default()
            .mstore(0, IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR)
            .append_many([PUSH0, PUSH0])
            .push_number(4u64)
            .append(PUSH0)
            .push_address(ACCESS_CONTROL_ADDRESS)
            .append(GAS)
            .append(STATICCALL)
            .append(POP)
            .append_many(event().build().iter().copied())
    };
    let one_event = intercepted_then_event().append(STOP).build();
    let two_events =
        intercepted_then_event().append_many(event().build().iter().copied()).append(STOP).build();

    // Enough for the intercepted call and the event's computation, and far below the event's
    // history, which only the allowance can cover.
    const FORWARDED: u64 = 8_000;
    assert!(FORWARDED < storage_call_stipend(), "the event's history must need the allowance");
    let sender = call_with(CALL, RECEIVER, 1, FORWARDED).append(STOP).build();
    let with_controls = |receiver: Bytes| {
        db(sender.clone(), receiver)
            .account_balance(ACCESS_CONTROL_ADDRESS, U256::from(1))
            .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
    };

    let one = run(with_controls(one_event), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));
    assert!(one.result.is_success(), "{:?}", one.result);
    assert_eq!(events(&one).len(), 1, "the receiver still had its allowance");
    assert_eq!(one.gas.history, body() + 2 * record(), "the refusal cost no history");

    let two = run(with_controls(two_events), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));
    assert!(two.result.is_success(), "the caller survives");
    assert!(two.result.logs().is_empty(), "the refusal left no second allowance behind");
}

/// A transaction-level stop hands the allowance back to nobody: it was never gas, so the unwind
/// that returns a stopped transaction's regular gas and its reservoir has nothing of it to
/// return.
///
/// Both transactions below are stopped at the same instruction — the sender's own event, whose
/// thousand bytes cross the transaction's data-size limit on their own — and differ in one thing:
/// one receiver spent its whole allowance on an event, the other spent none of it. The reservoir
/// the stop gives back is the same to the gas; an allowance that unwound into it would leave the
/// spender with a whole allowance more. What the spender does pay beyond the other is the event's
/// computation, which is nothing like what its history costs.
#[test]
fn test_a_transaction_level_stop_does_not_hand_the_allowance_back() {
    if runs_at_measurement_prices() {
        return;
    }
    // Above the execution cap, so the transaction has a reservoir for the stop to give back.
    let gas_limit = mega_evm::constants::TX_GAS_LIMIT_CAP + 50_000_000;
    // The sender's own event is larger than this on its own, so it crosses whatever the receiver
    // appended before it.
    // 400 bytes above the body: the receiver's event fits, and the sender's log crosses.
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE + 400);
    let sender = call_with(CALL, RECEIVER, 1, TRANSFER_GAS)
        .push_number(1u64)
        .push_number(2u64)
        .push_number(3u64)
        .push_number(1_000u64)
        .push_number(0u64)
        .append(LOG3)
        .append(STOP)
        .build();
    let stop_with = |receiver: Bytes| {
        let outcome = run_limited(
            db(sender.clone(), receiver),
            limits,
            call(CALLER, SENDER, U256::ZERO, gas_limit),
        );
        assert!(
            matches!(
                outcome.limit_exceeded,
                Some(mega_evm::LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, .. })
            ),
            "the transaction must be stopped by its data-size limit: {:?}",
            outcome.limit_exceeded,
        );
        assert_eq!(outcome.gas.history, body(), "the stop took every frame charge back");
        outcome
    };

    let spent = stop_with(event().append(STOP).build());
    let unspent = stop_with(BytecodeBuilder::default().append(STOP).build());

    assert_eq!(
        spent.gas.reservoir_remaining, unspent.gas.reservoir_remaining,
        "the allowance is not gas, so the unwind gives back the same pool either way",
    );
    assert!(spent.gas.gas_used > unspent.gas.gas_used, "the event still costs what it computes");
    assert!(
        spent.gas.gas_used - unspent.gas.gas_used < storage_call_stipend(),
        "and nothing like what its history costs",
    );
}

/// An allowance the frame did not spend does not return to its caller: what the caller pays for a
/// silent transfer is what it pays for one whose receiver is not there at all, beyond the two
/// write records the transfer writes either way.
#[test]
fn test_an_unspent_allowance_does_not_return_to_the_caller() {
    if runs_at_measurement_prices() {
        return;
    }
    let sender = call_with(CALL, RECEIVER, 1, TRANSFER_GAS).append(STOP).build();
    let silent = run(db(sender.clone(), Bytes::new()), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));
    let logging =
        run(db(sender, event().append(STOP).build()), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));

    assert!(silent.result.is_success() && logging.result.is_success());
    assert_eq!(silent.gas.history, body() + 2 * record(), "no allowance came back as history");
    assert_eq!(logging.gas.history, silent.gas.history, "and the event cost the caller none");
    // The event's own computation is the only difference between the two.
    assert!(logging.gas.regular > silent.gas.regular, "the event still costs what it computes");
}

/// A chain of value transfers grants each frame its own allowance, and one frame's spending is
/// not another's: three frames each emit their event and the transaction pays for none of them.
///
/// The allowances are on the frames' lanes, which the frame lifecycle keeps aligned with the
/// frames themselves, so a chain cannot draw on an allowance granted further up.
#[test]
fn test_a_chain_of_transfers_grants_each_frame_its_own_allowance() {
    if runs_at_measurement_prices() {
        return;
    }
    const MIDDLE: Address = address!("0000000000000000000000000000000000500003");
    // Each hop emits its event and then passes a wei on with a `transfer()`'s gas.
    let hop = |next: Option<Address>| {
        let code = event();
        match next {
            Some(next) => code
                .append_many(call_with(CALL, next, 1, TRANSFER_GAS).build().iter().copied())
                .append(STOP)
                .build(),
            None => code.append(STOP).build(),
        }
    };
    let db = db(call_with(CALL, MIDDLE, 1, 1_000_000).append(STOP).build(), hop(None))
        .account_balance(MIDDLE, U256::from(1))
        .account_code(MIDDLE, hop(Some(RECEIVER)));
    let outcome = run(db, call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(events(&outcome).len(), 2, "both hops emitted their event");
    assert_eq!(outcome.result.logs().len(), 4, "beside the two transfers' logs");
    // The first transfer writes the sender's account and the middle one's; the second writes the
    // receiver's, the middle account being recorded already by the value it received.
    assert_eq!(
        outcome.gas.history,
        body() + 3 * record(),
        "the transfers' account writes, and no event on any ledger",
    );
}

/* ---------- how large the discount is ---------- */

/// How many times the self-calling contract below repeats the grant.
const GRANTS: u64 = 32;

/// `CALLDATASIZE; ISZERO; JUMPI` to the loop, with the event on the fall-through path, and the
/// loop calling this same address with one wei and one byte of calldata.
///
/// The entry frame is reached with empty calldata and runs the loop; each frame the loop starts
/// is reached with one byte and runs the event.
fn self_calling_loop() -> Bytes {
    /// Bytes of the `CALLDATASIZE; ISZERO; PUSH1 <loop>; JUMPI` header.
    const HEADER: usize = 5;

    let event = event().append(STOP).build();
    // The `JUMPDEST` the entry jumps to, past the header and the event.
    let loop_start =
        u8::try_from(HEADER + event.len()).expect("the header and one event fit in a byte");
    let mut code: Vec<u8> =
        vec![CALLDATASIZE, ISZERO, PUSH1, loop_start, JUMPI].into_iter().collect();
    code.extend_from_slice(&event);
    code.push(JUMPDEST);
    assert_eq!(code[loop_start as usize], JUMPDEST);
    for _ in 0..GRANTS {
        // `CALL(GAS, ADDRESS, 1 wei, args = memory[0..1], ret = [])`, discarding the flag.
        code.extend_from_slice(&[PUSH0, PUSH0, PUSH1, 1, PUSH0, PUSH1, 1, ADDRESS, GAS, CALL, POP]);
    }
    code.push(STOP);
    Bytes::from(code)
}

/// How far a transaction's history gas can fall behind the bytes it appended, and the column that
/// reports the bytes regardless.
///
/// The allowance is drawn before the frame's own gas rather than after it, so it is a discount on
/// the first 160 bytes of every value-transferring call rather than a fallback for a frame that
/// cannot pay. A contract that calls itself pays its own account's record once and nothing after
/// that, so the grant is repeatable for as long as the transaction's gas lasts: the loop below
/// costs about 10,000 gas a turn, which is some twenty thousand turns inside one transaction at
/// the execution cap — around three megabytes of log bytes, and close to 280,000,000 of history
/// gas, on no gas ledger.
///
/// The allowance keeps paying first. What closes the gap for a reader is the byte column beside
/// the gas: the transaction reports every byte it appended, the block sums them, and the
/// difference between the two columns is what the allowances paid, a number on record rather than
/// a rediscovery.
#[test]
fn test_the_gap_the_allowance_opens_between_bytes_and_gas_is_pinned() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(SENDER, U256::from(10u64.pow(9)))
        .account_code(SENDER, self_calling_loop());
    let outcome = run(db, call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.result.logs().len(), GRANTS as usize, "every frame emitted its event");
    // A self-call writes no recipient, and the caller's own account is written once, so the whole
    // loop leaves one record behind.
    assert_eq!(outcome.usage.write_records, 1);
    assert_eq!(
        outcome.usage.data_size,
        TX_BODY_SIZE + WRITE_RECORD_SIZE + GRANTS * STORAGE_CALL_STIPEND_BYTES,
        "the body, the record and every event's bytes are counted",
    );
    assert_eq!(
        outcome.gas.history,
        body() + record(),
        "and the history gas is the body and that one record: no event is on it",
    );
    assert_eq!(
        outcome.gas.history_bytes,
        TX_BODY_SIZE + WRITE_RECORD_SIZE + GRANTS * STORAGE_CALL_STIPEND_BYTES,
        "the byte column has every event on it",
    );

    let bytes_paid_for = outcome.gas.history / COST_PER_HISTORY_BYTE;
    assert_eq!(
        outcome.gas.history_bytes - bytes_paid_for,
        GRANTS * STORAGE_CALL_STIPEND_BYTES,
        "one allowance per value call, and every one of them off the gas ledger",
    );
}

/// A frame budget reverts the frame that crosses it and nothing else; the allowance that frame
/// held does not come back to its caller with the revert.
#[test]
fn test_a_frame_budget_revert_does_not_hand_the_allowance_back() {
    if runs_at_measurement_prices() {
        return;
    }
    // The receiver spends its allowance on an event and then crosses its own budget with a
    // second one; the sender carries on and the transaction succeeds. Its budget is 98% of the
    // sender's 500 bytes, 490: its start — the two records and the transfer log — and one event
    // fit, a second event does not.
    let receiver = event().append_many(event().build().iter().copied()).append(STOP).build();
    let sender = call_with(CALL, RECEIVER, 1, 1_000_000).append(STOP).build();
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(500);
    let start = 2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE;
    assert!(
        start + STORAGE_CALL_STIPEND_BYTES <= 490 && start + 2 * STORAGE_CALL_STIPEND_BYTES > 490
    );

    let spent = run_limited(
        db(sender.clone(), receiver),
        limits,
        call(CALLER, SENDER, U256::ZERO, GAS_LIMIT),
    );
    let quiet = run_limited(
        db(sender, BytecodeBuilder::default().append(STOP).build()),
        limits,
        call(CALLER, SENDER, U256::ZERO, GAS_LIMIT),
    );

    assert!(spent.result.is_success(), "the caller resumes past a frame budget");
    assert_eq!(spent.limit_exceeded, None, "a frame budget latches nothing");
    assert_eq!(spent.gas.history, body(), "the reverted frame kept neither event nor record");
    assert_eq!(quiet.gas.history, body() + 2 * record(), "the frame that returned kept its own");
    // The reverting frame's records came back to the caller and its computation did not, which
    // is the whole of the difference: no allowance came back with either.
    assert!(spent.gas.regular > quiet.gas.regular, "the events still cost what they compute");
    assert!(
        spent.gas.regular - quiet.gas.regular < storage_call_stipend(),
        "and the difference is that computation, not an allowance",
    );
}

/// A frame that drained its allowance and then reverted hands nothing back: the caller pays what
/// the frame computed and gets no allowance with it.
#[test]
fn test_a_drained_allowance_is_not_refunded_by_a_revert() {
    if runs_at_measurement_prices() {
        return;
    }
    let sender = call_with(CALL, RECEIVER, 1, 1_000_000).append(STOP).build();
    let drained_then_reverts = event().append_many([PUSH0, PUSH0]).append(0xFD).build();
    let reverts = BytecodeBuilder::default().append_many([PUSH0, PUSH0]).append(0xFD).build();

    let drained =
        run(db(sender.clone(), drained_then_reverts), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));
    let quiet = run(db(sender, reverts), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));

    assert!(drained.result.is_success(), "the caller survives the revert");
    assert_eq!(drained.gas.history, body(), "the reverted frame's records went with it");
    assert_eq!(quiet.gas.history, body());
    assert!(
        drained.gas.gas_used > quiet.gas.gas_used,
        "the caller pays for the event's computation and gets no allowance back",
    );
    assert!(
        drained.gas.gas_used - quiet.gas.gas_used < storage_call_stipend(),
        "and the difference is that computation, not an allowance",
    );
}

/// A call that forwards no gas at all still reaches a `receive()` hook that emits one event: the
/// EVM's own `CALL_STIPEND` pays for the computation and the allowance for the bytes.
#[test]
fn test_a_transfer_forwarding_no_gas_still_buys_its_event() {
    if runs_at_measurement_prices() {
        return;
    }
    let sender = call_with(CALL, RECEIVER, 1, 0).append(STOP).build();
    let outcome =
        run(db(sender, event().append(STOP).build()), call(CALLER, SENDER, U256::ZERO, GAS_LIMIT));

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(events(&outcome).len(), 1, "the hook emitted its event");
    assert_eq!(outcome.gas.history, body() + 2 * record());
}
