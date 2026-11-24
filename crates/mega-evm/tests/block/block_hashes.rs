//! The block hashes a block's transactions read, which a stateless witness needs.

use alloy_evm::block::BlockExecutor;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{Bytes, B256};
use mega_evm::{test_utils::BytecodeBuilder, MegaBlockExecutor};
use revm::{
    bytecode::opcode::{BLOCKHASH, POP},
    database::State,
};

use crate::common::{self, executor, recovered, user_tx, BLOCK_NUMBER, CONTRACT};

/// The block number the contract below reads, one below the block being executed.
const READ_BLOCK: u64 = BLOCK_NUMBER - 1;

/// A second block number, read by a second contract.
const OTHER_READ_BLOCK: u64 = BLOCK_NUMBER - 2;

/// An account with no code, so a call to it reads no block hash.
const PLAIN: alloy_primitives::Address =
    alloy_primitives::address!("0x1000000000000000000000000000000000000002");

/// A second contract, which reads a different block number.
const OTHER: alloy_primitives::Address =
    alloy_primitives::address!("0x1000000000000000000000000000000000000003");

/// Code that reads `BLOCKHASH(READ_BLOCK)` and discards it.
fn blockhash_contract() -> Bytes {
    blockhash_reader(READ_BLOCK)
}

/// Code that reads `BLOCKHASH(number)` and discards it.
fn blockhash_reader(number: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(number as u32)
        .append(BLOCKHASH)
        .append(POP)
        .stop()
        .build()
}

/// The block numbers the record holds.
fn read_numbers(executor: &crate::common::TestExecutor<'_>) -> Vec<u64> {
    executor.get_accessed_block_hashes().into_keys().collect()
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

/// The record covers one block: an EVM and a `State` carried over to the next block report that
/// block's reads and not the ones before it.
#[test]
fn test_two_blocks_over_the_same_evm_report_only_their_own_reads() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, blockhash_reader(READ_BLOCK));
    db.set_account_code(OTHER, blockhash_reader(OTHER_READ_BLOCK));
    let mut state = State::builder().with_database(db).build();

    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&user_tx(0, 1_000_000)).expect("the transaction executes");
    assert_eq!(read_numbers(&executor), vec![READ_BLOCK]);

    // The next block runs on the EVM the first handed back, so both its record and its database
    // carry what the first block read.
    let (evm, _) = executor.finish_with_counters().expect("the block finishes");
    let mut executor = MegaBlockExecutor::new(
        evm,
        common::unlimited_ctx(),
        common::chain_spec(),
        OpAlloyReceiptBuilder::default(),
    );
    executor.apply_pre_execution_changes().expect("the next block starts");
    executor
        .execute_transaction(&recovered(common::tx(1, OTHER, Bytes::new(), 1_000_000)))
        .expect("the transaction executes");

    assert_eq!(
        read_numbers(&executor),
        vec![OTHER_READ_BLOCK],
        "what the block before read is not this block's read set"
    );
}

/// A hash the database already cached is not a read of this block — and the cache keeps it, so
/// nothing has to be fetched twice.
#[test]
fn test_a_cached_hash_the_block_never_read_is_not_reported() {
    const CACHED: u64 = BLOCK_NUMBER - 3;

    let mut db = common::database();
    db.set_account_code(CONTRACT, blockhash_contract());
    let mut state = State::builder().with_database(db).build();
    state.block_hashes.insert(CACHED, B256::repeat_byte(0xaa));

    {
        let mut executor = executor(&mut state, common::unlimited_ctx());
        executor.apply_pre_execution_changes().expect("the block starts");
        executor.execute_transaction(&user_tx(0, 1_000_000)).expect("the transaction executes");

        assert_eq!(
            read_numbers(&executor),
            vec![READ_BLOCK],
            "the cached hash was never asked for in this block"
        );
    }

    assert!(
        state.block_hashes.iter().any(|(number, _)| number == CACHED),
        "and the cache still holds it"
    );
}
