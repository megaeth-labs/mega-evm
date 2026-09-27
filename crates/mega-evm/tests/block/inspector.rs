//! The admission gate: which inspectors reach block execution, and what a declared observer sees
//! of a block's phases.

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
};
use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    Evm, EvmFactory, FromRecoveredTx,
};
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::{BytecodeBuilder, GasInspector, MemoryDatabase},
    transaction_body_bytes, BlockLimits, DeclaredObserver, LimitUsage, MegaBlockExecutionCtx,
    MegaContext, MegaEvmFactory, MegaTransaction, MegaTransactionOutcome, MegaTxEnvelope,
    PreBlockStateSource,
};
use revm::{
    bytecode::opcode::{ADD, POP, PUSH0, SLOAD, SSTORE},
    context::{ContextTr, JournalTr},
    database::State,
    handler::FrameResult,
    inspector::NoOpInspector,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        Interpreter, InterpreterResult, InterpreterTypes,
    },
    state::EvmState,
    Inspector,
};
use std::{
    cell::Cell,
    sync::{Arc, Mutex},
};

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
fn state_with_code() -> State<MemoryDatabase> {
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

/// An inspector works with the block executor: declared an observer, it reaches block execution and
/// sees the transaction's instructions — `CONTRACT` loads a slot, adds one and stores it — each
/// taking gas or none, and the block finishes with the transaction's receipt.
#[test]
fn test_inspector_works_with_block_executor() {
    let increment = BytecodeBuilder::default()
        .append_many([PUSH0, SLOAD])
        .push_number(1_u8)
        .append(ADD)
        .append_many([PUSH0, SSTORE])
        .stop()
        .build();
    let mut db = common::database();
    db.set_account_code(common::CONTRACT, increment);
    let mut state = State::builder().with_database(db).build();
    let factory = common::factory();
    let mut executor = factory.create_executor_with_trusted_inspector(
        &mut state,
        common::evm_env(),
        common::unlimited_ctx(),
        DeclaredObserver::new(GasInspector::new()),
    );

    executor.execute_transaction(&user_tx(0, 1_000_000)).expect("the transaction executes");

    let records = executor.evm().inspector().0.records();
    let opcodes: Vec<_> = records.iter().map(|record| record.opcode.as_str()).collect();
    for opcode in ["SLOAD", "ADD", "SSTORE"] {
        assert!(opcodes.contains(&opcode), "{opcode} ran: {opcodes:?}");
    }
    for record in &records {
        assert!(record.gas_before >= record.gas_after, "no instruction gives gas: {record:?}");
    }
    let (_, result) = executor.finish().expect("the block finishes");
    assert_eq!(result.receipts.len(), 1);
}

/// The contract `CONTRACT` calls in the early-return rows.
const CONTRACT_B: Address = address!("0x3000000000000000000000000000000000000003");

/// Answers every call below the transaction's own frame itself with a success, and counts those
/// answers and every call's end.
#[derive(Default)]
struct SkipNestedCallInspector {
    calls_intercepted: Cell<u32>,
    call_ends: Cell<u32>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipNestedCallInspector {
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (context.journal().depth() > 0).then(|| {
            self.calls_intercepted.set(self.calls_intercepted.get() + 1);
            CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                0..0,
            )
        })
    }

    fn call_end(&mut self, _: &mut CTX, _: &CallInputs, _: &mut CallOutcome) {
        self.call_ends.set(self.call_ends.get() + 1);
    }
}

/// Answers every creation itself with a success and no address, and counts those answers and every
/// creation's end.
#[derive(Default)]
struct SkipCreateInspector {
    creates_intercepted: Cell<u32>,
    create_ends: Cell<u32>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipCreateInspector {
    fn create(&mut self, _: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.creates_intercepted.set(self.creates_intercepted.get() + 1);
        Some(CreateOutcome::new(
            InterpreterResult::new(
                InstructionResult::Stop,
                Bytes::new(),
                Gas::new(inputs.gas_limit()),
            ),
            None,
        ))
    }

    fn create_end(&mut self, _: &mut CTX, _: &CreateInputs, _: &mut CreateOutcome) {
        self.create_ends.set(self.create_ends.get() + 1);
    }
}

/// A legacy creation from [`common::CALLER`] running `init_code`.
fn creation_tx(nonce: u64, gas_limit: u64, init_code: Bytes) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(common::CHAIN_ID),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
    };
    common::recovered(MegaTxEnvelope::Legacy(Signed::new_unchecked(
        tx,
        Signature::test_signature(),
        B256::repeat_byte(0xc0),
    )))
}

/// Runs `tx` on an EVM over `state` under `inspector`, outside block execution: the route a tool
/// takes.
fn run_as_a_tool<'a, I>(
    state: &'a mut State<MemoryDatabase>,
    tx: &Recovered<MegaTxEnvelope>,
    inspector: &mut I,
) -> (MegaTransaction, MegaTransactionOutcome)
where
    I: Inspector<MegaContext<&'a mut State<MemoryDatabase>>>,
{
    let tx = MegaTransaction::from_recovered_tx(tx.inner(), tx.signer());
    let mut evm =
        MegaEvmFactory::new().create_evm(state, common::evm_env()).with_inspector(inspector);
    let outcome = evm.execute_transaction(tx.clone()).expect("a valid transaction");
    (tx, outcome)
}

/// An inspector that answers the calls below the transaction's own frame rewrites what execution
/// produces, so block execution refuses it before the transaction runs. On the EVM, the route a
/// tool takes, its answer stands in for the frame: an empty lane keeps the lanes aligned with the
/// frames, the answered call ends as every call does, and the transaction settles as one whose
/// nested call never ran.
#[test]
fn test_inspector_early_return_with_additional_limits() {
    let caller = BytecodeBuilder::default().call(CONTRACT_B, U256::ZERO).append(POP).stop().build();
    let database = || {
        let mut db = common::database();
        db.set_account_code(common::CONTRACT, caller.clone());
        db.set_account_code(CONTRACT_B, BytecodeBuilder::default().stop().build());
        State::builder().with_database(db).build()
    };
    let tx = user_tx(0, 1_000_000);

    let mut state = database();
    let evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_inspector(SkipNestedCallInspector::default());
    let factory = common::factory();
    let mut executor = factory.create_executor(evm, common::unlimited_ctx());
    assert!(is_refused(executor.execute_transaction(&tx)), "no route to a block");
    assert_eq!(executor.evm().inspector().calls_intercepted.get(), 0, "nothing ran");

    let mut state = database();
    let mut inspector = SkipNestedCallInspector::default();
    let (tx, outcome) = run_as_a_tool(&mut state, &tx, &mut inspector);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(inspector.calls_intercepted.get(), 1, "the nested call was answered");
    assert_eq!(inspector.call_ends.get(), 2, "and both calls ended");
    let body = transaction_body_bytes(&tx);
    assert_eq!(outcome.usage, LimitUsage { data_size: body, write_records: 0 });
    assert_eq!(outcome.gas.state, 0);
}

/// An inspector that answers a creation rewrites what execution produces, so block execution
/// refuses it before the transaction runs. On the EVM, the transaction's own creation answered
/// with a success and no address settles as a creation that never started: no account, so the
/// state gas EIP-2780 charged for it and the history of its write record come back, and the
/// transaction keeps only its body.
#[test]
fn test_inspector_early_return_create_with_additional_limits() {
    let tx = creation_tx(0, 10_000_000, Bytes::from_static(&[0x00]));

    let mut state = common::state();
    let evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_inspector(SkipCreateInspector::default());
    let factory = common::factory();
    let mut executor = factory.create_executor(evm, common::unlimited_ctx());
    assert!(is_refused(executor.execute_transaction(&tx)), "no route to a block");
    assert_eq!(executor.evm().inspector().creates_intercepted.get(), 0, "nothing ran");

    let mut state = common::state();
    let mut inspector = SkipCreateInspector::default();
    let (tx, outcome) = run_as_a_tool(&mut state, &tx, &mut inspector);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(inspector.creates_intercepted.get(), 1, "the creation was answered");
    assert_eq!(inspector.create_ends.get(), 1, "and ended");
    let body = transaction_body_bytes(&tx);
    assert_eq!(outcome.gas.state, 0, "no account, so no state gas");
    assert_eq!(outcome.gas.history_bytes, body, "and no record: the body alone");
    assert_eq!(outcome.gas.history, mega_evm::history_gas(body).unwrap());
    assert_eq!(outcome.usage, LimitUsage { data_size: body, write_records: 0 });
    let created = common::CALLER.create(0);
    assert!(outcome.state.get(&created).is_none_or(|account| account.info.is_empty()));
}
