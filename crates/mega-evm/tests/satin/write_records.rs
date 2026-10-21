//! The write records the common execution layer counts, and when it counts them.
//!
//! A write record is one account or storage write, 40 bytes of data size. The sites: the first
//! change of a slot in the transaction (taken back when the slot is written back), a value
//! transfer's sender and recipient, a creation's creator nonce and created account, a
//! `SELFDESTRUCT` that moves value to another account, an applied EIP-7702 authority, and the
//! transaction's own value recipient or created account. The sender's own account is part of the
//! transaction body and is not a record.
//!
//! An account's record follows the net change of its info within the transaction: a write that
//! makes an unchanged account's nonce, balance or code differ from what the transaction loaded
//! creates the record, a write that returns it to that takes the record back, and a write to an
//! account that already differs adds nothing. A creation records the account it creates. The
//! records a frame made are discarded with the frame that fails; a creator's nonce record is the
//! creating frame's, and survives the creation's failure.
//!
//! Every value movement also leaves an EIP-7708 transfer log, which is data size and not a record:
//! the figures below carry [`TRANSFER_LOG_SIZE`] for each one a transaction keeps.

use alloy_primitives::{address, Address, Bytes, Log, LogData, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        IMegaAccessControl, IOracle, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE,
    },
    test_utils::{is_transfer_log, BytecodeBuilder, MemoryDatabase},
    LimitUsage, MegaContext, MegaEvm, TRANSFER_LOG_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLDATASIZE, CALLER as CALLER_OPCODE, CALLVALUE, CREATE, DELEGATECALL, GAS, INVALID,
        JUMPDEST, JUMPI, LOG2, POP, PUSH0, PUSH1, RETURN, REVERT, SELFDESTRUCT, SSTORE, STOP,
    },
    interpreter::{interpreter::EthInterpreter, interpreter_types::Jumps, Interpreter},
    Database, Inspector,
};

use crate::common::{
    account_state_gas, call, call_with_data, context, create, history, run, value_call_room,
};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");
const CONTRACT2: Address = address!("0000000000000000000000000000000000100003");
/// The transaction body and nothing else.
const fn body_only() -> LimitUsage {
    LimitUsage { data_size: TX_BODY_SIZE, write_records: 0 }
}

/// `n` write records on top of the transaction body.
const fn records(n: u64) -> LimitUsage {
    LimitUsage { data_size: TX_BODY_SIZE + n * WRITE_RECORD_SIZE, write_records: n }
}

/// `n` write records and no body: the difference between two transactions that carry the same one.
const fn records_only(n: u64) -> LimitUsage {
    LimitUsage { data_size: n * WRITE_RECORD_SIZE, write_records: n }
}

fn with_extra(usage: LimitUsage, bytes: u64) -> LimitUsage {
    LimitUsage { data_size: usage.data_size + bytes, ..usage }
}

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
}

/// Appends `CALL(gas=GAS, target, value, args=[], ret=[])`, leaving the success flag.
fn append_value_call(builder: BytecodeBuilder, target: Address, value: u8) -> BytecodeBuilder {
    builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(PUSH1)
        .append(value)
        .push_address(target)
        .append(GAS)
        .append(CALL)
}

/// Appends a `CREATE` with value 1 and empty init code.
fn append_value_create(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder.append_many([PUSH0, PUSH0]).append(PUSH1).append(1u8).append(CREATE)
}

/* ---------- value transfers and creations: a record per account that changed ---------- */

/// Two value transfers from one frame record the sender once and each recipient: the first
/// transfer changes the sender's balance, and the second finds it changed already.
#[test]
fn test_two_value_calls_record_the_sender_once() {
    let code = append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP);
    let code = append_value_call(code, CONTRACT2, 1).append(POP).append(STOP).build();
    let (result, usage) = run(
        funded().account_code(CALLEE, code),
        call(CALLER, CALLEE, U256::ZERO, value_call_room()),
    );
    assert!(result.result.is_success());
    assert_eq!(
        usage,
        with_extra(records(3), 2 * TRANSFER_LOG_SIZE),
        "CALLEE once, CONTRACT, CONTRACT2, and a transfer log for each transfer"
    );
}

/// Value transfers to one recipient record it once, and the sender once: the first transfer
/// changes both accounts, and the later ones find them changed. Each transfer still logs.
///
/// Before the net-change rule a recipient was recorded by every transfer to it — N transfers to
/// one recipient kept N records — and only the sender's record was deduplicated, per frame.
#[test]
fn test_value_calls_to_one_recipient_record_it_once() {
    let code = (0..3).fold(BytecodeBuilder::default(), |code, _| {
        append_value_call(code, CONTRACT, 1).append(POP)
    });
    let (result, usage) = run(
        funded().account_code(CALLEE, code.append(STOP).build()),
        call(CALLER, CALLEE, U256::ZERO, value_call_room()),
    );
    assert!(result.result.is_success());
    assert_eq!(
        usage,
        with_extra(records(2), 3 * TRANSFER_LOG_SIZE),
        "CALLEE once, CONTRACT once, and a transfer log for each transfer"
    );
}

/* ---------- a record follows the net change of its account ---------- */

/// `CALLDATASIZE; JUMPI` over `body`, so that a call with empty calldata stops at once: the
/// transaction calls the code with a byte of calldata, and a call back into it carries none.
fn guarded_code(body: BytecodeBuilder) -> Bytes {
    BytecodeBuilder::default()
        .append(CALLDATASIZE)
        .push_number(5_u8)
        .append(JUMPI)
        .append(STOP)
        .append(JUMPDEST)
        .append_many(body.build_vec())
        .build()
}

/// `CALL(gas=GAS, CALLER, value, [], [])`, leaving the success flag: a transfer of `value` back
/// to whoever called the frame.
fn append_return_to_caller(builder: BytecodeBuilder, value: u8) -> BytecodeBuilder {
    builder.append_many([PUSH0, PUSH0, PUSH0, PUSH0]).append(PUSH1).append(value).append_many([
        CALLER_OPCODE,
        GAS,
        CALL,
    ])
}

/// Runs the transaction that calls `CALLEE`, holding `callee`, with a byte of calldata, and
/// reports its outcome.
fn run_guarded(callee: Bytes, contract: Bytes) -> mega_evm::MegaTransactionOutcome {
    let db = funded().account_code(CALLEE, callee).account_code(CONTRACT, contract);
    crate::common::execute(
        db,
        call_with_data(CALLER, CALLEE, Bytes::from_static(&[1]), value_call_room()),
    )
}

/// What a transaction [`run_guarded`] runs keeps beside `usage`: the byte of calldata it carries.
fn guarded(usage: LimitUsage) -> LimitUsage {
    with_extra(usage, 1)
}

/// The history a transaction [`run_guarded`] runs pays for its body, its byte of calldata and
/// `records` records, at the prices in effect.
fn body_and_records(records: u64) -> u64 {
    history(TX_BODY_SIZE + 1 + records * WRITE_RECORD_SIZE)
}

/// One transfer records its sender and its recipient, and logs.
#[test]
fn test_one_transfer_records_its_sender_and_its_recipient() {
    let outcome = run_guarded(
        guarded_code(append_value_call(BytecodeBuilder::default(), CONTRACT, 5).append(POP).stop()),
        Bytes::new(),
    );
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.usage,
        guarded(with_extra(records(2), TRANSFER_LOG_SIZE)),
        "CALLEE, CONTRACT, a log"
    );
    if !crate::common::history_rounds() {
        assert_eq!(outcome.gas.history, body_and_records(2));
    }
}

/// A transfer and its exact return take both records back: the recipient hands the value back to
/// its caller, so both balances are what the transaction loaded, and neither account keeps a
/// record. The two transfer logs stay, and the history the caller paid for the two records comes
/// back to it once the return that took them back is kept.
#[test]
fn test_a_transfer_and_its_exact_return_take_both_records_back() {
    let outcome = run_guarded(
        guarded_code(append_value_call(BytecodeBuilder::default(), CONTRACT, 5).append(POP).stop()),
        // Returns what it was sent, `CALLVALUE`, to `CALLER`.
        BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, CALLVALUE, CALLER_OPCODE, GAS, CALL, POP])
            .stop()
            .build(),
    );
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.usage,
        guarded(with_extra(body_only(), 2 * TRANSFER_LOG_SIZE)),
        "no record stands; the two transfer logs do"
    );
    assert_eq!(outcome.state[&CALLEE].info.balance, U256::from(10_000_000), "back to its balance");
    assert_eq!(outcome.state[&CONTRACT].info.balance, U256::ZERO);
    if !crate::common::history_rounds() {
        assert_eq!(outcome.gas.history, body_and_records(0), "the records' history came back");
    }
}

/// A partial return leaves both accounts changed, so both records stand.
#[test]
fn test_a_partial_return_keeps_both_records() {
    let outcome = run_guarded(
        guarded_code(append_value_call(BytecodeBuilder::default(), CONTRACT, 5).append(POP).stop()),
        append_return_to_caller(BytecodeBuilder::default(), 3).append(POP).stop().build(),
    );
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.usage,
        guarded(with_extra(records(2), 2 * TRANSFER_LOG_SIZE)),
        "CALLEE and CONTRACT still differ from what the transaction loaded"
    );
    if !crate::common::history_rounds() {
        assert_eq!(outcome.gas.history, body_and_records(2));
    }
}

/// A child that reverts after writing keeps nothing: a slot it wrote, a transfer it made and
/// the transfer it received go with it, and so does a return of the value it received, which its
/// revert undoes with the receipt. A child whose return alone fails — the call that hands the
/// value back reverts, and the child then succeeds — undoes the take-back and keeps the receipt:
/// both records stand, with the history the caller paid for them, and the child earned no refund
/// for a take-back that did not stand.
#[test]
fn test_a_child_that_reverts_after_writing_keeps_nothing() {
    let callee = |returns: u8| {
        BytecodeBuilder::default()
            .append(CALLDATASIZE)
            .push_number(7_u8)
            .append(JUMPI)
            // A call back into it, carrying no calldata: refuses or accepts the return.
            .append_many(if returns == STOP {
                [PUSH0, PUSH0, STOP]
            } else {
                [PUSH0, PUSH0, REVERT]
            })
            .append(JUMPDEST)
            .append_many(
                append_value_call(BytecodeBuilder::default(), CONTRACT, 5).append(POP).build_vec(),
            )
            .stop()
            .build()
    };
    let writes_then_reverts = append_value_call(
        BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)),
        CONTRACT2,
        1,
    )
    .append(POP)
    .append_many([PUSH0, PUSH0, REVERT])
    .build();
    let returns_then_reverts = append_return_to_caller(BytecodeBuilder::default(), 5)
        .append(POP)
        .append_many([PUSH0, PUSH0, REVERT])
        .build();
    let returns_then_stops =
        append_return_to_caller(BytecodeBuilder::default(), 5).append(POP).stop().build();
    for (case, callee, contract, expected, records) in [
        (
            "a slot and a transfer, then a revert",
            callee(STOP),
            writes_then_reverts,
            guarded(body_only()),
            0,
        ),
        (
            "the value handed back, then a revert",
            callee(STOP),
            returns_then_reverts,
            guarded(body_only()),
            0,
        ),
        (
            "the value handed back into a call that reverts",
            callee(REVERT),
            returns_then_stops,
            guarded(with_extra(records(2), TRANSFER_LOG_SIZE)),
            2,
        ),
    ] {
        let outcome = run_guarded(callee, contract);
        assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
        assert_eq!(outcome.usage, expected, "{case}");
        let kept = U256::from(5 * records / 2);
        assert_eq!(outcome.state[&CONTRACT].info.balance, kept, "{case}: what CONTRACT keeps");
        if !crate::common::history_rounds() {
            assert_eq!(outcome.gas.history, body_and_records(records), "{case}");
        }
    }
}

/// A creation that fails leaves its creator's nonce record on the caller: the bump stands, and
/// the creator's account differs by it. A second failing creation adds nothing, the creator being
/// changed already.
#[test]
fn test_a_creation_that_fails_leaves_the_creators_record_on_the_caller() {
    for (creations, case) in [(1, "one failed creation"), (2, "two failed creations")] {
        let db = funded().account_code(OUTER_CREATOR, reverting_creations(creations));
        let (result, usage) = run(db, call(CALLER, OUTER_CREATOR, U256::ZERO, value_call_room()));
        assert!(result.result.is_success(), "{case}");
        assert_eq!(usage, records(1), "{case}: the creator's nonce record alone");
        assert_eq!(result.state[&OUTER_CREATOR].info.nonce, creations as u64, "{case}");
    }
}

/// Nested creations record each account once: the outer creator, the account it creates — whose
/// own creation bumps a nonce the creation already changed — and the account that one creates.
#[test]
fn test_nested_creations_record_each_account_once() {
    // Init code that creates an account with empty init code and returns no code.
    let inner: Vec<u8> = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, CREATE, POP])
        .stop()
        .build_vec();
    let len = inner.len() as u64;
    let outer = BytecodeBuilder::default()
        .mstore(0, &inner)
        .push_number(len)
        .push_number(0_u64)
        .push_number(0_u64)
        .append_many([CREATE, POP])
        .stop()
        .build();
    let (result, usage) = run(
        funded().account_code(CALLEE, outer),
        call(CALLER, CALLEE, U256::ZERO, value_call_room()),
    );
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(usage, records(3), "CALLEE, the account it created, and the one that created");
    let created = CALLEE.create(0);
    assert_eq!(result.state[&created].info.nonce, 2, "it created one account of its own");
    assert!(result.state.contains_key(&created.create(1)));
}

/// A value call to the frame's own account moves nothing and records nothing, and neither does a
/// value `CALLCODE`, which moves the value from the calling account to that same account.
///
/// Before the net-change rule either recorded the calling account once, as the transfer's sender.
#[test]
fn test_a_value_call_to_self_records_nothing() {
    assert_eq!(nested_delta(&[SELF_CALLER]), LimitUsage::ZERO, "a CALL to itself");

    let code = BytecodeBuilder::default().callcode(CONTRACT, U256::from(1)).append(POP).stop();
    let (result, usage) = run(
        funded().account_code(CALLEE, code.build()),
        call(CALLER, CALLEE, U256::ZERO, value_call_room()),
    );
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(usage, body_only(), "a value CALLCODE");
}

/// A transfer to the transaction's sender records nothing for it — validation changed its account
/// already, by its fee and its nonce — and one to a fee account records it, unchanged until
/// execution writes it. The sender of the transfer is recorded either way.
#[test]
fn test_a_transfer_to_the_sender_and_to_a_fee_account() {
    for (case, target, expected) in [
        ("the sender", CALLER, with_extra(records(1), TRANSFER_LOG_SIZE)),
        ("the block beneficiary", COINBASE, with_extra(records(2), TRANSFER_LOG_SIZE)),
    ] {
        let code = append_value_call(BytecodeBuilder::default(), target, 1).append(POP).stop();
        let db = funded().account_code(CALLEE, code.build()).account_balance(COINBASE, U256::ONE);
        let block = revm::context::BlockEnv { beneficiary: COINBASE, ..crate::common::block() };
        let outcome = MegaEvm::new(context(db).with_block(block))
            .execute_transaction(call(CALLER, CALLEE, U256::ZERO, value_call_room()))
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
        assert_eq!(outcome.usage, expected, "{case}");
    }
}

/// A `SELFDESTRUCT` that moves value writes two accounts: the beneficiary, recorded when the
/// move changes it, and the destructed account, whose balance the move clears. A fresh
/// beneficiary is recorded; one a transfer already changed is not; and a destructed account whose
/// whole balance the transaction gave it goes back to what was loaded, taking its record back —
/// as does the caller the value goes back to.
#[test]
fn test_a_destruction_to_a_fresh_and_to_a_changed_beneficiary() {
    let destruct_to = |code: BytecodeBuilder, beneficiary: Address| {
        code.push_address(beneficiary).append(SELFDESTRUCT)
    };
    let sends_then_destructs = destruct_to(
        append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP),
        CONTRACT,
    );
    for (case, callee, contract, balance, expected) in [
        (
            "to a fresh beneficiary",
            destruct_to(BytecodeBuilder::default(), CONTRACT).build(),
            Bytes::new(),
            5,
            guarded(with_extra(records(2), TRANSFER_LOG_SIZE)),
        ),
        (
            "to a beneficiary a transfer changed",
            sends_then_destructs.build(),
            Bytes::new(),
            5,
            guarded(with_extra(records(2), 2 * TRANSFER_LOG_SIZE)),
        ),
        (
            "of an account whose balance the transaction gave it, back to the giver",
            guarded_code(
                append_value_call(BytecodeBuilder::default(), CONTRACT, 5).append(POP).stop(),
            ),
            BytecodeBuilder::default().append_many([CALLER_OPCODE, SELFDESTRUCT]).build(),
            10_000_000,
            guarded(with_extra(body_only(), 2 * TRANSFER_LOG_SIZE)),
        ),
    ] {
        let db = funded()
            .account_code(CALLEE, callee)
            .account_code(CONTRACT, contract)
            .account_balance(CALLEE, U256::from(balance));
        let outcome = crate::common::execute(
            db,
            call_with_data(CALLER, CALLEE, Bytes::from_static(&[1]), value_call_room()),
        );
        assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
        assert_eq!(outcome.usage, expected, "{case}");
    }
}

/// A creation and a value transfer from one frame record the frame's account once.
#[test]
fn test_create_then_call_record_the_frame_account_once() {
    let code = append_value_create(BytecodeBuilder::default()).append(POP);
    let code = append_value_call(code, CONTRACT, 1).append(POP).append(STOP).build();
    let (result, usage) = run(
        funded().account_code(CALLEE, code),
        call(CALLER, CALLEE, U256::ZERO, value_call_room()),
    );
    assert!(result.result.is_success());
    assert_eq!(
        usage,
        with_extra(records(3), 2 * TRANSFER_LOG_SIZE),
        "CALLEE once, the created account, CONTRACT, and a transfer log for each endowment"
    );
}

/// Two creations from one frame record the creator once and each created account.
#[test]
fn test_two_creates_record_the_creator_once() {
    let code = append_value_create(BytecodeBuilder::default()).append(POP);
    let code = append_value_create(code).append(POP).append(STOP).build();
    let (result, usage) = run(
        funded().account_code(CALLEE, code),
        call(CALLER, CALLEE, U256::ZERO, value_call_room()),
    );
    assert!(result.result.is_success());
    assert_eq!(
        usage,
        with_extra(records(3), 2 * TRANSFER_LOG_SIZE),
        "CALLEE once, two created accounts, and a transfer log for each endowment"
    );
}

/// A value transfer into a child that fails is discarded with the child, sender included.
#[test]
fn test_failed_child_discards_its_transfer_records() {
    let code =
        append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP).append(STOP).build();
    let db =
        funded().account_code(CALLEE, code).account_code(CONTRACT, Bytes::from_static(&[INVALID]));
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
    assert!(result.result.is_success(), "the outer call succeeds although the child halts");
    assert_eq!(usage, body_only());
}

/// After a failed transfer the sender can be recorded again by the next one.
///
/// The first child burns the gas it was forwarded, so the second transfer forwards none: its
/// recipient runs no code, and the records it makes are paid out of what the caller keeps, which
/// then does not depend on the price of the recipient's new account.
#[test]
fn test_failed_first_child_lets_the_next_transfer_record_the_sender() {
    let code = append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP);
    let code = code
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(PUSH1)
        .append(1u8)
        .push_address(CONTRACT2)
        .append(PUSH0)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let db =
        funded().account_code(CALLEE, code).account_code(CONTRACT, Bytes::from_static(&[INVALID]));
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
    assert!(result.result.is_success());
    assert_eq!(
        usage,
        with_extra(records(2), TRANSFER_LOG_SIZE),
        "CALLEE and CONTRACT2 and the transfer log, from the second transfer"
    );
}

/// A library a `DELEGATECALL` runs as its caller: a value transfer to [`CONTRACT`], then a revert
/// when `fails`.
fn library_transferring(fails: bool) -> Bytes {
    let code = append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP);
    if fails {
        code.append_many([PUSH0, PUSH0, REVERT]).build()
    } else {
        code.append(STOP).build()
    }
}

/// A `DELEGATECALL` runs as its caller, so a value transfer it makes records its caller's account,
/// and that record follows the delegate's frame: kept and handed to the caller when the delegate
/// succeeds, so a later transfer from the caller does not record it again; taken back with the
/// delegate's failure, so the later transfer records it.
#[test]
fn test_a_delegates_record_of_its_callers_account_follows_the_delegate() {
    const LIBRARY: Address = address!("0000000000000000000000000000000000100004");
    for fails in [false, true] {
        let code = BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(LIBRARY)
            .append_many([GAS, DELEGATECALL, POP]);
        let code = append_value_call(code, CONTRACT2, 1).append(POP).append(STOP).build();
        let db =
            funded().account_code(CALLEE, code).account_code(LIBRARY, library_transferring(fails));
        let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
        assert!(result.result.is_success(), "the caller succeeds");
        let expected = if fails {
            with_extra(records(2), TRANSFER_LOG_SIZE)
        } else {
            with_extra(records(3), 2 * TRANSFER_LOG_SIZE)
        };
        assert_eq!(usage, expected, "CALLEE once, then CONTRACT2, and CONTRACT unless it failed");
    }
}

const OUTER_CREATOR: Address = address!("000000000000000000000000000000000000C0FF");

/// `PUSH5 <PUSH1 0 PUSH1 0 REVERT>; PUSH1 0; MSTORE; (PUSH1 5; PUSH1 27; PUSH1 0; CREATE; POP) x
/// n`: `n` creations whose init code reverts.
fn reverting_creations(n: usize) -> Bytes {
    let mut code = vec![0x64, 0x60, 0x00, 0x60, 0x00, 0xFD, 0x60, 0x00, 0x52];
    for _ in 0..n {
        code.extend_from_slice(&[0x60, 0x05, 0x60, 0x1b, 0x60, 0x00, 0xf0, 0x50]);
    }
    code.push(0x00);
    code.into()
}

/// Two reverted creations record the creator once: the first record survives, so the second
/// finds the creator recorded.
#[test]
fn test_reverted_then_retried_create_records_the_creator_once() {
    let db = funded().account_code(OUTER_CREATOR, reverting_creations(2));
    let (result, usage) = run(db, call(CALLER, OUTER_CREATOR, U256::ZERO, value_call_room()));
    assert!(result.result.is_success());
    assert_eq!(usage, records(1));
}

/// A creation the creator cannot fund never starts (the opcode checks the endowment before it
/// builds a frame), so it records nothing.
#[test]
fn test_creation_without_funds_records_nothing() {
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0])
        .push_number(1_000_000_000_000_u64)
        .append(CREATE)
        .append(POP)
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(CALLEE, code);
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
    assert!(result.result.is_success());
    assert_eq!(usage, body_only());
    assert_eq!(result.state[&CALLEE].info.nonce, 0, "the nonce was never bumped");
}

const A: Address = address!("00000000000000000000000000000000A0000001");
const B: Address = address!("00000000000000000000000000000000B0000002");
const SELF_CALLER: Address = address!("000000000000000000000000000000005E1F0004");

fn value_delta(to: Address) -> LimitUsage {
    let db = || MemoryDatabase::default().account_balance(A, U256::from(1_000_000));
    let (with_value, usage_value) = run(db(), call(A, to, U256::from(1), value_call_room()));
    let (zero_value, usage_zero) = run(db(), call(A, to, U256::ZERO, value_call_room()));
    assert!(with_value.result.is_success() && zero_value.result.is_success());
    usage_value.saturating_sub(usage_zero)
}

/// A top-level transfer to the sender itself writes no account the body does not already count.
#[test]
fn test_top_level_self_transfer_no_double_count() {
    assert_eq!(value_delta(A), LimitUsage::ZERO);
}

/// A top-level transfer to another account records the recipient.
#[test]
fn test_top_level_transfer_counts_the_recipient() {
    assert_eq!(value_delta(B), with_extra(records_only(1), TRANSFER_LOG_SIZE), "and its log");
}

/// `CALLDATASIZE; JUMPI` to a body that calls `targets` with `value`, so an inner call with empty
/// calldata stops at once.
fn self_calling_code(targets: &[Address], value: u64) -> Bytes {
    let mut code = BytecodeBuilder::default()
        .append(CALLDATASIZE)
        .push_number(5_u8)
        .append(JUMPI)
        .append(STOP)
        .append(JUMPDEST);
    for target in targets {
        code = code
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
            .push_number(value)
            .push_address(*target)
            .push_number(100_000_u64)
            .append(CALL)
            .append(POP);
    }
    code.stop().build()
}

fn nested_delta(targets: &[Address]) -> LimitUsage {
    let usage = |value| {
        let db = MemoryDatabase::default()
            .account_balance(A, U256::from(1_000_000))
            .account_balance(SELF_CALLER, U256::from(1_000_000))
            .account_code(SELF_CALLER, self_calling_code(targets, value));
        let (result, usage) =
            run(db, call_with_data(A, SELF_CALLER, Bytes::from_static(&[1]), value_call_room()));
        assert!(result.result.is_success());
        usage
    };
    usage(1).saturating_sub(usage(0))
}

/// After a transfer to another account recorded the frame's account, a transfer to itself adds
/// nothing.
#[test]
fn test_nested_self_transfer_after_recorded_sender_counts_nothing() {
    assert_eq!(
        nested_delta(&[B, SELF_CALLER]),
        with_extra(records_only(2), TRANSFER_LOG_SIZE),
        "the transfer to B and its log"
    );
}

/* ---------- storage, logs, selfdestruct ---------- */

/// A slot's first change is a record; writing it back to its original value takes it back.
#[test]
fn test_sstore_first_change_records_and_write_back_refunds() {
    type Writes = &'static [(u64, u64)];
    let cases: [(Writes, u64, LimitUsage); 4] = [
        (&[(0, 1)], 0, records(1)),
        (&[(0, 1), (0, 2)], 0, records(1)),
        (&[(0, 1), (0, 0)], 0, body_only()),
        (&[(0, 0)], 5, records(1)),
    ];
    for (writes, original, expected) in cases {
        let mut code = BytecodeBuilder::default();
        for (slot, value) in writes {
            code = code.sstore(U256::from(*slot), U256::from(*value));
        }
        let db = MemoryDatabase::default()
            .account_code(CALLEE, code.stop().build())
            .account_storage(CALLEE, U256::ZERO, U256::from(original));
        let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
        assert!(result.result.is_success());
        assert_eq!(usage, expected, "writes {writes:?} over original {original}");
    }
}

/// A write-back in a child refunds the record its caller made, and the refund dies with a
/// failing child.
#[test]
fn test_write_back_in_a_child_refunds_the_caller_record() {
    let child = BytecodeBuilder::default().sstore(U256::ZERO, U256::ZERO).stop().build();
    let failing_child =
        BytecodeBuilder::default().sstore(U256::ZERO, U256::ZERO).append(INVALID).build();
    for (code, expected) in [(child, body_only()), (failing_child, records(1))] {
        // CALLEE writes slot 0, then DELEGATECALLs CONTRACT, which writes it back in CALLEE's
        // storage.
        let parent = BytecodeBuilder::default()
            .sstore(U256::ZERO, U256::from(1))
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(CONTRACT)
            .append(GAS)
            .append(revm::bytecode::opcode::DELEGATECALL)
            .append(POP)
            .stop()
            .build();
        let db =
            MemoryDatabase::default().account_code(CALLEE, parent).account_code(CONTRACT, code);
        let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
        assert!(result.result.is_success());
        assert_eq!(usage, expected);
    }
}

/// `SELFDESTRUCT` records the accounts the move changes: the destructed account, whose balance it
/// clears, and the beneficiary, unless that is the sender, whose account validation changed. A
/// move to the sender still leaves its transfer log; a destruction to itself, of an account the
/// transaction did not create, moves nothing and leaves none, and so does one of an account with
/// no balance.
///
/// Before the net-change rule the destructed account was never recorded, only the beneficiary.
#[test]
fn test_selfdestruct_records_the_beneficiary_when_value_moves() {
    let destruct_to = |beneficiary: Address| {
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build()
    };
    let cases = [
        (U256::from(5), CONTRACT, with_extra(records(2), TRANSFER_LOG_SIZE)),
        (U256::from(5), CALLEE, body_only()),
        (U256::from(5), CALLER, with_extra(records(1), TRANSFER_LOG_SIZE)),
        (U256::ZERO, CONTRACT, body_only()),
    ];
    for (balance, beneficiary, expected) in cases {
        let db = MemoryDatabase::default()
            .account_code(CALLEE, destruct_to(beneficiary))
            .account_balance(CALLEE, balance);
        let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
        assert!(result.result.is_success());
        assert_eq!(usage, expected, "balance {balance}, beneficiary {beneficiary}");
    }
}

/// A top-level creation records the created account, and nothing when it fails.
#[test]
fn test_top_level_create_records_the_created_account() {
    // `STOP` is one byte of init code and deposits nothing.
    let (result, usage) = run(
        MemoryDatabase::default(),
        create(CALLER, Bytes::from_static(&[STOP]), value_call_room()),
    );
    assert!(result.result.is_success());
    assert_eq!(usage, with_extra(records(1), 1));

    let (result, usage) = run(
        MemoryDatabase::default(),
        create(CALLER, Bytes::from_static(&[INVALID]), value_call_room()),
    );
    assert!(!result.result.is_success());
    assert_eq!(usage, with_extra(body_only(), 1));
}

/* ---------- the fee accounts and the sender ---------- */

/// The block beneficiary of the fee-account rows: an account of its own, not the zero address.
const COINBASE: Address = address!("00000000000000000000000000000000000C0B00");

/// How execution reaches an account in the rows of
/// [`test_execution_writes_to_the_fee_accounts_are_records`].
#[derive(Clone, Copy, Debug)]
enum FeeRowPath {
    /// `CALLEE` sends it one wei with a `CALL`.
    ValueTransfer,
    /// `CALLEE` destructs itself naming it as beneficiary, which moves `CALLEE`'s balance to it.
    SelfDestruct,
}

/// A write execution makes to one of the four accounts a transaction's fees are credited to is a
/// record like a write to any other account: the body's five fixed records bound the fee credits
/// the settlement makes, not what the transaction's frames write. The sender's account is the
/// converse: the body counts it, so nothing execution writes to it is recorded again, and that
/// holds for a fee account that is the sender too.
///
/// Every row runs from a contract frame, `CALLEE`, which the transaction calls without value and
/// which holds ten million wei. A value transfer records its sender, `CALLEE`, and its recipient;
/// a `SELFDESTRUCT` of an account the transaction did not create records the account it clears,
/// `CALLEE`, and its beneficiary.
/// Each row moves value to another account, so each keeps one EIP-7708 transfer log, which is
/// data size and neither a record nor history. The fee accounts exist before the transaction, so
/// no row adds state; the transaction pays a fee at a gas price of ten over a base fee of one, so
/// the settlement credits the beneficiary and the base fee vault on top of what execution wrote.
///
/// A record weighs `WRITE_RECORD_SIZE` bytes of data size and history and is one KV update; the
/// history is its bytes at the cost per history byte, on top of the body's `TX_BODY_SIZE`.
///
/// Rules: [S10.17], [S10.19], [S10.22], [S10.28], [S10.29], [S7], [S10]. Independence: constants —
/// the record counts are written out per row, and the data size, KV count and history are hand
/// arithmetic from `TX_BODY_SIZE`, `WRITE_RECORD_SIZE`, `TRANSFER_LOG_SIZE` and the cost per
/// history byte in effect.
#[test]
fn test_execution_writes_to_the_fee_accounts_are_records() {
    use op_revm::constants::{BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT};
    use FeeRowPath::{SelfDestruct, ValueTransfer};

    // (row, the transaction's sender, the account execution writes, the path, the records kept)
    let rows: [(&str, Address, Address, FeeRowPath, u64); 12] = [
        ("the block beneficiary, by a value transfer", CALLER, COINBASE, ValueTransfer, 2),
        ("the block beneficiary, by a SELFDESTRUCT", CALLER, COINBASE, SelfDestruct, 2),
        ("the L1 fee vault, by a value transfer", CALLER, L1_FEE_RECIPIENT, ValueTransfer, 2),
        ("the L1 fee vault, by a SELFDESTRUCT", CALLER, L1_FEE_RECIPIENT, SelfDestruct, 2),
        ("the base fee vault, by a value transfer", CALLER, BASE_FEE_RECIPIENT, ValueTransfer, 2),
        ("the base fee vault, by a SELFDESTRUCT", CALLER, BASE_FEE_RECIPIENT, SelfDestruct, 2),
        (
            "the operator fee vault, by a value transfer",
            CALLER,
            OPERATOR_FEE_RECIPIENT,
            ValueTransfer,
            2,
        ),
        (
            "the operator fee vault, by a SELFDESTRUCT",
            CALLER,
            OPERATOR_FEE_RECIPIENT,
            SelfDestruct,
            2,
        ),
        ("the sender, by a value transfer", CALLER, CALLER, ValueTransfer, 1),
        ("the sender, by a SELFDESTRUCT", CALLER, CALLER, SelfDestruct, 1),
        (
            "the block beneficiary that is the sender, by a value transfer",
            COINBASE,
            COINBASE,
            ValueTransfer,
            1,
        ),
        (
            "the block beneficiary that is the sender, by a SELFDESTRUCT",
            COINBASE,
            COINBASE,
            SelfDestruct,
            1,
        ),
    ];
    for (row, sender, target, path, records) in rows {
        let code = match path {
            ValueTransfer => append_value_call(BytecodeBuilder::default(), target, 1)
                .append(POP)
                .append(STOP)
                .build(),
            SelfDestruct => {
                BytecodeBuilder::default().push_address(target).append(SELFDESTRUCT).build()
            }
        };
        let mut db = funded().account_code(CALLEE, code).account_balance(sender, U256::MAX >> 2);
        for account in [COINBASE, L1_FEE_RECIPIENT, BASE_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT] {
            if account != sender {
                db = db.account_balance(account, U256::from(1));
            }
        }
        let mut tx = call(sender, CALLEE, U256::ZERO, value_call_room());
        tx.0.base.gas_price = 10;
        let block =
            revm::context::BlockEnv { beneficiary: COINBASE, basefee: 1, ..crate::common::block() };
        let outcome = MegaEvm::new(context(db).with_block(block))
            .execute_transaction(tx)
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{row}: {:?}", outcome.result);
        assert_eq!(transfer_logs(&outcome.result), 1, "{row}: the transfer log");
        // The settlement credited the fees on top of what execution moved: the priority fee of
        // nine a gas to the beneficiary, the base fee of one a gas to the base fee vault.
        for (credited, per_gas) in [(COINBASE, 9u64), (BASE_FEE_RECIPIENT, 1)] {
            if credited == sender {
                continue;
            }
            let moved = match path {
                _ if credited != target => 0,
                ValueTransfer => 1,
                SelfDestruct => 10_000_000,
            };
            assert_eq!(
                outcome.state[&credited].info.balance,
                U256::from(1 + moved + outcome.gas.gas_used * per_gas),
                "{row}: the fee credited to {credited}"
            );
        }

        let history_bytes = TX_BODY_SIZE + records * WRITE_RECORD_SIZE;
        assert_eq!(outcome.usage.write_records, records, "{row}: the records, the KV count");
        assert_eq!(
            outcome.usage.data_size,
            history_bytes + TRANSFER_LOG_SIZE,
            "{row}: the data size: the body, the records and the transfer log"
        );
        assert_eq!(outcome.gas.history_bytes, history_bytes, "{row}: the history bytes");
        if !crate::common::history_rounds() {
            assert_eq!(
                outcome.gas.history,
                history(history_bytes),
                "{row}: the history gas: the body and the records at the cost per history byte"
            );
        }
    }
}

/* ---------- the commit happens after the opcode ---------- */

/// Records, after each opcode, what the layer holds.
#[derive(Default)]
struct LayerProbe {
    /// `(opcode, usage)` after each opcode.
    steps: Vec<(u8, LimitUsage)>,
    opcode: u8,
    /// Appends a log through the Host before every `SSTORE`, as a tool's write.
    log_before_sstore: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for LayerProbe {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut MegaContext<DB>) {
        self.opcode = interp.bytecode.opcode();
        if self.log_before_sstore && self.opcode == SSTORE {
            let before = context.limit_usage();
            let log = Log {
                address: CALLEE,
                data: LogData::new(vec![B256::ZERO], Bytes::new()).unwrap(),
            };
            revm::context_interface::Host::log(context, log);
            assert_eq!(context.limit_usage(), before, "a tool's log counts nothing");
        }
    }

    fn step_end(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<DB>,
    ) {
        self.steps.push((self.opcode, context.limit_usage()));
    }
}

fn probe(db: MemoryDatabase, gas_limit: u64, log_before_sstore: bool) -> Vec<(u8, LimitUsage)> {
    let mut evm = MegaEvm::new(context(db))
        .with_inspector(LayerProbe { log_before_sstore, ..Default::default() });
    alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, CALLEE, U256::ZERO, gas_limit)).unwrap();
    evm.inspector().steps.clone()
}

fn after_sstore(steps: &[(u8, LimitUsage)]) -> LimitUsage {
    steps.iter().find(|(op, _)| *op == SSTORE).map(|(_, usage)| *usage).unwrap()
}

/// A completed `SSTORE` commits its record by the time the opcode ends.
#[test]
fn test_completed_sstore_commits_its_record() {
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    let steps =
        probe(MemoryDatabase::default().account_code(CALLEE, code), value_call_room(), false);
    assert_eq!(after_sstore(&steps), records(1));
}

/// An `SSTORE` that runs out of gas after its Host call wrote the slot counts nothing: the
/// failure takes the write back, and revm offers the completed-write hook nothing.
#[test]
fn test_sstore_out_of_gas_after_the_write_counts_nothing() {
    // CALLEE calls CONTRACT with 5,000 gas: past the EIP-2200 stipend check, so the Host call
    // writes the slot, but short of the 22,100 a cold first write charges after it.
    let child = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    let parent = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT)
        .push_number(5_000_u64)
        .append(CALL)
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(CALLEE, parent).account_code(CONTRACT, child);
    let steps = probe(db, value_call_room(), false);
    assert_eq!(after_sstore(&steps), body_only(), "a failed opcode's write is not counted");
}

/// A log a tool appends through the Host, outside an opcode — here an inspector's, in its `step`
/// before an `SSTORE` — counts nothing, and the `SSTORE` that follows counts its own record alone.
#[test]
fn test_a_tools_log_in_step_counts_nothing() {
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    let steps =
        probe(MemoryDatabase::default().account_code(CALLEE, code), value_call_room(), true);
    assert_eq!(after_sstore(&steps), records(1), "the SSTORE's record, and not the tool's log");
}

/* ---------- applied EIP-7702 authorities ---------- */

const DELEGATE: Address = address!("00000000000000000000000000000000DE1E0001");
const AUTHORITY_1: Address = address!("00000000000000000000000000000000A0717001");
const AUTHORITY_2: Address = address!("00000000000000000000000000000000A0717002");

/// A type-4 transaction from `CALLER` to `to` with value `value`, carrying authorizations
/// `(authority, nonce)` that delegate to `DELEGATE`.
fn authorizing_call(
    to: Address,
    value: u64,
    authorizations: &[(Address, u64)],
) -> mega_evm::MegaTransaction {
    crate::common::authorizing_call(
        CALLER,
        to,
        U256::from(value),
        value_call_room(),
        DELEGATE,
        authorizations,
    )
}

/// Each distinct applied authority is one record, however many of its authorizations applied;
/// an authorization that does not apply (a wrong nonce) records nothing.
#[test]
fn test_applied_authorities_record_once_each() {
    let db = || MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
    let (result, usage) = run(
        db(),
        authorizing_call(CONTRACT, 0, &[(AUTHORITY_1, 0), (AUTHORITY_2, 0), (AUTHORITY_1, 1)]),
    );
    assert!(result.result.is_success(), "{:?}", result.result);
    // Three authorization tuples travel in the body; two distinct authorities are applied.
    assert_eq!(usage, with_extra(records(2), 3 * mega_evm::AUTHORIZATION_SIZE));

    let (result, usage) = run(db(), authorizing_call(CONTRACT, 0, &[(AUTHORITY_1, 7)]));
    assert!(result.result.is_success());
    assert_eq!(
        usage,
        with_extra(body_only(), mega_evm::AUTHORIZATION_SIZE),
        "a wrong nonce applies nothing, and its tuple is still in the body"
    );
}

/// A value transfer to an applied authority records the authority once, not again as the
/// recipient.
#[test]
fn test_value_to_an_applied_authority_records_it_once() {
    let db = || MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
    let (result, usage) = run(db(), authorizing_call(AUTHORITY_1, 1, &[(AUTHORITY_1, 0)]));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(
        usage,
        with_extra(records(1), mega_evm::AUTHORIZATION_SIZE + TRANSFER_LOG_SIZE),
        "the authority, its authorization and the transfer log"
    );

    // And is charged for once: the transaction's own frame writes no recipient, because the
    // authority's account is already written. A transfer to an account that is not an authority
    // pays for the recipient on top of the one authority it applied.
    if crate::common::runs_at_measurement_prices() {
        return;
    }
    const CPHB: u64 = mega_evm::constants::COST_PER_HISTORY_BYTE;
    let to_authority =
        crate::common::execute(db(), authorizing_call(AUTHORITY_1, 1, &[(AUTHORITY_1, 0)]));
    let to_another =
        crate::common::execute(db(), authorizing_call(CONTRACT, 1, &[(AUTHORITY_1, 0)]));
    let body = mega_evm::TX_BODY_SIZE + mega_evm::AUTHORIZATION_SIZE;

    assert!(to_another.result.is_success(), "{:?}", to_another.result);
    assert_eq!(
        to_authority.gas.history,
        (body + WRITE_RECORD_SIZE) * CPHB,
        "the authority's record, and no recipient record beside it",
    );
    assert_eq!(
        to_another.gas.history,
        (body + 2 * WRITE_RECORD_SIZE) * CPHB,
        "the authority's record and the recipient's",
    );
}

/// A frame running as the transaction's sender (a delegated sender called back) records no
/// write to the sender's account: the transaction body counts it.
#[test]
fn test_frames_running_as_the_sender_do_not_record_it() {
    // CALLER is delegated to CONTRACT, whose code sends value to CONTRACT2. CALLEE calls CALLER.
    let relay = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CALLER)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let sender_code = append_value_call(BytecodeBuilder::default(), CONTRACT2, 1)
        .append(POP)
        .append(STOP)
        .build();
    let mut db = funded().account_code(CALLEE, relay).account_code(CONTRACT, sender_code);
    let delegation = revm::state::Bytecode::new_eip7702(CONTRACT);
    let account = db.load_account(CALLER).unwrap();
    account.info.code_hash = delegation.hash_slow();
    account.info.code = Some(delegation);
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.state[&CONTRACT2].info.balance, U256::from(1), "the transfer happened");
    assert_eq!(
        usage,
        with_extra(records(1), TRANSFER_LOG_SIZE),
        "CONTRACT2 and the transfer log only; the sender is part of the body"
    );
}

/// Raises the endowment of every creation past what the creator holds.
struct OverfundCreations;

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for OverfundCreations {
    fn create(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &mut revm::interpreter::CreateInputs,
    ) -> Option<revm::interpreter::CreateOutcome> {
        inputs.set_value(U256::MAX);
        None
    }
}

/// A creation that fails before bumping the creator's nonce (here, an endowment an inspector
/// raised past the creator's balance) takes its creator record back with it.
#[test]
fn test_creation_failing_before_the_nonce_bump_records_nothing() {
    let code = append_value_create(BytecodeBuilder::default()).append(POP).append(STOP).build();
    let mut evm = MegaEvm::new(context(funded().account_code(CALLEE, code)))
        .with_inspector(OverfundCreations);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, CALLEE, U256::ZERO, value_call_room()))
            .unwrap();
    assert!(result.result.is_success());
    assert_eq!(result.state[&CALLEE].info.nonce, 0, "the nonce was never bumped");
    assert_eq!(evm.ctx().limit_usage(), body_only());
}

/// A creation that collides with an existing account fails after bumping the creator's nonce, so
/// the creator record stays and the created account's goes.
#[test]
fn test_colliding_creation_keeps_the_creator_record() {
    let code = append_value_create(BytecodeBuilder::default()).append(POP).append(STOP).build();
    let db = funded().account_code(CALLEE, code).account_nonce(CALLEE.create(0), 1);
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
    assert!(result.result.is_success());
    assert_eq!(result.state[&CALLEE].info.nonce, 1, "the collision comes after the bump");
    assert_eq!(usage, records(1), "the creator's nonce only");
}

/// A creation a frame budget stops before it runs reverts like one that ran: the creator's nonce
/// is bumped and its record kept, the created account's record goes.
#[test]
fn test_creation_stopped_at_init_bumps_the_creator_nonce() {
    // CALLEE writes a slot (40 bytes of its 100-byte budget), then creates: the creation's two
    // records (80 bytes) exceed the 60 bytes left to it. CALLEE stores the created address.
    let code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .append_many([PUSH0, PUSH0, PUSH0])
        .append(CREATE)
        .append(PUSH0)
        .append(SSTORE)
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(CALLEE, code);
    let limits = mega_evm::EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(100);
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, CALLEE, U256::ZERO, value_call_room()))
            .unwrap();
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.state[&CALLEE].info.nonce, 1, "the stopped creation bumped the nonce");
    assert_eq!(result.state[&CALLEE].storage[&U256::ZERO].present_value(), U256::ZERO, "it failed");
    assert_eq!(evm.ctx().limit_usage(), records(2), "slot 1 and the creator's nonce");
}

/// A transaction is held at validation to its body and a record for every entry of its
/// authorization list, against the data-size limit: the largest transaction that fits runs with
/// its authority applied, and with one byte of limit less it is rejected before anything is
/// applied. The bound counts every entry, applied or not: a list naming one authority twice is held
/// to two records, though the one authority makes one.
#[test]
fn test_authority_records_are_held_to_the_limit_at_validation() {
    let db = || MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
    let with_cap = |cap, authorizations: &[(Address, u64)]| {
        let limits = mega_evm::EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(cap);
        MegaEvm::new(context(db()).with_tx_runtime_limits(limits))
            .execute_transaction(authorizing_call(CONTRACT, 0, authorizations))
    };
    let refused = |cap, authorizations: &[(Address, u64)]| {
        let error = with_cap(cap, authorizations).expect_err("rejected at validation");
        format!("{error:?}").contains(mega_evm::TX_BODY_OVER_DATA_SIZE_LIMIT)
    };
    // The body carries the authorization tuple; the applied authority is one record on top.
    let tuple = mega_evm::AUTHORIZATION_SIZE;
    let once = [(AUTHORITY_1, 0)];
    let largest = TX_BODY_SIZE + tuple + WRITE_RECORD_SIZE;
    let at_the_cap = with_cap(largest, &once).expect("a transaction at the limit is valid");
    assert!(at_the_cap.result.is_success(), "{:?}", at_the_cap.result);
    assert_eq!(at_the_cap.usage, with_extra(records(1), tuple));
    let code = at_the_cap.state[&AUTHORITY_1].info.code.as_ref();
    assert_eq!(code.and_then(|code| code.eip7702_address()), Some(DELEGATE), "it is applied");
    assert!(refused(largest - 1, &once), "one byte of limit less rejects it");

    let twice = [(AUTHORITY_1, 0), (AUTHORITY_1, 1)];
    let bound = TX_BODY_SIZE + 2 * tuple + 2 * WRITE_RECORD_SIZE;
    assert!(refused(bound - 1, &twice), "two entries are held to two records");
    let ran = with_cap(bound, &twice).expect("valid at two records");
    assert!(ran.result.is_success(), "{:?}", ran.result);
    assert_eq!(ran.usage, with_extra(records(1), 2 * tuple), "the one authority makes one record");
}

/// A funded caller's database, for the runtime out-of-gas tests.
fn runtime_oog_db() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000))
}

/// A call to an authority the transaction delegates, at `gas_limit`: calling it pays for the
/// delegation target's access after the authorities were applied.
fn runtime_oog_tx(gas_limit: u64) -> mega_evm::MegaTransaction {
    let mut tx = authorizing_call(AUTHORITY_1, 0, &[(AUTHORITY_1, 0)]);
    tx.0.base.gas_limit = gas_limit;
    tx
}

/// The gas limit one short of [`runtime_oog_tx`]'s whole runtime phase: the transaction is
/// included, its authorities are applied, and it runs out of gas before its first frame.
fn one_short_of_the_runtime_phase() -> u64 {
    let run_at = |gas_limit| {
        MegaEvm::new(context(runtime_oog_db())).execute_transaction(runtime_oog_tx(gas_limit))
    };
    // A limit below the intrinsic charge is a validation rejection rather than a run, which is a
    // failure for the search all the same.
    let succeeds_at = |gas_limit| run_at(gas_limit).is_ok_and(|o| o.result.is_success());
    // Gas is monotone here: every limit above the smallest one that succeeds succeeds too. Search
    // for that smallest limit rather than walking up to it, which the delegation's state gas puts
    // several hundred thousand gas away.
    // 1,000,000 of regular gas on top of the authority's account and delegation and a kilobyte of
    // history, at the byte prices in effect.
    let high = 1_000_000 + 2 * account_state_gas() + history(1_000);
    let (mut fails, mut succeeds) = (0u64, high);
    assert!(succeeds_at(succeeds), "the high bound must succeed");
    while succeeds - fails > 1 {
        let middle = fails + (succeeds - fails) / 2;
        if succeeds_at(middle) {
            succeeds = middle;
        } else {
            fails = middle;
        }
    }
    succeeds - 1
}

/// A transaction that runs out of gas after its authorities were applied, before its first frame,
/// keeps none of their records: the out-of-gas takes the delegations back.
#[test]
fn test_runtime_out_of_gas_after_authorities_keeps_no_record() {
    let outcome = MegaEvm::new(context(runtime_oog_db()))
        .execute_transaction(runtime_oog_tx(one_short_of_the_runtime_phase()))
        .expect("one gas short of succeeding is still included");
    assert!(outcome.result.is_halt(), "{:?}", outcome.result);
    assert_eq!(
        outcome.usage,
        with_extra(body_only(), mega_evm::AUTHORIZATION_SIZE),
        "the out-of-gas takes the authority record back and keeps the body"
    );
    assert_eq!(outcome.limit_exceeded, None);
    assert!(
        outcome.state.get(&AUTHORITY_1).is_none_or(|a| a.info.nonce == 0),
        "the out-of-gas took the delegation back"
    );
}

/// The inspected path unwinds a transaction that runs out of gas before its first frame as the
/// plain one does: the delegation is taken back, and the result, the ledgers, the usage and the
/// state are the same.
#[test]
fn test_an_inspected_runtime_out_of_gas_unwinds_as_a_plain_one() {
    let gas_limit = one_short_of_the_runtime_phase();
    let plain = MegaEvm::new(context(runtime_oog_db()))
        .execute_transaction(runtime_oog_tx(gas_limit))
        .expect("included");
    let inspected = MegaEvm::new(context(runtime_oog_db()))
        .with_inspector(revm::inspector::NoOpInspector)
        .execute_transaction(runtime_oog_tx(gas_limit))
        .expect("included");
    assert!(inspected.result.is_halt(), "{:?}", inspected.result);
    assert!(
        inspected.state.get(&AUTHORITY_1).is_none_or(|a| a.info.nonce == 0),
        "the out-of-gas took the delegation back"
    );
    assert_eq!(inspected.result, plain.result);
    assert_eq!(inspected.gas, plain.gas);
    assert_eq!(inspected.usage, plain.usage);
    assert_eq!(inspected.state, plain.state);
}

/* ---------- whose account a frame's writes find changed ---------- */

const D: Address = address!("00000000000000000000000000000000000000D1");

/// A child that received value had its own account recorded by the transfer; passing all of it
/// on returns the account to what the transaction loaded and takes that record back, so what
/// stands is the sender's and the last recipient's.
///
/// Before the net-change rule the child's account stayed recorded once it was.
#[test]
fn test_value_receiving_child_records_itself_once() {
    let a =
        append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP).append(STOP).build();
    let b = append_value_call(BytecodeBuilder::default(), CONTRACT2, 1)
        .append(POP)
        .append(STOP)
        .build();
    let db = funded().account_code(CALLEE, a).account_code(CONTRACT, b);
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, value_call_room()));
    assert!(result.result.is_success());
    assert_eq!(result.state[&CONTRACT2].info.balance, U256::from(1));
    assert_eq!(result.state[&CONTRACT].info.balance, U256::ZERO, "as it was loaded");
    assert_eq!(
        usage,
        with_extra(records(2), 2 * TRANSFER_LOG_SIZE),
        "CALLEE, CONTRACT2, and two transfer logs: CONTRACT passed on what it received"
    );
}

/// A child that received nothing has its own account unchanged, whatever its caller's state: its
/// first value transfer records it.
#[test]
fn test_zero_value_child_starts_unrecorded() {
    let a = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let b = append_value_call(BytecodeBuilder::default(), CONTRACT2, 1)
        .append(POP)
        .append(STOP)
        .build();
    let db = funded()
        .account_code(CALLEE, a)
        .account_code(CONTRACT, b)
        .account_balance(CONTRACT, U256::from(5));
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::from(1), value_call_room()));
    assert!(result.result.is_success());
    assert_eq!(
        usage,
        with_extra(records(3), 2 * TRANSFER_LOG_SIZE),
        "CALLEE as the recipient, CONTRACT, CONTRACT2, and two transfer logs"
    );
}

/// Calls `D` with `DELEGATECALL`.
fn delegate_to_d() -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(D)
        .append(GAS)
        .append(revm::bytecode::opcode::DELEGATECALL)
        .append(POP)
        .stop()
        .build()
}

/// A `DELEGATECALL` runs as its caller's account, so a transfer it makes writes that account as
/// the caller left it: changed by the value it received, and returned to what the transaction
/// loaded when the delegate passes that value on, which takes the caller's record back.
///
/// Before the net-change rule the caller's account stayed recorded once it was.
#[test]
fn test_delegatecall_child_inherits_the_frame_account_record() {
    let d = append_value_call(BytecodeBuilder::default(), CONTRACT2, 1)
        .append(POP)
        .append(STOP)
        .build();
    let db = funded().account_code(CALLEE, delegate_to_d()).account_code(D, d);
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::from(1), value_call_room()));
    assert!(result.result.is_success());
    assert_eq!(result.state[&CONTRACT2].info.balance, U256::from(1), "CALLEE's value moved");
    assert_eq!(
        usage,
        with_extra(records(1), 2 * TRANSFER_LOG_SIZE),
        "CONTRACT2 and two transfer logs: CALLEE passed on the value it received"
    );
}

/// A created frame's account is recorded by the creation; a transfer a `DELEGATECALL` from its
/// init code makes finds it changed.
#[test]
fn test_created_frame_delegatecall_inherits_the_created_account_record() {
    let d = append_value_call(BytecodeBuilder::default(), CONTRACT2, 1)
        .append(POP)
        .append(STOP)
        .build();
    let db = funded().account_code(D, d);
    let tx = {
        let mut tx = create(CALLER, delegate_to_d(), value_call_room());
        tx.0.base.value = U256::from(5);
        tx
    };
    let (result, usage) = run(db, tx);
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.state[&CONTRACT2].info.balance, U256::from(1));
    // The init code travels in the body. `STOP` deploys nothing.
    assert_eq!(
        usage,
        with_extra(records(2), delegate_to_d().len() as u64 + 2 * TRANSFER_LOG_SIZE),
        "the created account, CONTRACT2, and two transfer logs"
    );
}

/// A transaction calling its own sender (delegated) finds the sender's account changed from the
/// start, by its fee and its nonce: a transfer from it records the recipient alone.
#[test]
fn test_top_level_self_call_counts_the_sender_as_recorded() {
    let sender_code = append_value_call(BytecodeBuilder::default(), CONTRACT2, 1)
        .append(POP)
        .append(STOP)
        .build();
    let mut db = funded().account_code(CONTRACT, sender_code);
    let delegation = revm::state::Bytecode::new_eip7702(CONTRACT);
    let account = db.load_account(CALLER).unwrap();
    account.info.code_hash = delegation.hash_slow();
    account.info.code = Some(delegation);
    let (result, usage) = run(db, call(CALLER, CALLER, U256::ZERO, value_call_room()));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.state[&CONTRACT2].info.balance, U256::from(1));
    assert_eq!(usage, with_extra(records(1), TRANSFER_LOG_SIZE), "CONTRACT2 and its log only");
}

/// Answers every call to `CONTRACT` itself.
struct AnswerContract;

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for AnswerContract {
    fn call(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &mut revm::interpreter::CallInputs,
    ) -> Option<revm::interpreter::CallOutcome> {
        (inputs.target_address == CONTRACT).then(|| {
            revm::interpreter::CallOutcome::new(
                revm::interpreter::InterpreterResult::new(
                    revm::interpreter::InstructionResult::Stop,
                    Bytes::new(),
                    revm::interpreter::Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            )
        })
    }
}

/// A call an inspector answers gets an empty lane of its own, so its caller's lane is intact for
/// what the caller does next.
#[test]
fn test_inspector_answered_call_keeps_the_caller_lane() {
    let a = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT)
        .append(GAS)
        .append(CALL)
        .append(POP);
    let a = append_value_call(a, CONTRACT2, 1).append(POP).append(STOP).build();
    let mut evm =
        MegaEvm::new(context(funded().account_code(CALLEE, a))).with_inspector(AnswerContract);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, CALLEE, U256::ZERO, value_call_room()))
            .unwrap();
    assert!(result.result.is_success());
    assert_eq!(
        evm.ctx().limit_usage(),
        with_extra(records(2), TRANSFER_LOG_SIZE),
        "CALLEE and CONTRACT2, and the transfer log"
    );
}

/* ---------- every kept write is paid for, once ---------- */

/// One transaction of the corpus below: what it runs, the history bytes it appends beyond its
/// body that are not write records, and the records it keeps.
struct Paired {
    name: &'static str,
    db: MemoryDatabase,
    tx: mega_evm::MegaTransaction,
    /// Log and deployed-code bytes. The calldata travels in the body, so it is not counted here.
    other_bytes: u64,
    /// The write records the transaction keeps, beyond the ones its body already carries.
    records: u64,
    /// Data-size bytes that are not history: an Oracle hint's payload, which goes to the node's
    /// oracle service rather than into a block. Zero for every case but the hint's.
    hint_bytes: u64,
    /// The EIP-7708 transfer logs the transaction keeps, one per value movement it kept: data
    /// size that is not history, and no record.
    transfer_logs: u64,
    /// The block this case runs in, for a case whose beneficiary is the point of it.
    beneficiary: Address,
}

/// A call to `MegaAccessControl` carrying one wei, which its interceptor refuses before a frame
/// runs, followed by a `STOP`.
fn value_call_a_system_contract_refuses() -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR)
        .append_many([PUSH0, PUSH0])
        .push_number(4u64)
        .append(PUSH0)
        .append(PUSH1)
        .append(1u8)
        .push_address(ACCESS_CONTROL_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// Init code that returns `size` zero bytes as the deployed contract.
fn constructor_returning(size: u8) -> Bytes {
    BytecodeBuilder::default().push_number(size).push_number(0_u8).append(RETURN).build()
}

/// Init code that reverts with `len` bytes of data and deploys nothing.
fn reverting_with(len: u8) -> Bytes {
    BytecodeBuilder::default().push_number(len).push_number(0_u8).append(REVERT).build()
}

/// The corpus, one case per rule the record sites follow.
fn paired_corpus() -> Vec<Paired> {
    let case = |name, db: MemoryDatabase, tx, other_bytes, records| Paired {
        name,
        db,
        tx,
        other_bytes,
        records,
        hint_bytes: 0,
        transfer_logs: 0,
        beneficiary: Address::ZERO,
    };
    let to_callee = |code: Bytes| funded().account_code(CALLEE, code);
    let plain = || call(CALLER, CALLEE, U256::ZERO, value_call_room());
    let calling = |target, value| {
        append_value_call(BytecodeBuilder::default(), target, value)
            .append(POP)
            .append(STOP)
            .build()
    };
    let storing = |slots: &[(u64, u64)]| {
        let mut code = BytecodeBuilder::default();
        for (slot, value) in slots {
            code = code.sstore(U256::from(*slot), U256::from(*value));
        }
        code.stop().build()
    };
    let reverting = Bytes::from_static(&[PUSH0, PUSH0, 0xFD]);
    vec![
        case("an empty call", to_callee(Bytes::new()), plain(), 0, 0),
        case("three fresh slots", to_callee(storing(&[(1, 1), (2, 1), (3, 1)])), plain(), 0, 3),
        case("a slot written and restored", to_callee(storing(&[(1, 1), (1, 0)])), plain(), 0, 0),
        case("zero written to an empty slot", to_callee(storing(&[(1, 0)])), plain(), 0, 0),
        case(
            "a slot rewritten twice",
            to_callee(storing(&[(1, 1), (1, 2), (1, 3)])),
            plain(),
            0,
            1,
        ),
        Paired {
            transfer_logs: 1,
            ..case("a value transfer", to_callee(calling(CONTRACT, 1)), plain(), 0, 2)
        },
        case(
            "a value transfer the callee reverts",
            to_callee(calling(CONTRACT, 1)).account_code(CONTRACT, reverting.clone()),
            plain(),
            0,
            0,
        ),
        // Its sender is its recipient: nothing moves, no record, and no transfer log.
        case(
            "a value call to the calling account itself",
            to_callee(self_calling_code(&[CALLEE], 1)),
            call_with_data(CALLER, CALLEE, Bytes::from_static(&[1]), value_call_room()),
            0,
            0,
        ),
        case(
            "a log over one word",
            to_callee(
                BytecodeBuilder::default()
                    .push_number(1u64)
                    .push_number(1u64)
                    .push_number(32u64)
                    .push_number(0u64)
                    .append(LOG2)
                    .append(STOP)
                    .build(),
            ),
            plain(),
            32 + 2 * 32 + 32,
            0,
        ),
        Paired {
            transfer_logs: 1,
            ..case(
                "a nested creation",
                to_callee(
                    append_value_create(BytecodeBuilder::default())
                        .append(POP)
                        .append(STOP)
                        .build(),
                ),
                plain(),
                0,
                2,
            )
        },
        case(
            "a creation whose init code reverts",
            funded().account_code(OUTER_CREATOR, reverting_creations(1)),
            call(CALLER, OUTER_CREATOR, U256::ZERO, value_call_room()),
            0,
            1,
        ),
        // The account it clears and its beneficiary.
        Paired {
            transfer_logs: 1,
            ..case(
                "a destruction that moves value",
                to_callee(
                    BytecodeBuilder::default().push_address(CONTRACT).append(SELFDESTRUCT).build(),
                )
                .account_balance(CALLEE, U256::from(1)),
                plain(),
                0,
                2,
            )
        },
        case(
            "a value call a system contract's interceptor refuses",
            to_callee(value_call_a_system_contract_refuses())
                .account_balance(ACCESS_CONTROL_ADDRESS, U256::from(1))
                .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE),
            plain(),
            0,
            0,
        ),
        case(
            "a transaction carrying calldata",
            to_callee(Bytes::new()),
            call_with_data(CALLER, CALLEE, Bytes::from(vec![7u8; 100]), value_call_room()),
            0,
            0,
        ),
        Paired {
            transfer_logs: 1,
            ..case(
                "a value transaction",
                funded(),
                call(CALLER, CONTRACT, U256::from(1), value_call_room()),
                0,
                1,
            )
        },
        case(
            "a value transaction the recipient reverts",
            funded().account_code(CONTRACT, reverting),
            call(CALLER, CONTRACT, U256::from(1), value_call_room()),
            0,
            0,
        ),
        case(
            "a creation transaction",
            funded(),
            create(CALLER, Bytes::new(), value_call_room()),
            0,
            1,
        ),
        case(
            "a creation that deploys code",
            funded(),
            create(CALLER, constructor_returning(32), value_call_room()),
            32,
            1,
        ),
        case(
            "a creation whose revert data is not deployed code",
            funded(),
            create(CALLER, reverting_with(10), value_call_room()),
            0,
            0,
        ),
        // Two of the three places where a transaction's data size and its history part company;
        // the third is every transfer log a case above keeps. All three are decisions, stated in
        // the byte table; each is written out here with the number it pays.
        Paired {
            hint_bytes: send_hint(b"a hint the transaction pays data size for").len() as u64,
            ..case(
                "a transaction that sends an Oracle hint",
                funded().account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE),
                call_with_data(
                    CALLER,
                    ORACLE_CONTRACT_ADDRESS,
                    send_hint(b"a hint the transaction pays data size for"),
                    value_call_room(),
                ),
                0,
                0,
            )
        },
        Paired {
            beneficiary: CONTRACT,
            transfer_logs: 1,
            ..case(
                "a value transaction whose recipient is the block beneficiary",
                MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18))),
                {
                    // A fee the beneficiary is actually credited, so the account the transfer
                    // records is one the body's five already bound.
                    let mut tx = call(CALLER, CONTRACT, U256::from(1), value_call_room());
                    tx.0.base.gas_price = 10;
                    tx
                },
                0,
                1,
            )
        },
    ]
}

/// The EIP-7708 transfer logs `result` carries.
fn transfer_logs<H>(result: &revm::context::result::ExecutionResult<H>) -> u64 {
    result.logs().iter().filter(|log| is_transfer_log(log)).count() as u64
}

/// The calldata of `sendHint(topic, data)`, whose payload the Oracle's interceptor counts on the
/// transaction's data size before the contract's own bytecode runs.
fn send_hint(data: &[u8]) -> Bytes {
    Bytes::from(
        IOracle::sendHintCall {
            topic: B256::repeat_byte(0x7a),
            data: Bytes::copy_from_slice(data),
        }
        .abi_encode(),
    )
}

/// Every write a transaction keeps is one forty-byte record of history, and nothing it does not
/// keep is: the history each transaction pays beyond its body, its logs and its deployed code is
/// exactly the write-record count the layer counted, at the history price.
///
/// This is the pairing itself. The layer counts the records and the engine charges for them at
/// separate sites, on separate rules; the corpus holds the two to the same number, case by case,
/// and to the number the case says.
///
/// The pairing is per record, and three things part a transaction's two totals: an Oracle hint's
/// payload is data size that is not history, a transfer to the block beneficiary pays a record the
/// transaction body's five already bound, and an EIP-7708 transfer log — which every case that
/// moves value keeps, and its receipt carries — is data size that is neither history nor a record.
/// All three are the byte table's decisions; each is written out with the number it costs so the
/// divergence reads as intended rather than as a defect.
#[test]
fn test_every_kept_write_pays_one_record_of_history() {
    if crate::common::runs_at_measurement_prices() {
        return;
    }
    const CPHB: u64 = mega_evm::constants::COST_PER_HISTORY_BYTE;
    for case in paired_corpus() {
        let ctx = context(case.db).with_block(revm::context::BlockEnv {
            beneficiary: case.beneficiary,
            ..crate::common::block()
        });
        let outcome = MegaEvm::new(ctx)
            .execute_transaction(case.tx.clone())
            .expect("the transaction is valid");
        let usage = outcome.usage;
        assert_eq!(usage.write_records, case.records, "{}: the records kept", case.name);
        assert_eq!(
            transfer_logs(&outcome.result),
            case.transfer_logs,
            "{}: the transfer logs the receipt carries",
            case.name
        );
        let body = mega_evm::transaction_body_bytes(&case.tx);
        let history_bytes = body + case.other_bytes + case.records * WRITE_RECORD_SIZE;
        assert_eq!(outcome.gas.history, history_bytes * CPHB, "{}: the history it pays", case.name);
        assert_eq!(outcome.gas.history_bytes, history_bytes, "{}: the history bytes", case.name);
        assert_eq!(
            usage.data_size,
            history_bytes + case.hint_bytes + case.transfer_logs * TRANSFER_LOG_SIZE,
            "{}: the data size the limit counts",
            case.name,
        );
    }
}

/// A value call an inspector answers makes none of the writes its caller paid for, so all of that
/// history comes back — whatever the inspector answered with.
///
/// A frame answered without running is the one case where a *successful* result must still give
/// the caller its charge back: the records were never made. An interceptor's refusal and the
/// depth guard both fail, so only an inspector reaches this.
#[test]
fn test_an_inspector_answered_value_call_gives_its_history_back() {
    if crate::common::runs_at_measurement_prices() {
        return;
    }
    let code =
        append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP).append(STOP).build();
    let run_with = |inspector: Option<AnswerContract>| {
        let db = funded().account_code(CALLEE, code.clone());
        let tx = call(CALLER, CALLEE, U256::ZERO, value_call_room());
        match inspector {
            Some(inspector) => {
                let mut evm = MegaEvm::new(context(db)).with_inspector(inspector);
                let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
                assert!(result.result.is_success(), "{:?}", result.result);
                (evm.ctx().limit_usage(), evm.ctx().additional_limit().history_gas_spent())
            }
            None => {
                let outcome = crate::common::execute(db, tx);
                assert!(outcome.result.is_success(), "{:?}", outcome.result);
                (outcome.usage, outcome.gas.history)
            }
        }
    };
    let (answered_usage, answered_history) = run_with(Some(AnswerContract));
    let (ran_usage, ran_history) = run_with(None);
    let body = mega_evm::TX_BODY_SIZE * mega_evm::constants::COST_PER_HISTORY_BYTE;

    assert_eq!(answered_usage, body_only(), "the answered call wrote nothing");
    assert_eq!(answered_history, body, "so its caller pays for nothing beyond its body");
    assert_eq!(
        ran_usage,
        with_extra(records(2), TRANSFER_LOG_SIZE),
        "the transfer that ran wrote two accounts and left its log"
    );
    assert_eq!(
        ran_history,
        body + 2 * WRITE_RECORD_SIZE * mega_evm::constants::COST_PER_HISTORY_BYTE
    );
}

/* ---------- the KV count and the data size ---------- */

/// Watches the usage the layer counts after every instruction, and keeps the first one whose
/// write records weigh more than its data size.
#[derive(Default)]
struct KvWeighsNoMore {
    steps: usize,
    over: Option<LimitUsage>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for KvWeighsNoMore {
    fn step_end(&mut self, _: &mut Interpreter<EthInterpreter>, context: &mut MegaContext<DB>) {
        self.steps += 1;
        let usage = context.limit_usage();
        if usage.write_records * WRITE_RECORD_SIZE > usage.data_size {
            self.over.get_or_insert(usage);
        }
    }
}

/// The KV count a node reads is the write-record count, and it never weighs more than the data
/// size: every record is forty bytes of it, counted with it and taken back with it. Across the
/// corpus — slots written back, a slot rewritten, accounts recorded once per frame, failed
/// children, a hint — `KV × 40 ≤ data size` holds after every instruction a transaction runs, and
/// what it keeps parts from the records by exactly the bytes that are not records: the body, the
/// logs — the transfer logs among them — the deployed code and the hint's payload.
///
/// So the data-size limit bounds the records: at the production data-size caps a transaction keeps
/// at most 327,680 of them.
#[test]
fn test_the_kv_count_never_weighs_more_than_the_data_size() {
    assert_eq!(mega_evm::constants::TX_DATA_LIMIT / WRITE_RECORD_SIZE, 327_680);
    let mut ran = 0;
    for case in paired_corpus() {
        let ctx = context(case.db).with_block(revm::context::BlockEnv {
            beneficiary: case.beneficiary,
            ..crate::common::block()
        });
        let mut evm = MegaEvm::new(ctx).with_inspector(KvWeighsNoMore::default());
        let outcome = evm.execute_transaction(case.tx.clone()).expect("the transaction is valid");
        let usage = outcome.usage;
        assert_eq!(usage.write_records, case.records, "{}: the KV count", case.name);
        assert_eq!(
            usage.data_size - usage.write_records * WRITE_RECORD_SIZE,
            mega_evm::transaction_body_bytes(&case.tx) +
                case.other_bytes +
                case.hint_bytes +
                case.transfer_logs * TRANSFER_LOG_SIZE,
            "{}: what is kept beside the records",
            case.name,
        );
        let probe = evm.inspector();
        assert_eq!(probe.over, None, "{}: after every instruction", case.name);
        ran += probe.steps;
    }
    assert!(ran > 100, "the corpus runs code: {ran} instructions");
}
