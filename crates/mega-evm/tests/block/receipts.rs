//! The receipts a block builds, which go through an OP receipt builder.

use alloy_evm::block::BlockExecutor;

use crate::common::{self, empty_call_gas, executor, unlimited_ctx, user_tx};

/// A block's receipts are readable while it is still being built, and they leave with its
/// result.
#[test]
fn test_the_receipts_are_readable_during_the_block_and_leave_with_its_result() {
    let mut state = common::state();
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    assert!(executor.receipts().is_empty(), "nothing is packed yet");
    executor.execute_transaction(&user_tx(0, empty_call_gas())).expect("the transaction executes");
    assert_eq!(executor.receipts().len(), 1, "the committed transaction has its receipt");

    executor.execute_transaction(&user_tx(1, empty_call_gas())).expect("the transaction executes");
    assert_eq!(executor.receipts().len(), 2);

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 2);
    assert_eq!(result.into_receipts().len(), 2, "the receipts leave with the result");
}
