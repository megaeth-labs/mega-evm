//! `DeclaredObserver` forwards the whole `Inspector` trait, and nothing else.
//!
//! The wrapper exists so that a node declaring a foreign tracer read-only writes one line instead
//! of a hundred, and the hundred it replaces were dangerous: every `Inspector` method has a default
//! body, so a callback revm adds and a hand-written forwarder misses is not a compile error but a
//! callback the wrapped tracer silently stops receiving. A tracer whose output is quietly short a
//! frame is worse than one that fails to build.
//!
//! Moving the forwarding into this crate does not remove that hazard, it concentrates it — there is
//! one forwarder now, and it is this crate's to keep complete. Three tests hold it:
//!
//! - [`test_every_callback_the_trait_declares_is_forwarded`] invokes each callback directly and
//!   checks the inner inspector received it. This is the pin on the set as it stands, and the
//!   overriding recorder makes a rename or a removal upstream a compile error.
//! - [`test_wrapping_changes_no_callback_a_recorder_sees`] runs one transaction twice, bare and
//!   wrapped, and compares the callback sequences element for element.
//! - [`test_wrapping_changes_no_trace_the_production_tracer_produces`] does the same with
//!   `revm-inspectors`' own tracer instead of a fixture recorder. That is the one that survives an
//!   upgrade: a callback added to the trait is added to `TracingInspector` too, so a forwarder that
//!   has not grown the new method produces a different trace here while a fixture recorder written
//!   before the upgrade would notice nothing.

use alloy_primitives::{address, Address, Bytes, Log, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DeclaredObserver, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, MSTORE, MSTORE8, POP, SELFDESTRUCT, STOP},
    handler::FrameResult,
    interpreter::{
        interpreter::EthInterpreter, CallInput, CallInputs, CallOutcome, CallScheme, CallValue,
        CreateInputs, CreateOutcome, CreateScheme, FrameInput, Gas, InstructionResult, Interpreter,
        InterpreterResult, InterpreterTypes,
    },
    Inspector,
};

/// Sends the fixture transaction.
const CALLER: Address = address!("2000000000000000000000000000000000000002");
/// The entry contract: logs, calls, creates and writes a slot.
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");
/// The callee, which self-destructs so that the last callback of the trait fires too.
const CALLEE: Address = address!("1000000000000000000000000000000000000002");
/// Where the callee sends its balance.
const BENEFICIARY: Address = address!("1000000000000000000000000000000000000003");

/// Gas the entry contract forwards to its inner call.
const INNER_CALL_GAS: u64 = 60_000;

// --- the recorder ---------------------------------------------------------------------------

/// Every callback `Inspector` declares today, in the order the trait declares them.
///
/// Restated as data so that [`test_every_callback_the_trait_declares_is_forwarded`] can compare a
/// set against it. The compile-time half of the same pin is [`Recorder`]'s impl below, which
/// overrides all of them: a callback upstream renames or removes stops this file building.
const CALLBACKS: [&str; 12] = [
    "initialize_interp",
    "step",
    "step_end",
    "log",
    "log_full",
    "frame_start",
    "frame_end",
    "call",
    "call_end",
    "create",
    "create_end",
    "selfdestruct",
];

/// Writes down the name of every callback it is handed, in order, and changes nothing.
#[derive(Default)]
struct Recorder {
    seen: Vec<&'static str>,
}

impl Recorder {
    /// The distinct callbacks this recorder was handed, so a comparison names the missing one
    /// rather than an index into a thousand-element sequence.
    fn distinct(&self) -> Vec<&'static str> {
        let mut seen: Vec<&'static str> = self.seen.clone();
        seen.sort_unstable();
        seen.dedup();
        seen
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Recorder {
    fn initialize_interp(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.seen.push("initialize_interp");
    }

    fn step(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.seen.push("step");
    }

    fn step_end(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.seen.push("step_end");
    }

    fn log(&mut self, _context: &mut CTX, _log: Log) {
        self.seen.push("log");
    }

    fn log_full(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX, _log: Log) {
        self.seen.push("log_full");
    }

    fn frame_start(
        &mut self,
        _context: &mut CTX,
        _frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        self.seen.push("frame_start");
        None
    }

    fn frame_end(
        &mut self,
        _context: &mut CTX,
        _frame_input: &FrameInput,
        _frame_result: &mut FrameResult,
    ) {
        self.seen.push("frame_end");
    }

    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.seen.push("call");
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.seen.push("call_end");
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.seen.push("create");
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.seen.push("create_end");
    }

    fn selfdestruct(&mut self, _contract: Address, _target: Address, _value: U256) {
        self.seen.push("selfdestruct");
    }
}

// --- the fixture ----------------------------------------------------------------------------

/// The code the created contract leaves behind: nothing.
fn init_code() -> Vec<u8> {
    vec![STOP]
}

/// One `LOG1`, one inner `CALL` to a self-destructing callee, one `CREATE`, one `SSTORE`.
///
/// Written so that as many of the trait's callbacks as a transaction can reach fire in a single
/// run. `log` is the one that cannot: revm calls it only for the logs a precompile emitted, and no
/// precompile this crate registers emits any. It is covered by the direct-invocation pin instead.
fn caller_code() -> Bytes {
    let init = init_code();
    let mut builder = BytecodeBuilder::default()
        // A word in memory for the LOG to read.
        .push_number(0xAAu64)
        .push_number(0u64)
        .append(MSTORE)
        // LOG1(offset=0, size=32, topic=1)
        .push_number(1u64)
        .push_number(32u64)
        .push_number(0u64)
        .append(revm::bytecode::opcode::LOG1)
        // CALL(gas, CALLEE, value=0, argsOffset=0, argsSize=0, retOffset=0, retSize=0)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(CALLEE)
        .push_number(u128::from(INNER_CALL_GAS))
        .append(CALL)
        .append(POP);
    for (offset, byte) in init.iter().enumerate() {
        builder = builder.push_number(u64::from(*byte)).push_number(offset as u64).append(MSTORE8);
    }
    builder
        .push_number(init.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .sstore(U256::from(1), U256::from(9))
        .append(STOP)
        .build()
}

fn build_db() -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, caller_code());
    db.set_account_code(
        CALLEE,
        BytecodeBuilder::default().push_address(BENEFICIARY).append(SELFDESTRUCT).build(),
    );
    db.set_account_balance(CALLEE, U256::from(1_000u64));
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000_000u64));
    db
}

fn fixture_tx() -> MegaTransaction {
    let mut tx = MegaTransaction::new(
        revm::context::tx::TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(1_000_000)
            .build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Runs the fixture with `inspector` attached to a measured shim.
///
/// Measured on both sides deliberately: the comparison is about what the wrapper forwards, so the
/// shim underneath must be the same one in both runs. A declared shim would take a different path
/// in release builds and make the two runs differ for a reason that is not the wrapper's.
fn run_with<I>(inspector: I)
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, mega_evm::EmptyExternalEnv>>,
{
    let mut db = build_db();
    let mut evm = MegaEvm::new(
        MegaContext::new(&mut db, MegaSpecId::REX7)
            .with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)),
    )
    .with_inspector(inspector);

    let outcome = evm.execute_transaction(fixture_tx()).expect("the fixture must execute");
    assert!(
        outcome.result_and_state.result.is_success(),
        "fixture check: {:?}",
        outcome.result_and_state.result,
    );
}

// --- the tests ------------------------------------------------------------------------------

/// Each of the twelve callbacks reaches the inspector inside the wrapper.
///
/// Invoked directly rather than through a transaction, so that the two callbacks no fixture can
/// reach — `log`, which only a precompile's output produces — are covered along with the rest. What
/// this cannot see is a callback added upstream, since a name that does not exist cannot be listed;
/// [`test_wrapping_changes_no_trace_the_production_tracer_produces`] is what covers that direction.
#[test]
fn test_every_callback_the_trait_declares_is_forwarded() {
    let mut wrapper = DeclaredObserver(Recorder::default());
    let inspector: &mut dyn Inspector<(), EthInterpreter> = &mut wrapper;

    let mut interpreter = Interpreter::<EthInterpreter>::default();
    let mut call_inputs = sample_call_inputs();
    let mut create_inputs = sample_create_inputs();
    let mut call_outcome = sample_call_outcome();
    let mut create_outcome = sample_create_outcome();
    let mut frame_input = FrameInput::Call(Box::new(sample_call_inputs()));
    let mut frame_result = FrameResult::Call(sample_call_outcome());

    inspector.initialize_interp(&mut interpreter, &mut ());
    inspector.step(&mut interpreter, &mut ());
    inspector.step_end(&mut interpreter, &mut ());
    inspector.log(&mut (), Log::default());
    inspector.log_full(&mut interpreter, &mut (), Log::default());
    inspector.frame_start(&mut (), &mut frame_input);
    inspector.frame_end(&mut (), &frame_input, &mut frame_result);
    inspector.call(&mut (), &mut call_inputs);
    inspector.call_end(&mut (), &call_inputs, &mut call_outcome);
    inspector.create(&mut (), &mut create_inputs);
    inspector.create_end(&mut (), &create_inputs, &mut create_outcome);
    inspector.selfdestruct(Address::ZERO, Address::ZERO, U256::ZERO);

    assert_eq!(
        wrapper.0.seen, CALLBACKS,
        "every callback the trait declares must reach the inspector inside the wrapper, once, \
         unchanged",
    );
}

/// `log_full`'s default body calls `log`, so forwarding one and not the other is not a silent
/// no-op but a rerouted callback — which is the failure a set comparison would miss.
///
/// The recorder above overrides both, so if `DeclaredObserver::log_full` were dropped the default
/// body would forward to `DeclaredObserver::log`, the inner inspector would record `log` where the
/// EVM sent `log_full`, and the assertion in
/// [`test_every_callback_the_trait_declares_is_forwarded`] would fail on the order rather than on
/// the membership. This states that dependency so a later edit does not weaken that assertion to a
/// set comparison.
#[test]
fn test_log_full_is_forwarded_as_itself_and_not_through_log() {
    let mut wrapper = DeclaredObserver(Recorder::default());
    let inspector: &mut dyn Inspector<(), EthInterpreter> = &mut wrapper;
    let mut interpreter = Interpreter::<EthInterpreter>::default();

    inspector.log_full(&mut interpreter, &mut (), Log::default());

    assert_eq!(wrapper.0.seen, ["log_full"], "the wrapper must not collapse `log_full` into `log`");
}

/// One transaction, run twice: the callbacks a recorder sees wrapped are the ones it sees bare.
///
/// The fixture reaches eleven of the twelve, nested frames and a self-destruct included, so a
/// forwarder that dropped one would show up as a shorter sequence rather than as a subtle
/// difference in what the transaction produced.
#[test]
fn test_wrapping_changes_no_callback_a_recorder_sees() {
    let mut bare = Recorder::default();
    run_with(&mut bare);

    let mut wrapped = DeclaredObserver(Recorder::default());
    run_with(&mut wrapped);

    assert_eq!(bare.distinct(), wrapped.0.distinct(), "the wrapper must not drop a whole callback",);
    assert_eq!(
        bare.seen, wrapped.0.seen,
        "and must not change the order or the number of times each one fires",
    );

    let reached = bare.distinct();
    let missing: Vec<&&str> = CALLBACKS.iter().filter(|name| !reached.contains(name)).collect();
    assert_eq!(
        missing,
        [&"log"],
        "fixture check: the fixture must keep reaching every callback a transaction can reach, so \
         that the comparison above stays worth making",
    );
}

/// The same comparison against `revm-inspectors`' own tracer, whose callback set grows with revm's.
///
/// This is the test that survives an upgrade. A callback added to the `Inspector` trait gets a
/// default body, so nothing here stops compiling and a recorder written against today's trait
/// records nothing new. `TracingInspector` is upgraded along with the trait, though — so if the
/// wrapper has not grown the new method, the tracer receives it bare and not wrapped, and the two
/// traces stop matching.
#[test]
fn test_wrapping_changes_no_trace_the_production_tracer_produces() {
    use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

    let trace_of = |wrap: bool| {
        let mut tracer = TracingInspector::new(TracingInspectorConfig::all());
        if wrap {
            run_with(DeclaredObserver(&mut tracer));
        } else {
            run_with(&mut tracer);
        }
        (tracer.traces().nodes().len(), format!("{:?}", tracer.traces().nodes()))
    };

    let (bare_frames, bare) = trace_of(false);
    let (wrapped_frames, wrapped) = trace_of(true);

    assert_eq!(
        bare_frames, 3,
        "fixture check: the tracer must have recorded the entry frame, the inner call and the \
         creation",
    );
    assert_eq!(wrapped_frames, bare_frames, "the wrapper must not cost the tracer a frame");
    assert_eq!(bare, wrapped, "the tracer must see the same execution wrapped as bare");
}

// --- sample arguments for the direct-invocation pin ------------------------------------------

fn sample_gas() -> Gas {
    Gas::new(1)
}

fn sample_call_inputs() -> CallInputs {
    CallInputs {
        input: CallInput::Bytes(Bytes::new()),
        return_memory_offset: 0..0,
        gas_limit: 1,
        reservoir: 0,
        bytecode_address: Address::ZERO,
        known_bytecode: Default::default(),
        target_address: Address::ZERO,
        caller: Address::ZERO,
        value: CallValue::Transfer(U256::ZERO),
        scheme: CallScheme::Call,
        is_static: false,
        charged_new_account_state_gas: false,
    }
}

fn sample_create_inputs() -> CreateInputs {
    CreateInputs::new(Address::ZERO, CreateScheme::Create, U256::ZERO, Bytes::new(), 1, 0)
}

fn sample_result() -> InterpreterResult {
    InterpreterResult::new(InstructionResult::Stop, Bytes::new(), sample_gas())
}

fn sample_call_outcome() -> CallOutcome {
    CallOutcome::new(sample_result(), 0..0)
}

fn sample_create_outcome() -> CreateOutcome {
    CreateOutcome::new(sample_result(), None)
}
