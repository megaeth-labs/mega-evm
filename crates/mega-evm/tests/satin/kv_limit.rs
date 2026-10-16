//! The KV limit: the write records a transaction keeps, held to a limit of their own.
//!
//! The KV count is the write-record count the common execution layer keeps: one record per
//! account or storage write the transaction keeps, deduplicated per frame, taken back with a slot
//! written back to its original value and with the frame that fails. The sender's account and the
//! accounts fees are credited to are part of the transaction body, not records, so a transaction
//! that writes nothing counts none.
//!
//! The limit follows the data-size limit's rules in its own unit. A transaction is held to
//! `tx_kv_update_limit`; its own frame gets what the transaction has left and a child 98% of what
//! its parent has left, under `frame_kv_update_limit`. A frame that crosses its budget reverts
//! alone; a transaction that crosses its limit is stopped with a revert carrying
//! `MegaLimitExceeded(1, limit)`.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    system::{IMegaLimitControl, LIMIT_CONTROL_ADDRESS},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaEvm, MegaLimitExceeded,
    MegaTransaction, MegaTransactionOutcome, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::bytecode::opcode::{
    CALL, CREATE, GAS, INVALID, POP, PUSH0, PUSH1, RETURN, RETURNDATACOPY, RETURNDATASIZE,
    SELFDESTRUCT, STOP,
};

use crate::common::{authorizing_call, call, call_with_data, context};

const CALLER: Address = address!("0000000000000000000000000000000000500000");
const A: Address = address!("0000000000000000000000000000000000500001");
const B: Address = address!("0000000000000000000000000000000000500002");
const C: Address = address!("0000000000000000000000000000000000500003");
const DELEGATE: Address = address!("00000000000000000000000000000000000de1e5");
const AUTHORITY_1: Address = address!("00000000000000000000000000000000000a0001");
const AUTHORITY_2: Address = address!("00000000000000000000000000000000000a0002");

/// Enough gas for a few hundred fresh slots and the calls around them.
const GAS_LIMIT: u64 = 100_000_000;

/// 98% of `remaining`, the share a child frame is given.
fn share(remaining: u64) -> u64 {
    remaining * FRAME_DATA_SHARE_NUMERATOR / FRAME_DATA_SHARE_DENOMINATOR
}

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(A, U256::from(1_000))
}

/// Appends writes of the fresh slots `0..n`.
fn write_slots(mut builder: BytecodeBuilder, n: u64) -> BytecodeBuilder {
    for slot in 0..n {
        builder = builder.sstore(U256::from(slot), U256::from(slot + 1));
    }
    builder
}

/// Code that writes the fresh slots `0..n` and stops.
fn slots(n: u64) -> Bytes {
    write_slots(BytecodeBuilder::default(), n).stop().build()
}

/// Appends a `CALL` to `target` carrying `value` with all the gas, dropping its success flag.
fn then_call(builder: BytecodeBuilder, target: Address, value: u8) -> BytecodeBuilder {
    builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(PUSH1)
        .append(value)
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(POP)
}

/// Code that calls `target` with no value and stops.
fn calls(target: Address) -> Bytes {
    then_call(BytecodeBuilder::default(), target, 0).stop().build()
}

/// Calls `target` with no value and returns whatever it returned or reverted with.
fn call_and_return_its_output(target: Address) -> Bytes {
    then_call(BytecodeBuilder::default(), target, 0)
        .append(RETURNDATASIZE)
        .append_many([PUSH0, PUSH0])
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .append(PUSH0)
        .append(RETURN)
        .build()
}

fn run_under(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits)).execute_transaction(tx).unwrap()
}

/// Runs a call from `CALLER` to `A` over `db` under a KV limit of `limit` and nothing else.
fn run_kv(db: MemoryDatabase, limit: u64) -> MegaTransactionOutcome {
    run_under(
        db,
        call(CALLER, A, U256::ZERO, GAS_LIMIT),
        EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(limit),
    )
}

/// `n` records and the body, with nothing else counted.
const fn records(n: u64) -> LimitUsage {
    LimitUsage { data_size: TX_BODY_SIZE + n * WRITE_RECORD_SIZE, write_records: n }
}

/// The outcome was stopped by the transaction's KV limit `limit`, crossed at `used`: a revert
/// carrying `MegaLimitExceeded(1, limit)`, not a halt.
fn assert_kv_stopped(outcome: &MegaTransactionOutcome, limit: u64, used: u64) {
    assert!(!outcome.result.is_success() && !outcome.result.is_halt(), "{:?}", outcome.result);
    let stop =
        LimitCheck::ExceedsLimit { kind: LimitKind::KVUpdate, limit, used, frame_local: false };
    assert_eq!(outcome.limit_exceeded, Some(stop));
    assert_eq!(outcome.result.output().unwrap(), &stop.revert_data());
}

fn slot_written(outcome: &MegaTransactionOutcome, address: Address, slot: u64) -> bool {
    outcome.state.get(&address).is_some_and(|account| {
        account.storage.get(&U256::from(slot)).is_some_and(|value| value.is_changed())
    })
}

/* ---------- the count ---------- */

/// The KV count is the write records kept, by the rules every record follows: a slot counts on
/// its first change and not again, a slot written back counts nothing, writing zero to an empty
/// slot is no change, and the sender is the body's.
#[test]
fn test_the_kv_count_is_the_write_records_kept() {
    let cases: [(&str, Bytes, u64, u64); 7] = [
        ("an empty call", Bytes::new(), 0, 0),
        ("three fresh slots", slots(3), 3, 0),
        (
            "zero written to an empty slot",
            BytecodeBuilder::default().sstore(U256::ZERO, U256::ZERO).stop().build(),
            0,
            0,
        ),
        (
            "one slot written twice",
            BytecodeBuilder::default()
                .sstore(U256::ZERO, U256::from(1))
                .sstore(U256::ZERO, U256::from(2))
                .stop()
                .build(),
            1,
            0,
        ),
        (
            "one slot written and written back",
            BytecodeBuilder::default()
                .sstore(U256::ZERO, U256::from(1))
                .sstore(U256::ZERO, U256::ZERO)
                .stop()
                .build(),
            0,
            0,
        ),
        (
            "a value transfer: the frame's account and the recipient",
            then_call(BytecodeBuilder::default(), B, 1).stop().build(),
            2,
            1,
        ),
        (
            "a destruction that moves value: its beneficiary",
            BytecodeBuilder::default().push_address(B).append(SELFDESTRUCT).build(),
            1,
            1,
        ),
    ];
    for (name, code, kept, transfer_logs) in cases {
        let outcome = run_kv(funded().account_code(A, code), u64::MAX);
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        let transfer_log_bytes = transfer_logs * mega_evm::TRANSFER_LOG_SIZE;
        let expected = records(kept);
        assert_eq!(
            outcome.usage,
            LimitUsage { data_size: expected.data_size + transfer_log_bytes, ..expected },
            "{name}: the records, and the transfer log of the value it moved"
        );
    }

    let transfer = run_under(
        funded(),
        call(CALLER, B, U256::from(1), GAS_LIMIT),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(transfer.result.is_success());
    assert_eq!(
        transfer.usage,
        LimitUsage {
            data_size: records(1).data_size + mega_evm::TRANSFER_LOG_SIZE,
            write_records: 1
        },
        "a value transaction records its recipient alone, beside its transfer log"
    );
}

/* ---------- the transaction's limit ---------- */

/// A limit equal to the count holds, and one below it stops the transaction at the write that
/// crosses it: a revert naming the KV dimension, with every write taken back.
#[test]
fn test_the_transaction_limit_holds_at_the_count_and_stops_one_above() {
    let fits = run_kv(funded().account_code(A, slots(3)), 3);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.limit_exceeded, None);
    assert_eq!(fits.usage, records(3));

    let stopped = run_kv(funded().account_code(A, slots(4)), 3);
    assert_kv_stopped(&stopped, 3, 4);
    assert_eq!(stopped.usage, records(0), "the stop takes every write back");
    for slot in 0..4 {
        assert!(!slot_written(&stopped, A, slot), "slot {slot} is not kept");
    }
}

/// The transaction's own frame gets the whole transaction limit, so a crossing there is the
/// transaction's and not a frame-local revert: the stop is latched and reported, and it is still
/// a revert, never a halt.
#[test]
fn test_a_crossing_in_the_first_frame_is_the_transactions() {
    let outcome = run_kv(funded().account_code(A, slots(101)), 100);
    assert_kv_stopped(&outcome, 100, 101);
    assert_eq!(outcome.usage, records(0));
}

/// A value transaction's recipient is the record its first frame's start makes. A limit it
/// crosses stops the transaction before that frame is built: nothing moves.
#[test]
fn test_a_value_transaction_over_the_limit_moves_nothing() {
    let run = |limit| {
        run_under(
            funded(),
            call(CALLER, B, U256::from(7), GAS_LIMIT),
            EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(limit),
        )
    };
    assert!(run(1).result.is_success());
    let stopped = run(0);
    assert_kv_stopped(&stopped, 0, 1);
    assert!(stopped.state.get(&B).is_none_or(|b| b.info.balance.is_zero()), "no value moved");
}

/// A nested value call records its caller's account and its recipient when it starts, two at
/// once. When the two cross the transaction's limit they stop the transaction, although they also
/// cross the child's own share: the transaction's limit is checked first.
#[test]
fn test_a_frame_start_over_the_transaction_limit_stops_the_transaction() {
    let code = then_call(BytecodeBuilder::default(), B, 1).stop().build();
    let library =
        BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).append(INVALID).build();
    let db = || funded().account_code(A, code.clone()).account_code(B, library.clone());

    let stopped = run_kv(db(), 1);
    assert_kv_stopped(&stopped, 1, 2);
    assert!(stopped.state.get(&B).is_none_or(|b| b.info.balance.is_zero()), "no value moved");

    // Without a limit the child starts, and its halt takes its two start records and its write
    // back with it.
    let ran = run_kv(db(), u64::MAX);
    assert!(ran.result.is_success(), "{:?}", ran.result);
    assert_eq!(ran.usage, records(0));
}

/// Data size is checked before the write records: a record that crosses both limits at once is
/// reported as a data-size stop.
#[test]
fn test_a_record_crossing_both_limits_reports_the_data_size() {
    let outcome = run_under(
        funded().account_code(A, slots(3)),
        call(CALLER, A, U256::ZERO, GAS_LIMIT),
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE)
            .with_tx_kv_update_limit(2),
    );
    assert!(!outcome.result.is_success() && !outcome.result.is_halt());
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE,
            used: TX_BODY_SIZE + 3 * WRITE_RECORD_SIZE,
            frame_local: false,
        })
    );
}

/// A transaction that filled the limit leaves nothing of it to the next one the same EVM runs:
/// each is held to the limit from zero.
#[test]
fn test_each_transaction_is_held_to_the_limit_from_zero() {
    let mut evm = MegaEvm::new(
        context(funded().account_code(A, slots(3)))
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(3)),
    );
    for run in 0..2 {
        let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
        assert!(outcome.result.is_success(), "run {run}: {:?}", outcome.result);
        assert_eq!(outcome.usage, records(3), "run {run}");
    }
}

/* ---------- the frame budgets ---------- */

/// With room for 100 records, a child may keep 98 and a grandchild 96; a parent that kept 20
/// leaves its child 78. A frame that crosses its share reverts alone: the transaction succeeds
/// with what the other frames kept, and a sibling started after it gets its full share.
#[test]
fn test_a_child_gets_98_percent_of_what_its_parent_has_left_in_records() {
    assert_eq!((share(100), share(98), share(80)), (98, 96, 78));
    let stop = || BytecodeBuilder::default().stop().build();
    let parent_of_20 =
        || then_call(write_slots(BytecodeBuilder::default(), 20), B, 0).stop().build();
    let cases: [(&str, Bytes, Bytes, Bytes, u64); 8] = [
        ("a child that fills its share", calls(B), slots(98), stop(), 98),
        ("a child one record over its share", calls(B), slots(99), stop(), 0),
        ("a child that fills what a parent of 20 left", parent_of_20(), slots(78), stop(), 98),
        ("a child one record over what a parent of 20 left", parent_of_20(), slots(79), stop(), 20),
        ("a grandchild that fills its share", calls(B), calls(C), slots(96), 96),
        ("a grandchild one record over its share", calls(B), calls(C), slots(97), 0),
        (
            "a sibling after a child that crossed its share",
            then_call(then_call(BytecodeBuilder::default(), B, 0), C, 0).stop().build(),
            slots(99),
            slots(98),
            98,
        ),
        (
            "a parent that writes after its child crossed",
            then_call(BytecodeBuilder::default(), B, 0)
                .sstore(U256::from(500), U256::from(1))
                .stop()
                .build(),
            slots(99),
            stop(),
            1,
        ),
    ];
    for (name, a, b, c, kept) in cases {
        let db = funded().account_code(A, a).account_code(B, b).account_code(C, c);
        let outcome = run_kv(db, 100);
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{name}: a frame budget does not latch");
        assert_eq!(outcome.usage, records(kept), "{name}");
    }
}

/// The child that crosses its share reverts with `MegaLimitExceeded` naming the KV dimension and
/// the share it crossed, which is what its caller reads.
#[test]
fn test_a_child_that_crosses_its_share_reverts_with_the_share() {
    let db = funded().account_code(A, call_and_return_its_output(B)).account_code(B, slots(99));
    let outcome = run_kv(db, 100);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let reverted = MegaLimitExceeded::abi_decode(outcome.result.output().unwrap()).unwrap();
    assert_eq!(reverted, MegaLimitExceeded { kind: LimitKind::KVUpdate.as_u8(), limit: 98 });
}

/// The frame cap binds every frame, the transaction's own included, and reverts the frame alone:
/// a top-level frame over the cap reverts without latching the transaction.
#[test]
fn test_the_frame_cap_binds_a_frame_alone() {
    let run = |a: Bytes, b: Bytes| {
        run_under(
            funded().account_code(A, a).account_code(B, b),
            call(CALLER, A, U256::ZERO, GAS_LIMIT),
            EvmTxRuntimeLimits::no_limits().with_frame_kv_update_limit(10),
        )
    };
    let fits = run(slots(10), Bytes::new());
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage, records(10));

    let over = run(slots(11), Bytes::new());
    assert!(!over.result.is_success() && !over.result.is_halt(), "{:?}", over.result);
    assert_eq!(over.limit_exceeded, None, "a frame cap is not the transaction's limit");
    assert_eq!(
        over.result.output().unwrap().as_ref(),
        MegaLimitExceeded { kind: LimitKind::KVUpdate.as_u8(), limit: 10 }.abi_encode()
    );

    // A child's budget is the share of what its parent has left, under the same cap.
    let child = run(call_and_return_its_output(B), slots(10));
    assert!(child.result.is_success(), "{:?}", child.result);
    assert_eq!(
        MegaLimitExceeded::abi_decode(child.result.output().unwrap()).unwrap(),
        MegaLimitExceeded { kind: LimitKind::KVUpdate.as_u8(), limit: 9 },
        "the child may keep 98% of ten",
    );
}

/// A creation too big for its share still bumps its creator's nonce, and the record of that
/// write lands on the creator: the creator is held to its budget with it before it runs on.
///
/// `A` calls `B`, which may keep 98 records. `B` writes 97 or 98 slots, then creates: the
/// creation's two start records cross its share of what `B` has left, so it is stopped. With one
/// record left, `B` keeps the nonce record at exactly its budget. With none left, the record puts
/// `B` over it: `B` reverts alone, and `A` returns its revert.
#[test]
fn test_a_failed_creations_nonce_record_holds_its_creator_to_its_budget() {
    let creator = |written| {
        write_slots(BytecodeBuilder::default(), written)
            .append_many([PUSH0, PUSH0, PUSH0])
            .append(CREATE)
            .append(POP)
            .append(STOP)
            .build()
    };
    let run = |written| {
        run_kv(
            funded()
                .account_code(A, call_and_return_its_output(B))
                .account_code(B, creator(written)),
            100,
        )
    };

    let fits = run(97);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.limit_exceeded, None);
    assert_eq!(fits.usage, records(98), "97 slots and B's nonce, exactly B's budget");
    assert_eq!(fits.state[&B].info.nonce, 1);

    let over = run(98);
    assert!(over.result.is_success(), "A resumes: {:?}", over.result);
    assert_eq!(over.limit_exceeded, None, "a frame budget does not latch");
    assert_eq!(over.usage, records(0), "B's slots and nonce went with its revert");
    assert!(over.state.get(&B).is_none_or(|b| b.info.nonce == 0));
    assert_eq!(
        over.result.output().unwrap().as_ref(),
        MegaLimitExceeded { kind: LimitKind::KVUpdate.as_u8(), limit: 98 }.abi_encode(),
    );
}

/* ---------- records made before the first frame ---------- */

/// The records of applied EIP-7702 authorities are made before the first frame. When they cross
/// the limit they are not applied: the limit is enforced before the writes it guards, and the
/// transaction is stopped at its first frame — whatever that frame's target, an intercepted
/// system contract included. Authorities that exist already are records all the same.
#[test]
fn test_authorities_crossing_the_kv_limit_are_not_applied() {
    let db = || {
        funded()
            .account_balance(AUTHORITY_1, U256::from(1))
            .account_balance(AUTHORITY_2, U256::from(1))
    };
    let authorities = [(AUTHORITY_1, 0), (AUTHORITY_2, 0)];
    let run = |to: Address, data: Bytes, limit: u64| {
        let mut tx = authorizing_call(CALLER, to, U256::ZERO, GAS_LIMIT, DELEGATE, &authorities);
        tx.0.base.data = data;
        run_under(db(), tx, EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(limit))
    };
    let selector = Bytes::from(IMegaLimitControl::remainingComputeGasCall::SELECTOR.to_vec());
    for (to, data) in [(A, Bytes::new()), (LIMIT_CONTROL_ADDRESS, selector)] {
        let applied = run(to, data.clone(), 2);
        assert!(applied.result.is_success(), "{to}: {:?}", applied.result);
        assert_eq!(applied.usage.write_records, 2, "{to}");

        let stopped = run(to, data, 1);
        assert_kv_stopped(&stopped, 1, 2);
        assert_eq!(stopped.usage.write_records, 0, "{to}");
        for authority in [AUTHORITY_1, AUTHORITY_2] {
            let account = stopped.state.get(&authority);
            assert!(
                account.is_none_or(|a| a.info.nonce == 0 && a.info.is_empty_code_hash()),
                "{to}: {authority} was not delegated: {account:?}"
            );
        }
    }
}

/// The records made before the first frame and the ones made in it add up to the limit: two
/// applied authorities leave the first frame room for one slot, and a second one stops it.
#[test]
fn test_authorities_and_execution_add_up_to_the_stop() {
    let run = |written| {
        let tx = authorizing_call(
            CALLER,
            A,
            U256::ZERO,
            GAS_LIMIT,
            DELEGATE,
            &[(AUTHORITY_1, 0), (AUTHORITY_2, 0)],
        );
        run_under(
            funded().account_code(A, slots(written)),
            tx,
            EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(3),
        )
    };
    let fits = run(1);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.write_records, 3);
    let stopped = run(2);
    assert_kv_stopped(&stopped, 3, 4);
    assert_eq!(stopped.usage.write_records, 2, "the authorities outlive the first frame's stop");
}

/// Calldata is data size and never a record: however much of it a transaction carries, its KV
/// count is what it writes.
#[test]
fn test_calldata_is_not_a_record() {
    let outcome = run_under(
        funded().account_code(A, slots(1)),
        call_with_data(CALLER, A, Bytes::from(vec![0xab; 1_000]), GAS_LIMIT),
        EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(1),
    );
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 1);
}
