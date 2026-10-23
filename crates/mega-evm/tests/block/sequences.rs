//! Blocks that follow one another on one state.
//!
//! A rotation, the block-hash ring and an activation block are each settled inside a single
//! block elsewhere. These tests run the blocks around that one, on the state the earlier block
//! committed.

use alloy_eips::eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_evm::{block::BlockExecutor, EvmEnv};
use alloy_primitives::{Bytes, B256, U256};
use mega_evm::{test_utils::rex6_predeploys, BlockLimits, MegaBlockExecutionCtx, MegaSpecId};
use revm::{database::State, Database};

use crate::common::{
    self, deposit_tx, empty_call_gas, executor, executor_with_env, unlimited_ctx, user_tx,
    BLOCK_NUMBER, CALLER,
};

/// The seven predeploys are as the state held them before the blocks: the code and the nonce.
fn assert_predeploys_unchanged(state: &mut State<mega_evm::test_utils::MemoryDatabase>) {
    for (address, held, _) in rex6_predeploys(common::registry_roles()) {
        let info = state.basic(address).expect("readable").expect("still there");
        assert_eq!((info.code_hash, info.nonce), (held.code_hash, held.nonce), "{address}");
    }
}

/// The ring slot EIP-2935 writes the parent hash of block `number` into.
fn ring_slot(number: u64) -> u64 {
    (number - 1) % (HISTORY_SERVE_WINDOW as u64)
}

/// The parent hash the history contract holds at `slot`.
fn stored_parent(state: &mut State<mega_evm::test_utils::MemoryDatabase>, slot: u64) -> B256 {
    let word = state.storage(HISTORY_STORAGE_ADDRESS, U256::from(slot)).expect("readable");
    B256::from(word.to_be_bytes())
}

/// Starts block `number` and commits its pre-block calls, recording `parent` in the history ring.
fn record_parent(
    state: &mut State<mega_evm::test_utils::MemoryDatabase>,
    number: u64,
    parent: B256,
) {
    let mut env = common::evm_env();
    env.block_env.number = U256::from(number);
    let ctx = MegaBlockExecutionCtx::new(
        parent,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    let mut executor = executor_with_env(state, ctx, env);
    executor.apply_pre_execution_changes().expect("the block starts");
}

/// The EIP-2935 ring stores one parent hash per block, and a later block that lands on a used
/// slot replaces that hash and leaves the slot beside it.
///
/// Block 1 writes slot 0. The block at the window writes the last slot. The next block wraps
/// onto slot 0. One window later the last slot is written again, and slot 0 still holds the
/// hash the wrap stored.
#[test]
fn test_the_block_hash_ring_wraps_and_keeps_the_slot_beside_it() {
    let window = HISTORY_SERVE_WINDOW as u64;
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    let mut state = State::builder().with_database(db).build();

    let first = B256::repeat_byte(0x11);
    let edge = B256::repeat_byte(0x22);
    let wrapped = B256::repeat_byte(0x33);
    let again = B256::repeat_byte(0x44);

    record_parent(&mut state, 1, first);
    assert_eq!(ring_slot(1), 0);
    assert_eq!(stored_parent(&mut state, 0), first);

    record_parent(&mut state, window, edge);
    assert_eq!(ring_slot(window), window - 1);
    assert_eq!(stored_parent(&mut state, window - 1), edge);
    assert_eq!(stored_parent(&mut state, 0), first, "the earlier slot survives");

    record_parent(&mut state, window + 1, wrapped);
    assert_eq!(ring_slot(window + 1), 0);
    assert_eq!(stored_parent(&mut state, 0), wrapped, "the next block wraps onto slot 0");
    assert_eq!(stored_parent(&mut state, window - 1), edge, "the slot beside the wrap survives");

    record_parent(&mut state, window * 2, again);
    assert_eq!(ring_slot(window * 2), window - 1);
    assert_eq!(stored_parent(&mut state, window - 1), again, "a later turn overwrites the edge");
    assert_eq!(stored_parent(&mut state, 0), wrapped, "the wrapped slot stands");
}

/// The environment of the block after the one the shared helpers build.
fn next_block_env() -> EvmEnv<MegaSpecId> {
    let mut env = common::evm_env();
    env.block_env.number = U256::from(BLOCK_NUMBER + 1);
    env
}

/// Satin's activation block packs a deposit and refuses a user transaction. The next block, on
/// the state that block committed, packs the user transaction, and the predeploys the activation
/// block checked are as they were.
#[test]
fn test_the_block_after_an_activation_block_packs_a_user_transaction() {
    let mut state = common::state();
    let nonce = {
        let ctx = unlimited_ctx()
            .with_no_user_tx_activation_block(true)
            .with_satin_activation_block(true);
        let mut executor = executor(&mut state, ctx);
        executor.apply_pre_execution_changes().expect("the activation block starts");
        executor
            .execute_transaction(&deposit_tx(Bytes::new(), empty_call_gas()))
            .expect("a deposit is what an activation block packs");
        let err = executor
            .execute_transaction(&user_tx(0, empty_call_gas()))
            .expect_err("a user transaction has no place in an activation block");
        assert!(
            format!("{err}").contains("non-deposit transaction in fork activation block"),
            "{err}",
        );

        let (_, result) = executor.finish_with_counters().expect("the activation block finishes");
        assert_eq!(result.receipts().len(), 1, "only the deposit was packed");
        assert!(result.receipts()[0].status(), "the deposit succeeds");
        state.basic(CALLER).expect("readable").expect("the caller").nonce
    };
    assert_predeploys_unchanged(&mut state);

    let mut executor = executor_with_env(&mut state, unlimited_ctx(), next_block_env());
    executor.apply_pre_execution_changes().expect("the next block starts");
    executor
        .execute_transaction(&user_tx(nonce, empty_call_gas()))
        .expect("the block after an activation block packs a user transaction");
    let (_, result) = executor.finish_with_counters().expect("the next block finishes");
    assert_eq!(result.receipts().len(), 1);
    assert!(result.receipts()[0].status(), "the user transaction succeeds");
    assert_predeploys_unchanged(&mut state);
}
