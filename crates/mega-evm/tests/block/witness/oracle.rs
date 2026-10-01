//! Oracle reads, hints and the high-precision timestamp through the harness.
//!
//! An oracle read loads the chain's slot, then takes the service's answer over it. The chain's
//! slot is in the record like any slot; the service's answer is not in any database, so a replay
//! matches only when it is given the answer — the included transactions' own records, in block
//! order — or when the chain holds it at the read, which is what a node must arrange for a
//! validator that runs no service, and can arrange only when every answer a transaction was given
//! is a value the slot can hold at that read: a transaction answered two values for one slot
//! replays from its record alone.

use alloy_primitives::{Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        IOracle, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, MEGA_SYSTEM_ADDRESS,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE,
    },
    test_utils::{BytecodeBuilder, MemoryDatabase},
    OracleRead,
};
use revm::bytecode::opcode::{CALL, GAS, POP, PUSH0};

use super::harness::{assert_differs, assert_same_run, call, legacy_from, Case, Envs, Oracle};
use crate::common::{self, CONTRACT};

/// The slot the tests read, the value the service answers, and the value the chain holds.
const SLOT: U256 = U256::from_limbs([42, 0, 0, 0]);
const SERVICE_VALUE: U256 = U256::from_limbs([0x1234_5678, 0, 0, 0]);
const STATE_VALUE: U256 = U256::from_limbs([0xfedc_ba98, 0, 0, 0]);

/// A second slot, which a dropped candidate reads, and the value the service answers for it —
/// and answers a dropped candidate for [`SLOT`] before it moves on to [`SERVICE_VALUE`].
const OTHER_SLOT: U256 = U256::from_limbs([43, 0, 0, 0]);
const OTHER_VALUE: U256 = U256::from_limbs([0x0bad_cafe, 0, 0, 0]);

/// The service's read of [`SLOT`], as the engine records it.
const SLOT_READ: OracleRead = OracleRead { slot: SLOT, answer: Some(SERVICE_VALUE) };

/// The selector of the timestamp contract's `timestamp()`.
const TIMESTAMP_SELECTOR: [u8; 4] = [0xb8, 0x07, 0x77, 0xea];

/// A microsecond timestamp the service holds in the Oracle's slot 0.
const MICROSECONDS: U256 = U256::from_limbs([1_800_000_000_000_000, 0, 0, 0]);

/// A chain holding the Oracle, as every chain does after its first Satin block: a block that
/// creates the Oracle clears its storage, and a created account's slots are known to be zero
/// without a read, so only a chain that already holds it reads the Oracle's slots.
pub(super) fn db() -> MemoryDatabase {
    common::database().account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
}

/// A gas limit with room for a call into the Oracle carrying `calldata` bytes.
fn gas(calldata: u64) -> u64 {
    1_000_000 + common::body_history(calldata)
}

/// The calldata of `getSlot(SLOT)`.
fn get_slot() -> Bytes {
    get_slot_of(SLOT)
}

/// The calldata of `getSlot(slot)`.
fn get_slot_of(slot: U256) -> Bytes {
    IOracle::getSlotCall { slot }.abi_encode().into()
}

/// A system transaction writing `value` into `slot` of the Oracle: a legacy call from the live
/// system address to `setSlot`, which the engine promotes to a deposit. Its gas is regular room
/// on top of the account the engine creates for its caller and the history its body would pay.
fn oracle_update(slot: U256, value: U256) -> super::harness::Tx {
    let input = IOracle::setSlotCall { slot, value: B256::from(value) }.abi_encode();
    let gas =
        1_000_000 + common::new_account_state_gas() + common::body_history(input.len() as u64);
    legacy_from(
        MEGA_SYSTEM_ADDRESS,
        0,
        TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        U256::ZERO,
        input.into(),
        gas,
    )
}

/// The word a successful call returned.
fn returned(run: &super::harness::Run, index: usize) -> U256 {
    let output = run.tx(index).result.output().cloned().unwrap_or_default();
    U256::from_be_slice(&output)
}

/// A read the service answers over the chain's value: the read and its answer are in the record,
/// and so is the chain's slot; a replay given the answers matches, and one without a service
/// answers the chain's value and produces another block.
#[test]
fn test_an_oracle_read_replays_with_its_answer_and_not_without() {
    let db = db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, STATE_VALUE);
    let case = Case::new("oracle read", db)
        .envs(Envs::new().with_oracle_storage(SLOT, SERVICE_VALUE))
        .tx(call(0, ORACLE_CONTRACT_ADDRESS, get_slot(), gas(36)));
    let recorded = case.record();
    assert_eq!(returned(&recorded, 0), SERVICE_VALUE, "the service answered");
    assert_eq!(recorded.record.oracle_reads, vec![SLOT_READ]);
    assert_eq!(recorded.tx(0).oracle_reads, vec![SLOT_READ], "and the transaction recorded it");
    assert_eq!(
        recorded.record.storage.get(&(ORACLE_CONTRACT_ADDRESS, SLOT)),
        Some(&STATE_VALUE),
        "the chain's slot was loaded all the same"
    );

    let given = case.replay_channels(&recorded, Oracle::Recorded);
    assert_same_run("oracle read, answers given", &recorded, &given);

    let without = case.replay_channels(&recorded, Oracle::Absent);
    assert_eq!(returned(&without, 0), STATE_VALUE, "the chain's value stands in");
    assert_differs("oracle read, no service", &recorded, &without);
    assert!(recorded.record.covers(&without.record), "it read nothing else");
}

/// When the chain holds what the service answers — the node wrote it before the read — a replay
/// without a service produces the same block: the read is priced cold on both paths and the slot
/// is loaded on both.
#[test]
fn test_an_oracle_read_the_chain_holds_replays_without_a_service() {
    let db = db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, SERVICE_VALUE);
    let case = Case::new("oracle read on chain", db)
        .envs(Envs::new().with_oracle_storage(SLOT, SERVICE_VALUE))
        .tx(call(0, ORACLE_CONTRACT_ADDRESS, get_slot(), gas(36)));
    let recorded = case.record();
    assert_eq!(recorded.record.oracle_reads, vec![SLOT_READ]);
    let without = case.replay_channels(&recorded, Oracle::Absent);
    assert_same_run("oracle read on chain, no service", &recorded, &without);
}

/// A read the service does not answer takes the chain's value on both runs.
#[test]
fn test_an_unanswered_oracle_read_replays() {
    let db = db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, STATE_VALUE);
    let replay = Case::new("oracle unanswered", db)
        .tx(call(0, ORACLE_CONTRACT_ADDRESS, get_slot(), gas(36)))
        .run();
    assert_eq!(returned(&replay.recorded, 0), STATE_VALUE);
    assert_eq!(replay.recorded.record.oracle_reads, vec![OracleRead { slot: SLOT, answer: None }]);
}

/// The answers a validator replays are the included transactions' own, in block order, not the
/// service's view of every candidate: a candidate that read another slot and was dropped leaves
/// its read in the service's log and in no transaction's record. Given the included
/// transaction's record the replay matches; given the service's log it answers the wrong read
/// and computes another block.
#[test]
fn test_the_replayed_answers_are_the_included_transactions_own() {
    let db = db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, STATE_VALUE).account_storage(
        ORACLE_CONTRACT_ADDRESS,
        OTHER_SLOT,
        STATE_VALUE,
    );
    let case = Case::new("oracle read after a dropped candidate", db)
        .envs(
            Envs::new()
                .with_oracle_storage(SLOT, SERVICE_VALUE)
                .with_oracle_storage(OTHER_SLOT, OTHER_VALUE),
        )
        .tx(call(0, ORACLE_CONTRACT_ADDRESS, get_slot_of(OTHER_SLOT), gas(36)))
        .dropped(0)
        .tx(call(0, ORACLE_CONTRACT_ADDRESS, get_slot(), gas(36)));
    let recorded = case.record();
    assert!(recorded.txs[0].is_err(), "the candidate was dropped");
    assert_eq!(returned(&recorded, 1), SERVICE_VALUE);
    let other_read = OracleRead { slot: OTHER_SLOT, answer: Some(OTHER_VALUE) };
    assert_eq!(recorded.record.oracle_reads, vec![other_read, SLOT_READ], "the service saw both");
    assert_eq!(recorded.tx(1).oracle_reads, vec![SLOT_READ], "the included one recorded its own");
    assert_eq!(recorded.included_oracle_reads(), vec![SLOT_READ]);

    let own = case.replay_channels(&recorded, Oracle::Recorded);
    assert_same_run("the included transaction's record", &recorded, &own);
    assert!(own.oracle_replayed_exactly);

    // The service's log, replayed in order, answers the included transaction's read with the
    // candidate's: a mismatch, which leaves the chain's value standing.
    let log = case.replay(&recorded.record, &recorded.included(), Oracle::Recorded);
    assert!(!log.oracle_replayed_exactly, "the log's first read is not the transaction's");
    assert_eq!(returned(&log, 1), STATE_VALUE);
    assert_differs("the service's log", &recorded, &log);
}

/// A dropped candidate that read the same slot as an included transaction, the service having
/// moved on between the two: its log holds both answers for the slot, in execution order, and
/// the included transaction's record holds its own.
///
/// A validator given the included transaction's record replays the block. One handed the
/// service's log answers the included read with the candidate's answer — the slot is the same, so
/// no read is out of place, and only the answer left over tells — and computes another block. A
/// validator without a service depends on the slot alone: it computes the block when the journal
/// holds the included read's answer at the read, whatever the service answered before, and
/// another block when it holds the candidate's.
#[test]
fn test_a_dropped_read_of_the_same_slot_leaves_the_included_read_its_own_answer() {
    let case = |chain_value: U256| {
        Case::new(
            "oracle read after a dropped read of the same slot",
            db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, chain_value),
        )
        .envs(Envs::new().with_oracle_storage(SLOT, OTHER_VALUE))
        .tx(call(0, ORACLE_CONTRACT_ADDRESS, get_slot(), gas(36)))
        .dropped(0)
        .service_answers_after(0, SLOT, SERVICE_VALUE)
        .tx(call(0, ORACLE_CONTRACT_ADDRESS, get_slot(), gas(36)))
    };
    let dropped_read = OracleRead { slot: SLOT, answer: Some(OTHER_VALUE) };

    // The chain holds the answer the dropped candidate was given.
    let stale = case(OTHER_VALUE);
    let recorded = stale.record();
    assert!(recorded.txs[0].is_err(), "the candidate was dropped");
    assert_eq!(returned(&recorded, 1), SERVICE_VALUE, "the service had moved on");
    assert_eq!(recorded.record.oracle_reads, vec![dropped_read, SLOT_READ], "one slot, twice");
    assert_eq!(recorded.tx(1).oracle_reads, vec![SLOT_READ], "the included one recorded its own");
    assert_eq!(recorded.included_oracle_reads(), vec![SLOT_READ]);

    let own = stale.replay_channels(&recorded, Oracle::Recorded);
    assert_same_run("the included transaction's record", &recorded, &own);
    assert!(own.oracle_replayed_exactly);

    let mut witness = stale.channel_witness(&recorded);
    witness.oracle_reads.clone_from(&recorded.record.oracle_reads);
    let log = stale.replay(&witness, &recorded.included(), Oracle::Recorded);
    assert_eq!(returned(&log, 1), OTHER_VALUE, "the candidate's answer, for the same slot");
    assert_eq!(log.tx(1).oracle_reads, vec![dropped_read]);
    assert!(!log.oracle_replayed_exactly, "an answer was left over");
    assert_differs("the service's log", &recorded, &log);

    let without = stale.replay_channels(&recorded, Oracle::Absent);
    assert_eq!(returned(&without, 1), OTHER_VALUE, "the slot holds the candidate's answer");
    assert_differs("no service, the candidate's answer in the slot", &recorded, &without);

    // The chain holds the answer the included transaction was given.
    let held = case(SERVICE_VALUE);
    let recorded = held.record();
    assert_eq!(recorded.record.oracle_reads, vec![dropped_read, SLOT_READ]);
    assert_eq!(returned(&recorded, 1), SERVICE_VALUE);
    let without = held.replay_channels(&recorded, Oracle::Absent);
    assert_same_run("no service, the included answer in the slot", &recorded, &without);
}

/// A validator without a service finds the answer in the slot only if an included transaction
/// wrote it there before the read: the node's oracle update, included, makes the replay match;
/// the same update executed and dropped leaves the chain's old value at the read, and the replay
/// without a service computes another block, while a replay given the read's record still
/// matches.
#[test]
fn test_a_validator_without_a_service_needs_the_included_update() {
    let case = |dropped: bool| {
        let db = db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, STATE_VALUE);
        let case =
            Case::new(if dropped { "oracle update dropped" } else { "oracle update included" }, db)
                .envs(Envs::new().with_oracle_storage(SLOT, SERVICE_VALUE))
                .tx(oracle_update(SLOT, SERVICE_VALUE))
                .tx(call(0, ORACLE_CONTRACT_ADDRESS, get_slot(), gas(36)));
        if dropped {
            case.dropped(0)
        } else {
            case
        }
    };

    let included = case(false);
    let recorded = included.record();
    assert!(recorded.tx(0).result.is_success(), "{:?}", recorded.tx(0).result);
    assert_eq!(returned(&recorded, 1), SERVICE_VALUE);
    let written = &recorded.tx(0).state[&ORACLE_CONTRACT_ADDRESS].storage[&SLOT];
    assert_eq!(written.present_value, SERVICE_VALUE, "the update wrote the answer");
    let without = included.replay_channels(&recorded, Oracle::Absent);
    assert_same_run("oracle update included, no service", &recorded, &without);

    let dropped = case(true);
    let recorded = dropped.record();
    assert!(recorded.txs[0].is_err(), "the update was dropped");
    assert_eq!(returned(&recorded, 1), SERVICE_VALUE, "the service answered all the same");
    assert_eq!(recorded.tx(1).oracle_reads, vec![SLOT_READ]);
    let given = dropped.replay_channels(&recorded, Oracle::Recorded);
    assert_same_run("oracle update dropped, answers given", &recorded, &given);
    let without = dropped.replay_channels(&recorded, Oracle::Absent);
    assert_eq!(returned(&without, 1), STATE_VALUE, "the chain never held the answer");
    assert_differs("oracle update dropped, no service", &recorded, &without);
}

/// Code that sends a hint naming `SLOT`, then reads `SLOT` through the Oracle and returns it.
fn hint_then_read() -> Bytes {
    let hint = IOracle::sendHintCall {
        topic: SLOT.into(),
        data: Bytes::from(SERVICE_VALUE.to_be_bytes::<32>()),
    }
    .abi_encode();
    let read = get_slot();
    let mut code = BytecodeBuilder::default().mstore(0, &hint);
    code = code
        .append_many([PUSH0, PUSH0])
        .push_number(hint.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append_many([GAS, CALL, POP]);
    code = code.mstore(0x100, &read);
    code.push_number(32_u8)
        .push_number(0x200_u16)
        .push_number(read.len() as u64)
        .push_number(0x100_u16)
        .append(PUSH0)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append_many([GAS, CALL, POP])
        .push_number(32_u8)
        .push_number(0x200_u16)
        .append(revm::bytecode::opcode::RETURN)
        .build()
}

/// A hint followed by a read: the hint reaches the service and is in the record, in order before
/// the read, and the read's answer replays.
#[test]
fn test_a_hint_and_a_read_replay() {
    let mut db = db();
    db.set_account_code(CONTRACT, hint_then_read());
    let replay = Case::new("hint then read", db)
        .envs(Envs::new().with_oracle_storage(SLOT, SERVICE_VALUE))
        .tx(call(0, CONTRACT, Bytes::new(), 2_000_000 + common::body_history(0)))
        .run();
    let run = &replay.recorded;
    assert_eq!(returned(run, 0), SERVICE_VALUE);
    assert_eq!(run.record.hints.len(), 1, "the hint reached the service");
    assert_eq!(run.record.hints[0].topic, B256::from(SLOT));
    assert_eq!(run.record.oracle_reads, vec![SLOT_READ]);
}

/// The high-precision timestamp is an Oracle read of slot 0 through the wrapper's own bytecode:
/// the read is in the record, and the value the service answers replays.
#[test]
fn test_the_high_precision_timestamp_replays() {
    let replay = Case::new("timestamp", db())
        .envs(Envs::new().with_oracle_storage(U256::ZERO, MICROSECONDS))
        .tx(call(0, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, TIMESTAMP_SELECTOR.into(), gas(4)))
        .run();
    let run = &replay.recorded;
    assert_eq!(returned(run, 0), MICROSECONDS, "{:?}", run.tx(0).result);
    assert_eq!(
        run.record.oracle_reads,
        vec![OracleRead { slot: U256::ZERO, answer: Some(MICROSECONDS) }]
    );
    assert!(
        run.record.storage.contains_key(&(ORACLE_CONTRACT_ADDRESS, U256::ZERO)),
        "the Oracle's slot 0 is loaded from the chain"
    );
}
