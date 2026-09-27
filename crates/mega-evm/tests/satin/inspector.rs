//! Inspectors on Satin: what a rewriting inspector gets, the one rewrite refused, and the
//! admission gate block execution keys on.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, Log, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, GasInspector, MemoryDatabase},
    DeclaredObserver, MegaContext, MegaEvm, FORBIDDEN_CREATE_REVIVAL,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, GAS, INVALID, PUSH0, REVERT, SSTORE},
    context::result::EVMError,
    inspector::NoOpInspector,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Gas, InstructionResult, Interpreter, InterpreterResult,
    },
    Database, Inspector,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000400000");
const A: Address = address!("00000000000000000000000000000000000000A1");
const B: Address = address!("00000000000000000000000000000000000000B1");
const FAKE: Address = address!("00000000000000000000000000000000000FA4E0");
const GAS_LIMIT: u64 = 1_000_000;

/// Calls `B` with `value` and stores the call's success flag in slot 0.
fn call_b_and_store_flag(value: u64) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(value)
        .push_address(B)
        .append(GAS)
        .append(CALL)
        .append(PUSH0)
        .append(SSTORE)
        .stop()
        .build()
}

/// Creates a contract from `init_code` (at most 32 bytes) and stores the address in slot 0.
fn create_and_store_address(init_code: &[u8]) -> Bytes {
    let mut word = [0u8; 32];
    word[..init_code.len()].copy_from_slice(init_code);
    BytecodeBuilder::default()
        .mstore(0, word)
        .push_number(init_code.len() as u64)
        .append_many([PUSH0, PUSH0])
        .append(CREATE)
        .append(PUSH0)
        .append(SSTORE)
        .stop()
        .build()
}

/// Rewrites what execution produces, one way per field.
#[derive(Default)]
struct Rewriter {
    opcode: u8,
    /// Gas charged after every `SSTORE`.
    charge_after_sstore: u64,
    /// Gas handed back after every `SSTORE`.
    erase_after_sstore: u64,
    /// Turns every call result into a success.
    call_succeeds: bool,
    /// Answers every call to `B` itself with a success.
    answer_calls: bool,
    /// Replaces every created address.
    created_address: Option<Address>,
    /// Turns every creation result into a success.
    create_succeeds: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Rewriter {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        self.opcode = interp.bytecode.opcode();
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<DB>,
    ) {
        if self.opcode == SSTORE {
            assert!(interp.gas.record_regular_cost(self.charge_after_sstore));
            interp.gas.erase_cost(self.erase_after_sstore);
        }
    }

    fn call(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        (self.answer_calls && inputs.target_address == B).then(|| {
            CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            )
        })
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        _inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if self.call_succeeds {
            outcome.result.result = InstructionResult::Stop;
        }
    }

    fn create_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if let Some(address) = self.created_address {
            outcome.address = Some(address);
        }
        if self.create_succeeds {
            outcome.result.result = InstructionResult::Return;
            outcome.address = Some(FAKE);
        }
    }
}

fn run_with<INSP: Inspector<MegaContext<MemoryDatabase>, EthInterpreter>>(
    db: MemoryDatabase,
    inspector: INSP,
    value: u64,
) -> Result<
    revm::context::result::ResultAndState<mega_evm::MegaHaltReason>,
    EVMError<core::convert::Infallible, mega_evm::MegaTransactionError>,
> {
    let mut evm = MegaEvm::new(context(db)).with_inspector(inspector);
    evm.transact_raw(call(CALLER, A, U256::from(value), GAS_LIMIT))
}

fn writes_two_slots() -> MemoryDatabase {
    let code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(1))
        .stop()
        .build();
    MemoryDatabase::default().account_code(A, code)
}

/// Gas an inspector charges or hands back in `step_end` lands on the receipt, exactly.
#[test]
fn test_step_end_charge_and_refund_land_on_the_receipt() {
    let gas_used = |rewriter: Rewriter| {
        let result = run_with(writes_two_slots(), rewriter, 0).unwrap();
        assert!(result.result.is_success());
        result.result.gas().tx_gas_used()
    };
    let baseline = gas_used(Rewriter::default());
    assert_eq!(
        gas_used(Rewriter { charge_after_sstore: 1_000, ..Default::default() }),
        baseline + 2_000
    );
    assert_eq!(
        gas_used(Rewriter { erase_after_sstore: 500, ..Default::default() }),
        baseline - 1_000
    );
}

/// A call result rewritten in `call_end` is what the caller sees.
#[test]
fn test_call_end_rewrite_reaches_the_caller() {
    let db = || {
        MemoryDatabase::default()
            .account_code(A, call_b_and_store_flag(0))
            .account_code(B, Bytes::from_static(&[PUSH0, PUSH0, REVERT]))
    };
    let flag = |rewriter| {
        run_with(db(), rewriter, 0).unwrap().state[&A].storage[&U256::ZERO].present_value()
    };
    assert_eq!(flag(Rewriter::default()), U256::ZERO, "B reverts");
    assert_eq!(flag(Rewriter { call_succeeds: true, ..Default::default() }), U256::from(1));
}

/// A value call the inspector answers itself moves no value, and counts no write.
#[test]
fn test_short_circuited_value_call_moves_no_value() {
    let db = MemoryDatabase::default()
        .account_code(A, call_b_and_store_flag(5))
        .account_balance(A, U256::from(100));
    let mut evm = MegaEvm::new(context(db))
        .with_inspector(Rewriter { answer_calls: true, ..Default::default() });
    let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    assert!(result.result.is_success());
    assert_eq!(
        result.state[&A].storage[&U256::ZERO].present_value(),
        U256::from(1),
        "the answer is a success"
    );
    assert_eq!(result.state[&A].info.balance, U256::from(100), "no value left A");
    assert!(result.state.get(&B).is_none_or(|b| b.info.balance.is_zero()), "no value reached B");
    assert_eq!(evm.ctx().additional_limit().usage().write_records, 1, "only A's slot 0");
}

/// An address rewritten in `create_end` is what the caller sees, but the code is where the
/// creation put it.
#[test]
fn test_create_end_address_rewrite_reaches_the_caller_not_the_code() {
    // Init code returning one byte of runtime code (0x00).
    let init_code = [0x60, 0x01, 0x60, 0x00, 0xf3];
    let db = MemoryDatabase::default().account_code(A, create_and_store_address(&init_code));
    let result =
        run_with(db, Rewriter { created_address: Some(FAKE), ..Default::default() }, 0).unwrap();
    assert!(result.result.is_success(), "{:?}", result.result);
    let stored = result.state[&A].storage[&U256::ZERO].present_value();
    assert_eq!(Address::from_word(stored.into()), FAKE, "the caller got the rewritten address");
    let created = A.create(0);
    assert!(result.state[&created].info.code.as_ref().is_some_and(|code| !code.is_empty()));
    assert!(result.state.get(&FAKE).is_none_or(|fake| fake.info.is_empty_code_hash()));
}

/// A failed creation rewritten into a success is refused: the transaction fails with the
/// refusal.
#[test]
fn test_create_revival_is_refused() {
    let db = MemoryDatabase::default()
        .account_code(A, create_and_store_address(&[PUSH0, PUSH0, REVERT]));
    let result = run_with(db, Rewriter { create_succeeds: true, ..Default::default() }, 0);
    match result {
        Err(EVMError::Custom(message)) => {
            assert!(message.starts_with(FORBIDDEN_CREATE_REVIVAL), "{message}")
        }
        other => panic!("expected the refusal, got {other:?}"),
    }

    // A creation that succeeded may be rewritten freely; the refusal is for revivals only.
    let db = MemoryDatabase::default()
        .account_code(A, create_and_store_address(&[0x60, 0x01, 0x60, 0x00, 0xf3]));
    assert!(run_with(db, Rewriter { create_succeeds: true, ..Default::default() }, 0)
        .unwrap()
        .result
        .is_success());
}

/// A creation its own frame budget stops is refused its revival too: the stop is the creation's
/// alone and latches nothing, so the creation failed as any failed creation does. Under a KV limit
/// of three, the creation's start makes two records, and the write in its init code is a third one
/// over its budget of 98% of the three, which the transaction's limit still holds; under a limit
/// of four the budget holds it, and the same creation succeeds.
#[test]
fn test_a_creation_its_frame_budget_stops_is_refused_its_revival() {
    // A write of 1 to slot 1, short enough for `create_and_store_address`.
    let init_code = BytecodeBuilder::default()
        .push_number(1_u8)
        .push_number(1_u8)
        .append(SSTORE)
        .stop()
        .build();
    let db = || MemoryDatabase::default().account_code(A, create_and_store_address(&init_code));
    let under =
        |records: u64| mega_evm::EvmTxRuntimeLimits::default().with_tx_kv_update_limit(records);
    let plain = |records: u64| {
        MegaEvm::new(context(db()).with_tx_runtime_limits(under(records)))
            .execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT))
            .unwrap()
    };
    let created = |outcome: &mega_evm::MegaTransactionOutcome| {
        outcome.state[&A]
            .storage
            .get(&U256::ZERO)
            .is_some_and(|slot| !slot.present_value().is_zero())
    };
    let roomy = plain(4);
    assert!(roomy.result.is_success() && created(&roomy), "{:?}", roomy.result);

    // Without the inspector the creation fails alone: its creator stores no address and succeeds,
    // and the transaction is not stopped.
    let stopped = plain(3);
    assert!(stopped.result.is_success(), "{:?}", stopped.result);
    assert_eq!(stopped.limit_exceeded, None, "a frame budget latches nothing");
    assert!(!created(&stopped), "the creation failed");

    let limits = under(3);
    let mut evm = MegaEvm::new(context(db()).with_tx_runtime_limits(limits))
        .with_inspector(Rewriter { create_succeeds: true, ..Default::default() });
    match evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)) {
        Err(EVMError::Custom(message)) => {
            assert!(message.starts_with(FORBIDDEN_CREATE_REVIVAL), "{message}")
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
}

/// A declared observer that writes back fails its declaration in a debug build.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a declared observer wrote to the interpreter in `step_end`")]
fn test_false_declaration_fails_in_debug() {
    let rewriter = Rewriter { charge_after_sstore: 1, ..Default::default() };
    let mut evm = MegaEvm::new(context(writes_two_slots()))
        .with_trusted_inspector(DeclaredObserver(rewriter));
    let _ = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT));
}

/// A declared call rewrite fails too.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a declared observer rewrote a call result")]
fn test_false_declaration_of_a_call_rewrite_fails_in_debug() {
    let db = MemoryDatabase::default()
        .account_code(A, call_b_and_store_flag(0))
        .account_code(B, Bytes::from_static(&[INVALID]));
    let rewriter = Rewriter { call_succeeds: true, ..Default::default() };
    let mut evm = MegaEvm::new(context(db)).with_trusted_inspector(DeclaredObserver(rewriter));
    let _ = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT));
}

/// A declared observer that only reads produces the transaction no inspector produces.
#[test]
fn test_declared_tracer_changes_nothing() {
    let db = || {
        MemoryDatabase::default()
            .account_code(A, call_b_and_store_flag(0))
            .account_code(B, Bytes::from_static(&[PUSH0, PUSH0, REVERT]))
    };
    let plain =
        MegaEvm::new(context(db())).transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    let tracer = TracingInspector::new(TracingInspectorConfig::default_parity());
    let mut evm = MegaEvm::new(context(db())).with_trusted_inspector(DeclaredObserver(tracer));
    let traced = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    assert_eq!(plain.result, traced.result);
    assert_eq!(plain.state, traced.state);
    assert_eq!(evm.inspector().0.traces().nodes().len(), 2, "the tracer saw both frames");
}

/// The admission gate: only an enabled inspector that did not arrive declared is a rewriting one.
#[test]
fn test_has_rewriting_inspector_truth_table() {
    let evm = || MegaEvm::new(context(MemoryDatabase::default()));
    assert!(!evm().has_rewriting_inspector(), "no inspector");
    assert!(evm().with_inspector(NoOpInspector).has_rewriting_inspector(), "a tool's inspector");
    assert!(
        evm().with_inspector(GasInspector::new()).has_rewriting_inspector(),
        "test utilities are tools"
    );
    assert!(!evm().with_trusted_inspector(NoOpInspector).has_rewriting_inspector());
    let tracer = TracingInspector::new(TracingInspectorConfig::default_parity());
    assert!(!evm().with_trusted_inspector(DeclaredObserver(tracer)).has_rewriting_inspector());

    let mut disabled = evm().with_inspector(Rewriter::default());
    disabled.set_inspector_enabled(false);
    assert!(!disabled.has_rewriting_inspector(), "a disabled inspector does not run");
    disabled.set_inspector_enabled(true);
    assert!(disabled.has_rewriting_inspector());
}

/// Counts frame starts and ends.
#[derive(Default)]
struct Pairs {
    calls: usize,
    call_ends: usize,
    creates: usize,
    create_ends: usize,
}

impl<CTX> Inspector<CTX, EthInterpreter> for Pairs {
    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.calls += 1;
        None
    }
    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends += 1;
    }
    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.creates += 1;
        None
    }
    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.create_ends += 1;
    }
}

/// Every frame the inspector sees start, it sees end: a frame that ran, one answered before it
/// ran (a value transfer the sender cannot fund, a latched transaction's first frame), and a
/// creation.
#[test]
fn test_frame_start_and_end_stay_paired() {
    let pairs = |db: MemoryDatabase, limits: mega_evm::EvmTxRuntimeLimits, value: u64| {
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .with_inspector(Pairs::default());
        let _ = evm.transact_raw(call(CALLER, A, U256::from(value), GAS_LIMIT)).unwrap();
        let pairs = evm.inspector();
        (pairs.calls, pairs.call_ends, pairs.creates, pairs.create_ends)
    };
    let no_limits = mega_evm::EvmTxRuntimeLimits::no_limits();
    let unfunded = MemoryDatabase::default().account_code(A, call_b_and_store_flag(5));
    assert_eq!(pairs(unfunded, no_limits, 0), (2, 2, 0, 0));
    let creates = MemoryDatabase::default().account_code(A, create_and_store_address(&[0x00]));
    assert_eq!(pairs(creates, no_limits, 0), (1, 1, 1, 1));
    let funded = MemoryDatabase::default().account_balance(CALLER, U256::from(10));
    let latched = no_limits.with_tx_data_size_limit(0);
    assert_eq!(pairs(funded, latched, 1), (1, 1, 0, 0), "the stopped first frame ends too");
}

/// Counts every callback it gets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Recorder {
    initialize_interp: usize,
    step: usize,
    step_end: usize,
    log_full: usize,
    frame_start: usize,
    frame_end: usize,
    call: usize,
    call_end: usize,
    create: usize,
    create_end: usize,
    selfdestruct: usize,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Recorder {
    fn initialize_interp(&mut self, _: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.initialize_interp += 1;
    }
    fn step(&mut self, _: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.step += 1;
    }
    fn step_end(&mut self, _: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.step_end += 1;
    }
    fn log_full(&mut self, _: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>, _: Log) {
        self.log_full += 1;
    }
    fn frame_start(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &mut revm::interpreter::FrameInput,
    ) -> Option<revm::handler::FrameResult> {
        self.frame_start += 1;
        None
    }
    fn frame_end(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &revm::interpreter::FrameInput,
        _: &mut revm::handler::FrameResult,
    ) {
        self.frame_end += 1;
    }
    fn call(&mut self, _: &mut MegaContext<DB>, _: &mut CallInputs) -> Option<CallOutcome> {
        self.call += 1;
        None
    }
    fn call_end(&mut self, _: &mut MegaContext<DB>, _: &CallInputs, _: &mut CallOutcome) {
        self.call_end += 1;
    }
    fn create(&mut self, _: &mut MegaContext<DB>, _: &mut CreateInputs) -> Option<CreateOutcome> {
        self.create += 1;
        None
    }
    fn create_end(&mut self, _: &mut MegaContext<DB>, _: &CreateInputs, _: &mut CreateOutcome) {
        self.create_end += 1;
    }
    fn selfdestruct(&mut self, _: Address, _: Address, _: U256) {
        self.selfdestruct += 1;
    }
}

/// A contract that logs, creates a contract, calls `B` and self-destructs.
fn busy_contract() -> MemoryDatabase {
    let init_code = [0x60, 0x01, 0x60, 0x00, 0xf3];
    let mut word = [0u8; 32];
    word[..init_code.len()].copy_from_slice(&init_code);
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0])
        .append(revm::bytecode::opcode::LOG0)
        .mstore(0, word)
        .push_number(init_code.len() as u64)
        .append_many([PUSH0, PUSH0])
        .append(CREATE)
        .append(revm::bytecode::opcode::POP)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .append(GAS)
        .append(CALL)
        .append(revm::bytecode::opcode::POP)
        .push_address(B)
        .append(revm::bytecode::opcode::SELFDESTRUCT)
        .build();
    MemoryDatabase::default().account_code(A, code).account_code(B, Bytes::from_static(&[0x00]))
}

/// A declared observer forwards every callback to the inspector inside.
#[test]
fn test_declared_observer_forwards_every_callback() {
    let mut plain = MegaEvm::new(context(busy_contract())).with_inspector(Recorder::default());
    plain.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    let mut declared = MegaEvm::new(context(busy_contract()))
        .with_trusted_inspector(DeclaredObserver::new(Recorder::default()));
    declared.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();

    let seen = plain.inspector().clone();
    assert_eq!(declared.inspector().0, seen);
    assert!(
        seen.initialize_interp > 0 &&
            seen.step > 0 &&
            seen.step_end > 0 &&
            seen.log_full > 0 &&
            seen.frame_start > 0 &&
            seen.frame_end > 0 &&
            seen.call > 0 &&
            seen.call_end > 0 &&
            seen.create > 0 &&
            seen.create_end > 0 &&
            seen.selfdestruct > 0,
        "every callback ran: {seen:?}"
    );
    assert_eq!(DeclaredObserver::new(Recorder::default()).into_inner(), Recorder::default());
}

/// Edits a frame's inputs, writes to the journal, or rewrites a frame result, one per flag.
#[derive(Default)]
struct Writer {
    edit_inputs: bool,
    log_in_step: bool,
    rewrite_frame_end: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Writer {
    fn step(&mut self, _interp: &mut Interpreter<EthInterpreter>, context: &mut MegaContext<DB>) {
        if self.log_in_step {
            let log = Log { address: A, data: Default::default() };
            revm::context::JournalTr::log(revm::context::ContextTr::journal_mut(context), log);
        }
    }

    fn frame_start(
        &mut self,
        _context: &mut MegaContext<DB>,
        frame_input: &mut revm::interpreter::FrameInput,
    ) -> Option<revm::handler::FrameResult> {
        if let (true, revm::interpreter::FrameInput::Call(inputs)) = (self.edit_inputs, frame_input)
        {
            inputs.gas_limit -= 1;
        }
        None
    }

    fn frame_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        _frame_input: &revm::interpreter::FrameInput,
        frame_result: &mut revm::handler::FrameResult,
    ) {
        if self.rewrite_frame_end {
            frame_result.interpreter_result_mut().output = Bytes::from_static(b"rewritten");
        }
    }
}

fn run_declared(writer: Writer) {
    let mut evm =
        MegaEvm::new(context(writes_two_slots())).with_trusted_inspector(DeclaredObserver(writer));
    let _ = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT));
}

/// A declared observer that edits a frame's inputs fails its declaration in a debug build.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a declared observer rewrote a frame's inputs")]
fn test_false_declaration_of_an_input_edit_fails_in_debug() {
    run_declared(Writer { edit_inputs: true, ..Default::default() });
}

/// A declared observer that writes to the journal fails its declaration in a debug build.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a declared observer wrote to the journal in `step`")]
fn test_false_declaration_of_a_journal_write_fails_in_debug() {
    run_declared(Writer { log_in_step: true, ..Default::default() });
}

/// A declared observer that rewrites a frame result in `frame_end` fails its declaration in a
/// debug build.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a declared observer rewrote a frame result in `frame_end`")]
fn test_false_declaration_of_a_frame_end_rewrite_fails_in_debug() {
    run_declared(Writer { rewrite_frame_end: true, ..Default::default() });
}

/// Counts the frames an inspector sees start and the instructions it sees run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen {
    frames: usize,
    steps: usize,
}

impl<CTX> Inspector<CTX, EthInterpreter> for Seen {
    fn step(&mut self, _: &mut Interpreter<EthInterpreter>, _: &mut CTX) {
        self.steps += 1;
    }

    fn frame_start(
        &mut self,
        _: &mut CTX,
        _: &mut revm::interpreter::FrameInput,
    ) -> Option<revm::handler::FrameResult> {
        self.frames += 1;
        None
    }
}

/// A system call runs on the handler's plain system-call path wherever the engine and alloy-evm
/// make one — the pre-block calls' entry point, alloy-evm's and revm's — so the EVM's inspector
/// sees nothing of it, as alloy-evm's own EVMs have it. revm's inspecting entry point is the one
/// that hands it to the inspector, and there the call keeps every exemption a system call has: a
/// data-size limit of zero, which would stop a transaction before its first frame, stops nothing.
#[test]
fn test_only_the_inspecting_system_call_entry_point_runs_the_inspector() {
    use revm::{InspectSystemCallEvm, SystemCallEvm};
    let code = BytecodeBuilder::default().append_many([PUSH0, revm::bytecode::opcode::POP]).stop();
    let db = MemoryDatabase::default().account_code(A, code.build());
    let limits = mega_evm::EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(0);
    let mut evm =
        MegaEvm::new(context(db).with_tx_runtime_limits(limits)).with_inspector(Seen::default());

    let result = evm.transact_system_call_with_gas_limit(CALLER, A, Bytes::new(), 30_000_000);
    assert!(result.unwrap().result.is_success());
    assert!(Evm::transact_system_call(&mut evm, CALLER, A, Bytes::new())
        .unwrap()
        .result
        .is_success());
    assert!(SystemCallEvm::system_call_one_with_caller(&mut evm, CALLER, A, Bytes::new())
        .unwrap()
        .is_success());
    assert_eq!(*evm.inspector(), Seen::default(), "none of them ran the inspector");

    let inspected = InspectSystemCallEvm::inspect_one_system_call_with_caller(
        &mut evm,
        CALLER,
        A,
        Bytes::new(),
    )
    .unwrap();
    assert!(inspected.is_success(), "{inspected:?}");
    assert_eq!(*evm.inspector(), Seen { frames: 1, steps: 3 }, "PUSH0, POP, STOP");
    assert!(evm.ctx().additional_limit().latched().is_none(), "no limit holds a system call");
}
