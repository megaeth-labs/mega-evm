//! The KV-update limit: the write records a transaction keeps, held per transaction at
//! `TX_KV_UPDATE_LIMIT` and per frame at 98/100 of what the parent has left, rounded down. A record
//! that crosses either stops the frame that makes it with `MegaLimitExceeded(1, limit)`, naming the
//! frame's budget when the record crosses it, and its caller resumes.
//!
//! Every figure is hand arithmetic from the record rules: a fresh slot's first write is one record,
//! a write back to its original takes it back.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    constants::{TX_DATA_LIMIT, TX_KV_UPDATE_LIMIT},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaLimitExceeded, MegaSpecId,
    MegaTransactionOutcome, ProtocolLimits, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, GAS, POP, PUSH0, RETURN, RETURNDATACOPY, RETURNDATASIZE},
    context::result::ExecutionResult,
};

use crate::common::{call, execute_under, share};

const CALLER: Address = address!("0000000000000000000000000000000000740000");
const A: Address = address!("0000000000000000000000000000000000740001");
const B: Address = address!("0000000000000000000000000000000000740002");
const C: Address = address!("0000000000000000000000000000000000740003");
const D: Address = address!("0000000000000000000000000000000000740004");

/// Room for a few hundred fresh slots and the calls around them, at any byte price the grid runs.
const GAS_LIMIT: u64 = 2_000_000_000;

/// Writes the fresh slots `from..from + count`.
fn writes(code: BytecodeBuilder, from: u64, count: u64) -> BytecodeBuilder {
    (from..from + count).fold(code, |code, slot| code.sstore(U256::from(slot), U256::from(1)))
}

/// Calls `to` with all its gas, then returns what the call returned or reverted with.
fn then_call_returning(code: BytecodeBuilder, to: Address) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(to)
        .append_many([GAS, CALL, POP, RETURNDATASIZE, PUSH0, PUSH0, RETURNDATACOPY])
        .append_many([RETURNDATASIZE, PUSH0, RETURN])
}

/// The revert data of a KV stop naming `limit`.
fn kv_stop(limit: u64) -> Bytes {
    MegaLimitExceeded { kind: LimitKind::KvUpdate.as_u8(), limit }.abi_encode().into()
}

/// Runs a call to `A` over `db` under `limits`.
fn run(db: MemoryDatabase, limits: EvmTxRuntimeLimits) -> MegaTransactionOutcome {
    execute_under(db, call(CALLER, A, U256::ZERO, GAS_LIMIT), limits)
}

/// Limits holding the transaction to `records` write records, and nothing else.
fn kv(records: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(records)
}

/// The spec's numbers: 500,000 records per transaction, more than the data size can hold at 40
/// bytes each, which is why the data size binds first at the protocol's values.
#[test]
fn test_the_kv_limit_is_the_specs() {
    assert_eq!(TX_KV_UPDATE_LIMIT, 500_000);
    let limits = ProtocolLimits::for_spec(MegaSpecId::SATIN).tx_runtime_limits;
    assert_eq!(limits.tx_kv_update_limit, TX_KV_UPDATE_LIMIT);
    const {
        assert!(
            TX_KV_UPDATE_LIMIT * WRITE_RECORD_SIZE > TX_DATA_LIMIT,
            "data size is the tighter cap"
        )
    };
    assert_eq!(EvmTxRuntimeLimits::no_limits().tx_kv_update_limit, u64::MAX);
}

/// The transaction's own frame gets what the transaction has left — all ten records of a limit of
/// ten when nothing was counted before it — and the eleventh record stops it, kind 1, naming that
/// budget. The stop settles as a revert and keeps nothing the frame wrote; ten records fit.
#[test]
fn test_the_eleventh_record_stops_the_transactions_own_frame() {
    let db = |count| {
        MemoryDatabase::default()
            .account_code(A, writes(BytecodeBuilder::default(), 1, count).stop().build())
    };
    let fits = run(db(10), kv(10));
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.write_records, 10);

    let stopped = run(db(11), kv(10));
    assert_eq!(
        stopped.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::KvUpdate,
            limit: 10,
            used: 11,
            frame_local: true
        })
    );
    match &stopped.result {
        ExecutionResult::Revert { output, .. } => assert_eq!(output, &kv_stop(10)),
        other => panic!("expected the stop, got {other:?}"),
    }
    assert_eq!(
        stopped.usage,
        LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 }
    );
}

/// A child gets 98/100 of what its caller has left, rounded down: under a limit of ten, a caller
/// that kept two records gives its child seven, and the child's eighth record stops it, kind 1,
/// naming seven. The caller catches the stop with its eight records left, and keeps writing within
/// them: eight more fit, which a ninth would not.
#[test]
fn test_a_child_gets_98_percent_of_what_its_caller_has_left() {
    assert_eq!(share(10 - 2), 7);
    let child = writes(BytecodeBuilder::default(), 1, 8).stop().build();
    let caller = |after: u64| {
        let code = writes(BytecodeBuilder::default(), 1, 2)
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(B)
            .append_many([GAS, CALL, POP]);
        let code = writes(code, 3, after);
        code.append_many([
            RETURNDATASIZE,
            PUSH0,
            PUSH0,
            RETURNDATACOPY,
            RETURNDATASIZE,
            PUSH0,
            RETURN,
        ])
        .build()
    };
    let db = |after| {
        MemoryDatabase::default().account_code(A, caller(after)).account_code(B, child.clone())
    };

    let caught = run(db(8), kv(10));
    assert!(caught.result.is_success(), "{:?}", caught.result);
    assert_eq!(caught.result.output(), Some(&kv_stop(7)), "the child's stop names its budget");
    assert_eq!(caught.limit_exceeded, None, "a stop below the transaction's own frame");
    assert_eq!(caught.usage.write_records, 10, "the caller's ten records, none of the child's");

    let stopped = run(db(9), kv(10));
    assert_eq!(
        stopped.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::KvUpdate,
            limit: 10,
            used: 11,
            frame_local: true
        }),
        "the caller's own eleventh record stops it in turn, naming its budget"
    );
}

/// The budgets at depth, under a limit of 100: the transaction's own frame gets 100, a child 98,
/// a grandchild 96, the next frame 94, each the share of what the frame above has left; the 95th
/// record of the frame three calls down stops it, kind 1, naming 94, and every caller resumes.
#[test]
fn test_the_kv_budgets_at_depth() {
    assert_eq!((share(100), share(share(100)), share(share(share(100)))), (98, 96, 94));
    let db = |records: u64| {
        MemoryDatabase::default()
            .account_code(A, then_call_returning(BytecodeBuilder::default(), B).build())
            .account_code(B, then_call_returning(BytecodeBuilder::default(), C).build())
            .account_code(C, then_call_returning(BytecodeBuilder::default(), D).build())
            .account_code(D, writes(BytecodeBuilder::default(), 1, records).stop().build())
    };
    let fits = run(db(94), kv(100));
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.write_records, 94);

    let caught = run(db(95), kv(100));
    assert!(caught.result.is_success(), "{:?}", caught.result);
    assert_eq!(caught.result.output(), Some(&kv_stop(94)), "D's stop, returned up the chain");
    assert_eq!(caught.usage.write_records, 0, "D's records went with its revert");
}

/// One record that crosses the data-size frame budget and the KV frame budget at once stops on
/// data size, the first of the counted dimensions: kind 0, naming the data-size budget.
#[test]
fn test_a_record_crossing_data_size_and_kv_stops_on_data_size() {
    let db = MemoryDatabase::default()
        .account_code(A, writes(BytecodeBuilder::default(), 1, 2).stop().build());
    let limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_kv_update_limit(1)
        .with_tx_data_size_limit(TX_BODY_SIZE + WRITE_RECORD_SIZE);
    let stopped = run(db, limits);
    assert_eq!(
        stopped.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: WRITE_RECORD_SIZE,
            used: 2 * WRITE_RECORD_SIZE,
            frame_local: true
        })
    );
}

/// A frame's remaining is its budget less its net, never more than the budget: a child that takes
/// back a record its caller made has a net of minus one, and the frame it starts still gets 98% of
/// the child's budget, not of the budget and the record it took back.
///
/// `A` writes its slot 1 and `DELEGATECALL`s `B`, which runs on `A`'s storage: under a limit of
/// 100, `A` has 99 left and `B` gets 97. `B` writes slot 1 back to its original, which takes `A`'s
/// record back, and calls `C`, which gets 98% of `B`'s 97, 95 — not of the 98 `B`'s pool holds —
/// and writes slots of its own: the 96th stops it, naming 95.
#[test]
fn test_a_negative_net_does_not_enlarge_the_next_childs_budget() {
    use revm::bytecode::opcode::DELEGATECALL;
    assert_eq!((share(99), share(97), share(98)), (97, 95, 96));
    let a = writes(BytecodeBuilder::default(), 1, 1)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .append_many([GAS, DELEGATECALL, POP, RETURNDATASIZE, PUSH0, PUSH0, RETURNDATACOPY])
        .append_many([RETURNDATASIZE, PUSH0, RETURN])
        .build();
    let b = then_call_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::ZERO), C)
        .build();
    let db = |records: u64| {
        MemoryDatabase::default()
            .account_code(A, a.clone())
            .account_code(B, b.clone())
            .account_code(C, writes(BytecodeBuilder::default(), 100, records).stop().build())
    };
    let fits = run(db(95), kv(100));
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.write_records, 95, "C's 95; A's record went back with B's write");

    let caught = run(db(96), kv(100));
    assert!(caught.result.is_success(), "{:?}", caught.result);
    assert_eq!(caught.result.output(), Some(&kv_stop(95)), "C's budget is the share of 97, not 98");
}

/// An EIP-7702 list that would keep more records than the transaction's limit allows is not
/// applied: none of its authorities is bumped, delegated or counted, and the transaction's own
/// frame returns the stop before it runs, kind 1, naming the transaction's limit, since no frame
/// budget exists yet. A list within the limit is applied.
#[test]
fn test_an_authorization_list_over_the_kv_limit_is_skipped_and_stops_the_first_frame() {
    use crate::common::authorizing_call;
    const DELEGATE: Address = address!("0000000000000000000000000000000000740005");
    const AUTHORITIES: [Address; 2] = [
        address!("0000000000000000000000000000000000740006"),
        address!("0000000000000000000000000000000000740007"),
    ];
    let db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10_u64.pow(18)))
            .account_code(A, writes(BytecodeBuilder::default(), 1, 1).stop().build())
    };
    let keys = AUTHORITIES;
    let list = [(keys[0], 0), (keys[1], 0)];
    let tx = authorizing_call(CALLER, A, U256::ZERO, GAS_LIMIT, DELEGATE, &list);

    let applied = execute_under(db(), tx.clone(), kv(3));
    assert!(applied.result.is_success(), "{:?}", applied.result);
    assert_eq!(applied.usage.write_records, 3, "two authorities and A's slot");

    let skipped = execute_under(db(), tx, kv(1));
    assert_eq!(
        skipped.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::KvUpdate,
            limit: 1,
            used: 2,
            frame_local: false
        })
    );
    match &skipped.result {
        ExecutionResult::Revert { output, .. } => assert_eq!(output, &kv_stop(1)),
        other => panic!("expected the stop, got {other:?}"),
    }
    assert_eq!(skipped.usage.write_records, 0, "nothing of the list is kept");
    for authority in &keys {
        let account = skipped.state.get(authority);
        assert!(account.is_none_or(|account| account.info.nonce == 0), "{authority}: not bumped");
        assert!(
            account.is_none_or(|account| account.info.is_empty_code_hash()),
            "{authority}: not delegated"
        );
    }
}
