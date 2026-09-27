//! The admission gate: which inspectors reach block execution, and what a declared observer sees
//! of a block's phases.

use alloy_consensus::transaction::Recovered;
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
};
use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    Evm, EvmFactory,
};
use alloy_primitives::{Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::{BytecodeBuilder, GasInspector},
    BlockLimits, DeclaredObserver, MegaBlockExecutionCtx, MegaEvmFactory, MegaTxEnvelope,
    PreBlockStateSource,
};
use revm::{
    database::State,
    handler::FrameResult,
    inspector::NoOpInspector,
    interpreter::{FrameInput, Interpreter, InterpreterTypes},
    state::EvmState,
    Inspector,
};
use std::sync::{Arc, Mutex};

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

/// Counts the frames an inspector sees start and the instructions it sees run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen {
    frames: usize,
    steps: usize,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Seen {
    fn step(&mut self, _: &mut Interpreter<INTR>, _: &mut CTX) {
        self.steps += 1;
    }

    fn frame_start(&mut self, _: &mut CTX, _: &mut FrameInput) -> Option<FrameResult> {
        self.frames += 1;
        None
    }
}

/// The state a block runs on, with the EIP-2935 and EIP-4788 contracts' own code, so the
/// pre-block calls run instructions, and a contract that writes a slot.
fn state_with_code() -> State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    db.set_account_code(BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone());
    db.set_account_code(
        common::CONTRACT,
        BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build(),
    );
    State::builder().with_database(db).build()
}

/// A block whose parent hash and beacon root the pre-block calls record.
fn recording_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(
        B256::repeat_byte(0xab),
        Some(B256::repeat_byte(0xcd)),
        Bytes::new(),
        BlockLimits::no_limits(),
    )
}

/// A system-address transaction: a legacy call from the system address the registry names to the
/// Oracle's `getSlot(0)`.
fn system_tx() -> Recovered<MegaTxEnvelope> {
    let input = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    Recovered::new_unchecked(
        common::tx(0, ORACLE_CONTRACT_ADDRESS, input.into(), 1_000_000),
        MEGA_SYSTEM_ADDRESS,
    )
}

/// A declared observer sees each phase of a block as the inspector contract says: nothing of the
/// pre-block calls, which run code of their own, the frames of a system transaction and of a user
/// one, and nothing of the block's end, after which the EVM hands the inspector back with what it
/// saw.
#[test]
fn test_a_declared_observer_sees_the_transactions_of_a_block_and_nothing_else() {
    let mut state = state_with_code();
    let factory = common::factory();
    let mut executor = factory.create_executor_with_trusted_inspector(
        &mut state,
        common::evm_env(),
        recording_ctx(),
        DeclaredObserver::new(Seen::default()),
    );
    let pre_block = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&pre_block);
    executor.set_pre_block_observer(Some(Box::new(
        move |source: PreBlockStateSource, state: &EvmState| {
            let wrote =
                state.values().any(|account| account.storage.values().any(|s| s.is_changed()));
            log.lock().unwrap().push((source, wrote));
        },
    )));

    executor.apply_pre_execution_changes().expect("a declared observer is admitted");
    let calls: Vec<_> = pre_block
        .lock()
        .unwrap()
        .iter()
        .filter(|(source, _)| {
            matches!(source, PreBlockStateSource::Eip2935 | PreBlockStateSource::Eip4788)
        })
        .copied()
        .collect();
    assert_eq!(
        calls,
        [(PreBlockStateSource::Eip2935, true), (PreBlockStateSource::Eip4788, true)],
        "both pre-block calls ran and wrote"
    );
    assert_eq!(executor.evm().inspector().0, Seen::default(), "and the observer saw none of it");

    let outcome = executor.run_transaction(&system_tx()).expect("a system transaction");
    assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);
    assert!(executor.evm().ctx().is_system_originated(), "it is the protocol's own");
    let after_system = executor.evm().inspector().0;
    assert_eq!(after_system.frames, 1, "the system transaction's call");
    assert!(after_system.steps > 0, "and the Oracle's code");
    executor.commit_transaction(outcome);

    executor.execute_transaction(&user_tx(0, 1_000_000)).expect("a user transaction");
    let after_user = executor.evm().inspector().0;
    assert_eq!(after_user.frames, 2, "the user transaction's call");
    assert!(after_user.steps > after_system.steps);

    let (evm, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.inner.receipts.len(), 2);
    assert_eq!(evm.inspector().0, after_user, "the block's end ran no code");
}

/// A rewriting inspector is refused at a system transaction as at a user one: the gate is the
/// entry point's, and the system transaction does not run.
#[test]
fn test_a_rewriting_inspector_is_refused_at_a_system_transaction() {
    let mut state = state_with_code();
    let factory = common::factory();
    let mut evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_inspector(Seen::default());
    evm.set_inspector_enabled(false);
    let mut executor = factory.create_executor(evm, recording_ctx());
    executor.apply_pre_execution_changes().expect("a disabled inspector rewrites nothing");

    executor.evm_mut().set_inspector_enabled(true);
    assert!(is_refused(executor.run_transaction(&system_tx())), "the system transaction");
    assert_eq!(*executor.evm().inspector(), Seen::default(), "ran nothing");
}
