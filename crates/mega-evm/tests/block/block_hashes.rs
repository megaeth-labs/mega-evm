//! The block hashes a block's transactions read, which a stateless witness needs.

use alloy_evm::block::BlockExecutor;
use alloy_primitives::Bytes;
use mega_evm::test_utils::BytecodeBuilder;
use revm::{
    bytecode::opcode::{BLOCKHASH, POP},
    database::State,
};

use crate::common::{self, executor, recovered, user_tx, BLOCK_NUMBER, CONTRACT};

/// The block number the contract below reads, one below the block being executed.
const READ_BLOCK: u64 = BLOCK_NUMBER - 1;

/// An account with no code, so a call to it reads no block hash.
const PLAIN: alloy_primitives::Address =
    alloy_primitives::address!("0x1000000000000000000000000000000000000002");

/// Code that reads `BLOCKHASH(READ_BLOCK)` and discards it.
fn blockhash_contract() -> Bytes {
    BytecodeBuilder::default()
        .push_number(READ_BLOCK as u32)
        .append(BLOCKHASH)
        .append(POP)
        .stop()
        .build()
}

/// The record accumulates over the block's transactions and can be cleared, so a caller can
/// attribute the reads to one transaction. Clearing changes no execution result.
#[test]
fn test_accessed_block_hashes_track_and_clear_per_transaction() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, blockhash_contract());
    db.set_account_code(PLAIN, Bytes::new());
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.clear_accessed_block_hashes();

    // A transaction that reads a block hash leaves it in the record.
    executor.execute_transaction(&user_tx(0, 1_000_000)).expect("the transaction executes");
    let accessed = executor.get_accessed_block_hashes();
    assert_eq!(accessed.len(), 1, "the read is recorded");
    assert!(accessed.contains_key(&READ_BLOCK), "under the block number it asked for");

    // Clearing empties the record.
    executor.clear_accessed_block_hashes();
    assert!(executor.get_accessed_block_hashes().is_empty());

    // A transaction that reads none leaves the cleared record empty, even though an earlier
    // transaction of the same block read one.
    executor
        .execute_transaction(&recovered(common::tx(1, PLAIN, Bytes::new(), 1_000_000)))
        .expect("the transaction executes");
    assert!(executor.get_accessed_block_hashes().is_empty());

    // And a read after the clear is attributed to the transaction that made it.
    executor.execute_transaction(&user_tx(2, 1_000_000)).expect("the transaction executes");
    assert_eq!(executor.get_accessed_block_hashes().len(), 1);
}
