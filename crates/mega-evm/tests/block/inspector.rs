//! The admission gate: which inspectors reach block execution.

use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    Evm, EvmFactory,
};
use mega_evm::{test_utils::GasInspector, DeclaredObserver, MegaEvmFactory};
use revm::inspector::NoOpInspector;

use crate::common::{self, user_tx};

/// A declared observer reaches a block: its type says it writes nothing back, so what the block
/// executes is what the chain executes.
#[test]
fn test_a_declared_observer_reaches_block_execution() {
    let mut state = common::state();
    let factory = common::factory();

    let mut executor = factory.create_executor_with_trusted_inspector(
        &mut state,
        common::evm_env(),
        common::unlimited_ctx(),
        DeclaredObserver::new(GasInspector::new()),
    );

    assert!(executor.evm().is_inspecting(), "the inspector runs");
    assert!(!executor.evm().has_rewriting_inspector(), "and is admitted");
    executor.apply_pre_execution_changes().expect("a declared observer is admitted");
    executor.execute_transaction(&user_tx(0, 100_000)).expect("the transaction executes");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
}

/// An inspector that arrived without the declaration does not: block execution refuses it before
/// the block's first call.
#[test]
fn test_a_rewriting_inspector_cannot_reach_block_execution() {
    let mut state = common::state();
    let factory = common::factory();
    let evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_inspector(GasInspector::new());
    assert!(evm.has_rewriting_inspector());

    let mut executor = factory.create_executor(evm, common::unlimited_ctx());

    let err = executor
        .apply_pre_execution_changes()
        .expect_err("an undeclared inspector has no route to a block");
    assert!(
        format!("{err}").contains("does not admit an inspector that may rewrite execution"),
        "{err}"
    );
}

/// An EVM with no inspector at all is admitted, which is the ordinary case.
#[test]
fn test_a_block_without_an_inspector_is_admitted() {
    let mut state = common::state();
    let factory = common::factory();
    let evm = MegaEvmFactory::new().create_evm(&mut state, common::evm_env());

    let mut executor = factory.create_executor(evm, common::unlimited_ctx());

    assert!(!executor.evm().is_inspecting());
    executor.apply_pre_execution_changes().expect("nothing to refuse");
}

/// The gate keys on what runs, not on what is held: an inspector that is disabled does not
/// rewrite anything, and the block is admitted.
#[test]
fn test_a_disabled_inspector_is_admitted() {
    let mut state = common::state();
    let factory = common::factory();
    let mut evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_inspector(NoOpInspector);
    evm.set_inspector_enabled(false);

    let mut executor = factory.create_executor(evm, common::unlimited_ctx());

    executor.apply_pre_execution_changes().expect("a disabled inspector rewrites nothing");
}

/// The gate is checked at every entry point, not once: an inspector enabled after the block was
/// set up is refused by the transaction that would run under it, by a commit and by the end of
/// the block.
#[test]
fn test_enabling_a_rewriting_inspector_after_the_block_started_is_refused() {
    let mut state = common::state();
    let factory = common::factory();
    let mut evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_inspector(GasInspector::new());
    evm.set_inspector_enabled(false);

    let mut executor = factory.create_executor(evm, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("a disabled inspector rewrites nothing");

    // The one transaction that ran before the inspector was enabled is admitted.
    let outcome = executor
        .execute_transaction_without_commit(&user_tx(0, 100_000))
        .expect("the block is still clean");

    executor.evm_mut().set_inspector_enabled(true);

    assert!(is_refused(executor.execute_transaction(&user_tx(1, 100_000))), "no transaction runs");
    assert!(is_refused(executor.commit_transaction_outcome(outcome)), "nothing commits");
    assert!(executor.receipts().is_empty(), "the block packed nothing");
    assert!(is_refused(executor.finish_with_counters()), "and the block does not finish");
}

/// The gate holds for a caller that never runs the block's pre-execution changes: the check is on
/// the path that runs code, not only on the block's setup.
#[test]
fn test_a_rewriting_inspector_is_refused_without_the_pre_execution_changes() {
    let mut state = common::state();
    let factory = common::factory();
    let evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_inspector(GasInspector::new());

    let mut executor = factory.create_executor(evm, common::unlimited_ctx());

    assert!(
        is_refused(executor.execute_transaction_without_commit(&user_tx(0, 100_000))),
        "skipping the setup is not a way past the gate"
    );
    assert!(is_refused(executor.finish_with_counters()), "and neither is finishing the block");
}

/// Whether block execution refused the call because of the inspector.
fn is_refused<T>(result: Result<T, alloy_evm::block::BlockExecutionError>) -> bool {
    match result {
        Ok(_) => false,
        Err(err) => format!("{err}").contains("does not admit an inspector that may rewrite"),
    }
}
