//! The receipts a block builds, which go through an OP receipt builder.

use alloy_evm::block::BlockExecutor;
use alloy_hardforks::ForkCondition;
use alloy_op_hardforks::OpHardfork;
use alloy_primitives::Bytes;
use op_alloy_consensus::OpReceiptEnvelope;

use crate::common::{
    self, deposit_tx, empty_call_gas, executor, executor_with_spec, unlimited_ctx, user_tx,
};

/// The deposit receipt of the block's only transaction.
fn deposit_receipt(receipts: &[OpReceiptEnvelope]) -> &op_alloy_consensus::OpDepositReceipt {
    match receipts {
        [OpReceiptEnvelope::Deposit(deposit)] => &deposit.receipt,
        other => panic!("one deposit receipt, got {other:?}"),
    }
}

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

/// A deposit's receipt carries the depositor's nonce and the receipt version, each from the fork
/// that introduced it: Regolith for the nonce, Canyon for the version.
#[test]
fn test_a_deposit_receipt_reports_the_nonce_and_the_version_the_schedule_activated() {
    let mut state = common::state();
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&deposit_tx(Bytes::new(), 100_000)).expect("the deposit executes");

    let receipt = deposit_receipt(executor.receipts());
    assert_eq!(receipt.deposit_nonce, Some(0), "the depositor's nonce before the deposit ran");
    assert_eq!(receipt.deposit_receipt_version, Some(1), "Canyon's receipt version");
}

/// The same deposit on a chain that has reached neither fork: no nonce and no version. The two
/// fields follow the chain's schedule, not the transaction.
#[test]
fn test_a_deposit_receipt_omits_what_the_schedule_has_not_activated() {
    let before_both = common::chain_spec()
        .with(OpHardfork::Regolith, ForkCondition::Never)
        .with(OpHardfork::Canyon, ForkCondition::Never);

    let mut state = common::state();
    let mut executor = executor_with_spec(&mut state, unlimited_ctx(), before_both);
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&deposit_tx(Bytes::new(), 100_000)).expect("the deposit executes");

    let receipt = deposit_receipt(executor.receipts());
    assert_eq!(receipt.deposit_nonce, None, "Regolith brought the nonce");
    assert_eq!(receipt.deposit_receipt_version, None, "Canyon brought the version");
}
