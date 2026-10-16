//! The harness on ordinary blocks: writes, an empty block, a refused transaction, a dropped
//! candidate, the reads of a frame that failed and of a deposit that failed, a SALT lookup that
//! fails and one answered below the minimum bucket.

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{test_utils::BytecodeBuilder, MegaHaltReason, SaltEnv, MIN_BUCKET_SIZE};
use revm::{
    bytecode::opcode::{
        BALANCE, CALL, CALLDATALOAD, GAS, INVALID, POP, PUSH0, REVERT, SLOAD, SSTORE,
    },
    context::result::ExecutionResult,
};

use super::harness::{call, deposit, Case, Envs};
use crate::common::{self, CONTRACT};

/// Code that sets the slot the first calldata word names to one.
pub(crate) fn slot_writer() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1_u8)
        .append(PUSH0)
        .append(CALLDATALOAD)
        .append(SSTORE)
        .stop()
        .build()
}

/// Calldata naming `slot`.
pub(crate) fn slot(slot: u64) -> Bytes {
    U256::from(slot).to_be_bytes::<32>().to_vec().into()
}

/// A gas limit for one fresh slot's write with 32 bytes of calldata, at the byte prices in
/// effect.
pub(crate) fn write_gas() -> u64 {
    1_000_000 +
        common::slot_state_gas() +
        common::body_history(32) +
        mega_evm::write_record_history_gas(1).expect("a record has a price")
}

/// A block whose pre-block phase deploys the system contracts over an empty chain and whose
/// transactions write two fresh slots: the replay reads only the record and produces the same
/// block, the exported buckets are the two slots', and the absent accounts the deploys asked
/// about are in the record as absent.
#[test]
fn test_a_block_of_writes_replays_from_its_witness() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let replay = Case::new("writes", db)
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .tx(call(1, CONTRACT, slot(2), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert_eq!(run.receipts.len(), 2);
    if !common::state_is_free() {
        assert_eq!(run.bucket_ids.len(), 2, "one bucket per fresh slot");
    }
    assert_eq!(
        run.record.accounts.get(&mega_evm::system::ORACLE_CONTRACT_ADDRESS),
        Some(&None),
        "the deploy asked about the Oracle's account and found none"
    );
    assert!(run.record.codes.values().all(|code| !code.is_empty()), "code travels by hash");
    assert_eq!(
        replay.replayed.record, run.record,
        "the replay read exactly what the recording read, nothing less"
    );
}

/// An empty block: the pre-block phase alone, whose reads the record holds.
#[test]
fn test_an_empty_block_replays_from_its_witness() {
    let replay = Case::new("empty", common::database()).run();
    assert!(replay.recorded.txs.is_empty());
    assert!(replay.recorded.bucket_ids.is_empty());
    assert!(!replay.recorded.record.accounts.is_empty(), "the deploys read their accounts");
}

/// A transaction the block refuses before it runs — its declared gas above the block's gas
/// limit — reads nothing, is in no state, and is not run on replay.
#[test]
fn test_a_refused_transaction_reads_nothing_and_is_not_replayed() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let replay = Case::new("refused", db)
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .tx(call(1, CONTRACT, slot(2), common::BLOCK_GAS_LIMIT + 1))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success());
    assert!(run.refusal(1).contains("gas limit"), "{}", run.refusal(1));
    assert_eq!(run.receipts.len(), 1);
    assert!(
        !run.record.storage.contains_key(&(CONTRACT, U256::from(2))),
        "the refused transaction read no slot"
    );
    assert!(!run.keys.slots.contains(&(CONTRACT, U256::from(2))), "and no state names it");
    assert!(replay.replayed.txs[1].is_err() && replay.channel.txs[1].is_err(), "not replayed");
}

/// A candidate the builder executes and drops reads like any transaction — its reads are in the
/// database-level record — and is in no state, no receipt and no replay; the block's other
/// transaction replays on a witness that holds nothing of it.
#[test]
fn test_a_dropped_candidate_is_in_no_state_and_no_replay() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let replay = Case::new("dropped candidate", db)
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .dropped(0)
        .tx(call(0, CONTRACT, slot(2), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.txs[0].is_err(), "dropped");
    assert!(run.tx(1).result.is_success());
    assert_eq!(run.receipts.len(), 1);
    assert!(run.record.storage.contains_key(&(CONTRACT, U256::from(1))), "executed, so read");
    assert!(!run.keys.slots.contains(&(CONTRACT, U256::from(1))), "in no state");
    assert!(run.keys.slots.contains(&(CONTRACT, U256::from(2))));
    assert!(replay.channel.txs[0].is_err() && replay.channel.tx(1).result.is_success());
    if !common::state_is_free() {
        assert_eq!(run.bucket_ids.len(), 2, "the export holds the dropped candidate's bucket too");
    }
}

/// A SALT lookup that fails fails its transaction, which is in no block: the failure is in the
/// environment's record, the bucket is not exported, and the block's other transaction replays
/// on a witness that proves the answered bucket alone.
#[test]
fn test_a_failed_salt_lookup_fails_its_transaction_and_is_not_exported() {
    if common::state_is_free() {
        return;
    }
    let failing = <Envs as SaltEnv>::bucket_id_for_slot(CONTRACT, U256::from(1));
    let answered = <Envs as SaltEnv>::bucket_id_for_slot(CONTRACT, U256::from(2));
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let replay = Case::new("failed lookup", db)
        .envs(Envs::new().with_failing_bucket(failing, "salt backend unreachable".into()))
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .tx(call(0, CONTRACT, slot(2), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.refusal(0).contains("salt backend unreachable"), "{}", run.refusal(0));
    assert!(run.tx(1).result.is_success());
    assert_eq!(run.receipts.len(), 1);
    assert_eq!(
        run.record.buckets.get(&failing),
        Some(&Err("salt backend unreachable".into())),
        "the environment recorded the failure"
    );
    assert_eq!(run.bucket_ids, vec![answered], "the export holds the answered bucket alone");
}

/// A SALT environment that answers a capacity below the minimum bucket has answered nothing a
/// bucket can hold: the lookup fails its transaction as an error does, so the bucket is not
/// exported although the environment's record holds its answer, and the block's other
/// transaction replays on a witness that proves the valid bucket alone.
#[test]
fn test_a_capacity_below_the_minimum_fails_its_transaction_and_is_not_exported() {
    if common::state_is_free() {
        return;
    }
    let below = <Envs as SaltEnv>::bucket_id_for_slot(CONTRACT, U256::from(1));
    let valid = <Envs as SaltEnv>::bucket_id_for_slot(CONTRACT, U256::from(2));
    let capacity = MIN_BUCKET_SIZE as u64 - 1;
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let replay = Case::new("capacity below the minimum", db)
        .envs(Envs::new().with_bucket_capacity(below, capacity))
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .tx(call(0, CONTRACT, slot(2), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.refusal(0).contains("below the minimum bucket"), "{}", run.refusal(0));
    assert!(run.tx(1).result.is_success());
    assert_eq!(run.receipts.len(), 1);
    assert_eq!(run.record.buckets.get(&below), Some(&Ok(capacity)), "the environment answered");
    assert_eq!(run.bucket_ids, vec![valid], "the export holds the valid bucket alone");
}

/// A frame that failed has loaded what it read: its failure takes back its writes, not its
/// reads. A call into a contract that reads one of its slots and another account's balance, then
/// reverts or halts, leaves the transaction a success whose state names the slot and the account,
/// so the channel witness holds them and the block replays from it — a validator re-executing the
/// transaction makes the same reads in the same frame.
#[test]
fn test_the_reads_of_a_frame_that_failed_are_in_the_witness() {
    let reader = address!("0x4000000000000000000000000000000000000007");
    let read = address!("0x4000000000000000000000000000000000000008");
    let caller = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(reader)
        .append_many([GAS, CALL, POP])
        .stop()
        .build();
    for (how, ending) in [("reverts", vec![PUSH0, PUSH0, REVERT]), ("halts", vec![INVALID])] {
        let code = BytecodeBuilder::default()
            .push_number(7_u8)
            .append_many([SLOAD, POP])
            .push_address(read)
            .append_many([BALANCE, POP])
            .append_many(ending)
            .build();
        let db = common::database()
            .account_code(CONTRACT, caller.clone())
            .account_code(reader, code)
            .account_storage(reader, U256::from(7), U256::from(70))
            .account_balance(read, U256::from(80));
        let replay = Case::new(&format!("a frame that {how}"), db)
            .tx(call(0, CONTRACT, Bytes::new(), 1_000_000 + common::body_history(0)))
            .run();
        let run = &replay.recorded;
        assert!(run.tx(0).result.is_success(), "{how}: {:?}", run.tx(0).result);
        let state = &run.tx(0).state;
        assert!(state[&reader].storage.contains_key(&U256::from(7)), "{how}: the slot is named");
        assert!(state.contains_key(&read), "{how}: the account is named");
        assert!(run.keys.slots.contains(&(reader, U256::from(7))), "{how}: and is a key");
        assert!(run.keys.accounts.contains(&read), "{how}: as is the account");
    }
}

/// A deposit that failed has loaded what its frames read, as a frame that failed has: the
/// failure takes back everything but the sender's nonce bump and mint, not the reads. A deposit
/// from a sender that does not exist into a contract that reads one of its slots and another
/// account's balance, then halts, is included as a failed deposit whose state names the slot and
/// the account beside the sender it created, so the channel witness holds them and the block
/// replays from it — a validator re-executing the deposit makes the same reads in the same frame.
/// The failed deposit uses its whole gas limit, all of it regular gas: the account it creates for
/// its sender carries no state gas on any ledger, and it pays no history.
#[test]
fn test_the_reads_of_a_deposit_that_failed_are_in_the_witness() {
    let depositor = address!("0x4000000000000000000000000000000000000009");
    let reader = address!("0x4000000000000000000000000000000000000007");
    let read = address!("0x4000000000000000000000000000000000000008");
    let code = BytecodeBuilder::default()
        .push_number(7_u8)
        .append_many([SLOAD, POP])
        .push_address(read)
        .append_many([BALANCE, POP])
        .append(INVALID)
        .build();
    let db = common::database()
        .account_code(reader, code)
        .account_storage(reader, U256::from(7), U256::from(70))
        .account_balance(read, U256::from(80));
    let (mint, gas_limit) = (1_000_000_000, 1_000_000 + common::new_account_state_gas());
    let replay = Case::new("a deposit that failed", db)
        .tx(deposit(depositor, TxKind::Call(reader), mint, U256::ZERO, Bytes::new(), gas_limit))
        .run();
    let run = &replay.recorded;
    let tx = run.tx(0);
    assert!(
        matches!(tx.result, ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }),
        "{:?}",
        tx.result
    );
    assert!(tx.state[&reader].storage.contains_key(&U256::from(7)), "the slot is named");
    assert!(tx.state.contains_key(&read), "the account is named");
    assert!(run.keys.slots.contains(&(reader, U256::from(7))), "and is a key");
    assert!(run.keys.accounts.contains(&read), "as is the account");

    assert_eq!(run.record.accounts.get(&depositor), Some(&None), "the sender did not exist");
    let sender = &tx.state[&depositor].info;
    assert_eq!(sender.nonce, 1, "the failure keeps the nonce bump");
    assert_eq!(sender.balance, U256::from(mint), "and the mint");
    assert_eq!(tx.gas.gas_used, gas_limit, "a failed deposit uses its whole gas limit");
    assert_eq!(
        (tx.gas.regular, tx.gas.state, tx.gas.history, tx.gas.history_bytes),
        (gas_limit, 0, 0, 0),
        "all of it regular gas: the account it created carries no state gas"
    );
    assert_eq!((run.gas.state, run.gas.history), (0, 0), "nor does the block count any");
}
