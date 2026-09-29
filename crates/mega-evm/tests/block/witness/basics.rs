//! The harness on ordinary blocks: writes, an empty block and a refused transaction.

use alloy_primitives::{Bytes, U256};
use mega_evm::test_utils::BytecodeBuilder;
use revm::bytecode::opcode::{CALLDATALOAD, PUSH0, SSTORE};

use super::harness::{call, Case};
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
/// limit — reads nothing and is refused the same way on replay.
#[test]
fn test_a_refused_transaction_reads_nothing_and_is_refused_on_replay() {
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
}
