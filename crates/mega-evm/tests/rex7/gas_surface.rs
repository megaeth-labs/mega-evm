//! The closed enumeration of gas the `Inspector` trait puts within an inspector's reach.
//!
//! `inspector_cheat_matrix.rs` asks whether every callback × *rewrite shape* pair is covered. That
//! question is answered over a shape list this repository writes down, so it can only be as
//! complete as that list. This module asks the question one level below it, over a list
//! *upstream* writes down: is there a number that carries gas, reachable through an argument some
//! callback is handed, that nothing in `MegaETH` has classified?
//!
//! # The two levels the enumeration has
//!
//! - **Shapes.** Which objects the EVM hands a callback that carry gas at all. Every one of them
//!   arrives through an enum — `InterpreterAction`, `FrameInput`, `FrameResult` — so an exhaustive
//!   match with no catch-all is a compile-time pin: a variant added upstream stops the build.
//! - **Fields.** Which numbers inside those objects carry gas. Rust cannot enumerate a foreign
//!   struct's fields, but a derived `Debug` renders every one of them by name, so a snapshot of
//!   that name set against the classification table is a pin with the same reach: a field added
//!   upstream appears in the rendering, fails to match a table row, and the test names it.
//!
//! # What the pins cannot reach, and what covers it instead
//!
//! A callback *added* to the `Inspector` trait is not a compile error anywhere — the trait gives
//! every method a default body, so an unimplemented one silently does nothing and an unwrapped one
//! is silently unmeasured. [`test_the_callback_set_is_the_one_the_shim_wraps`] pins the set that
//! exists today by overriding all of it, which catches a rename or a removal at compile time and
//! an addition only through the upgrade obligation stated in `src/evm/AGENTS.md`. That obligation
//! is the reason the classification table lives there rather than here.

use revm::{
    handler::FrameResult,
    interpreter::{
        interpreter::EthInterpreter, CallInput, CallInputs, CallOutcome, CallScheme, CallValue,
        CreateInputs, CreateOutcome, CreateScheme, FrameInput, Gas, InstructionResult, Interpreter,
        InterpreterAction, InterpreterResult, InterpreterTypes,
    },
    primitives::{Address, Bytes, Log, U256},
    Inspector,
};
use std::{collections::BTreeSet, string::String, vec::Vec};

// --- the classification ------------------------------------------------------------------------

/// What `MegaETH` does about one field of one object an inspector can reach.
///
/// The whole point of the enum is that there is no fifth arm and no catch-all: a field is
/// measured, or it carries no gas, or it carries gas that reaches nothing `MegaETH` reports, or it
/// is a hole with a name. "Nobody looked at it" is not one of the options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coverage {
    /// Measured, and booked on the named `InspectorLedger` lane.
    Lane(&'static str),
    /// Not a gas quantity.
    NotGas(&'static str),
    /// A gas quantity that moves nothing `MegaETH` reports, with the reason it cannot.
    Inert(&'static str),
    /// A gas quantity that moves what `MegaETH` reports, and that no lane books.
    ///
    /// Carrying this verdict in the type is deliberate. A gap named in a table is a gap someone
    /// can close; a gap that is merely absent from a table is one nobody knows about.
    NotClosed(&'static str),
}

/// The five numbers a `Gas`'s tracker holds.
///
/// `remaining` is the one every lane the shim books is defined over. The other four are reachable
/// through exactly the same `&mut Gas`, on all four of the objects the table in `src/evm/AGENTS.md`
/// lists, and the verdicts here are what each was measured to do.
const GAS_TRACKER_FIELDS: [(&str, Coverage); 5] = [
    (
        "remaining",
        Coverage::Lane(
            "gas / env / result, by which object holds the `Gas` and how its frame ends",
        ),
    ),
    (
        "gas_limit",
        Coverage::Inert(
            "op-revm normalises the top-level gas object to the transaction's own limit before \
             the settlement point, and no REX7 lane reads a frame's limit — the two that do are \
             the REX4 legacy stipend's burn and rescue caps, which REX5 mode does not take",
        ),
    ),
    (
        "refunded",
        Coverage::NotClosed(
            "a refund written at any callback that holds a `Gas` travels to the caller on frame \
             return and reaches the receipt's gas used, while the conservation law — stated over \
             `limit - remaining` — stays closed and every ledger lane stays zero",
        ),
    ),
    (
        "reservoir",
        Coverage::NotClosed(
            "EIP-8037 state gas is structurally zero on every MegaETH path, so a reservoir an \
             inspector writes is gas the transaction never funded; it moves the envelope and the \
             terminal cross-check trips on it, with every ledger lane zero",
        ),
    ),
    (
        "state_gas_spent",
        Coverage::Inert(
            "the counterpart of `reservoir` on the spending side, and dead for the same reason: \
             with EIP-8037 off nothing reads it",
        ),
    ),
];

/// The memoisation of how far a frame's memory has been paid for.
const MEMORY_GAS_FIELDS: [(&str, Coverage); 2] = [
    (
        "words_num",
        Coverage::NotGas(
            "a memo of the interpreter's memory size, not a budget: editing it without editing \
             the memory desynchronises the two and the EVM reads out of bounds, which is the \
             stack-and-memory row of the table rather than a gas lane",
        ),
    ),
    ("expansion_cost", Coverage::NotGas("the memo's other half, and dead for the same reason")),
];

/// The two halves of a `Gas`.
const GAS_FIELDS: [(&str, Coverage); 2] = [
    ("tracker", Coverage::NotGas("a container; its own fields are classified separately")),
    ("memory", Coverage::NotGas("a container; its own fields are classified separately")),
];

/// Everything a call frame is built from.
const CALL_INPUTS_FIELDS: [(&str, Coverage); 12] = [
    (
        "gas_limit",
        Coverage::Lane(
            "env, at the callback boundary — or, when the same callback answers the frame itself, \
             the baseline the interception's own gas is settled against",
        ),
    ),
    (
        "reservoir",
        Coverage::NotClosed(
            "the child frame's EIP-8037 state-gas pool, and gas the caller was never debited for \
             in exactly the way a raised `gas_limit` is; no lane books it",
        ),
    ),
    ("input", Coverage::NotGas("what the frame does")),
    ("return_memory_offset", Coverage::NotGas("what the frame does")),
    ("bytecode_address", Coverage::NotGas("what the frame does")),
    ("known_bytecode", Coverage::NotGas("what the frame does")),
    ("target_address", Coverage::NotGas("what the frame does")),
    ("caller", Coverage::NotGas("what the frame does")),
    ("value", Coverage::NotGas("what the frame does")),
    ("scheme", Coverage::NotGas("what the frame does")),
    ("is_static", Coverage::NotGas("what the frame does")),
    (
        "charged_new_account_state_gas",
        Coverage::NotGas(
            "an EIP-8037 refund flag rather than an amount; the rewrite comparison books it as an \
             intervention like any other semantic field",
        ),
    ),
];

/// Everything a creation frame is built from.
const CREATE_INPUTS_FIELDS: [(&str, Coverage); 8] = [
    ("gas_limit", Coverage::Lane("env, exactly as a call's")),
    ("reservoir", Coverage::NotClosed("a creation's copy of the call form above")),
    ("caller", Coverage::NotGas("what the frame does")),
    ("scheme", Coverage::NotGas("what the frame does")),
    ("value", Coverage::NotGas("what the frame does")),
    ("init_code", Coverage::NotGas("what the frame does")),
    ("cached_address", Coverage::NotGas("a memo of the init code and scheme above")),
    ("cached_init_code_hash", Coverage::NotGas("a memo of the init code above")),
];

/// Everything a finished frame hands back.
const INTERPRETER_RESULT_FIELDS: [(&str, Coverage); 3] = [
    ("gas", Coverage::NotGas("a container; its own fields are classified separately")),
    (
        "result",
        Coverage::NotGas("the classification, booked as an intervention rather than as gas"),
    ),
    ("output", Coverage::NotGas("the returned bytes, booked as an intervention")),
];

// --- reading a struct's field names off its `Debug` ----------------------------------------------

/// The field names a derived `Debug` rendering shows at the top level of the struct it renders.
///
/// Depth-limited on purpose: a nested value's own fields belong to that value's own table row, and
/// pinning them here would make this test fail on churn in a type nothing reaches.
fn field_names(rendered: &str) -> BTreeSet<String> {
    let bytes: Vec<char> = rendered.chars().collect();
    let mut names = BTreeSet::new();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 1 => {
                // A field name follows the opening brace or a comma, with at most one space.
                let starts_a_field = (index > 0 && matches!(bytes[index - 1], '{' | ',')) ||
                    (index > 1 &&
                        bytes[index - 1] == ' ' &&
                        matches!(bytes[index - 2], '{' | ','));
                if starts_a_field && (bytes[index].is_ascii_lowercase() || bytes[index] == '_') {
                    let mut end = index;
                    while end < bytes.len() &&
                        (bytes[end].is_ascii_alphanumeric() || bytes[end] == '_')
                    {
                        end += 1;
                    }
                    if bytes.get(end) == Some(&':') {
                        names.insert(bytes[index..end].iter().collect());
                        index = end;
                        continue;
                    }
                }
            }
            _ => {}
        }
        index += 1;
    }
    names
}

/// Asserts that a struct's rendered field names are exactly the ones `table` classifies.
///
/// Both directions are checked. A field upstream added is one the table has no verdict for; a row
/// the table keeps for a field upstream removed is a verdict about nothing, and stale prose about
/// a field that no longer exists is how a table stops being evidence.
fn assert_classified(what: &str, rendered: &str, table: &[(&str, Coverage)]) {
    let seen = field_names(rendered);
    let classified: BTreeSet<String> = table.iter().map(|(name, _)| String::from(*name)).collect();
    let unclassified: Vec<&String> = seen.difference(&classified).collect();
    let vanished: Vec<&String> = classified.difference(&seen).collect();
    assert!(
        unclassified.is_empty(),
        "{what} has {} field(s) no verdict covers: {unclassified:?}\n  rendered: {rendered}",
        unclassified.len(),
    );
    assert!(
        vanished.is_empty(),
        "{what} no longer has {} classified field(s): {vanished:?}",
        vanished.len(),
    );
    assert!(!seen.is_empty(), "{what}: the rendering parsed to nothing, so nothing was checked");
}

// --- the samples the renderings are taken from ---------------------------------------------------

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

// --- the field-level pin -------------------------------------------------------------------------

/// Every field of every gas-carrying object an inspector is handed has a verdict.
///
/// This is the closure the completeness table rests on. It is not a claim that the verdicts are
/// right — the tests in `measured_inspector.rs`, `inspector_cheat_matrix.rs` and
/// `interception_gas.rs` are — it is the claim that there is no field without one.
#[test]
fn test_every_field_of_every_gas_carrier_has_a_verdict() {
    let gas = sample_gas();
    assert_classified("Gas", &std::format!("{gas:?}"), &GAS_FIELDS);
    assert_classified("GasTracker", &std::format!("{:?}", gas.tracker()), &GAS_TRACKER_FIELDS);
    assert_classified("MemoryGas", &std::format!("{:?}", gas.memory()), &MEMORY_GAS_FIELDS);
    assert_classified(
        "CallInputs",
        &std::format!("{:?}", sample_call_inputs()),
        &CALL_INPUTS_FIELDS,
    );
    assert_classified(
        "CreateInputs",
        &std::format!("{:?}", sample_create_inputs()),
        &CREATE_INPUTS_FIELDS,
    );
    assert_classified(
        "InterpreterResult",
        &std::format!("{:?}", sample_result()),
        &INTERPRETER_RESULT_FIELDS,
    );
}

/// The parser the pin rests on reads what it is supposed to read.
///
/// Without this, a change to `Debug`'s formatting that made the parser return nothing would turn
/// every assertion above into a tautology — and `assert_classified`'s emptiness check would be the
/// only thing standing in the way, which is one check too few for the thing the whole module is
/// built on.
#[test]
fn test_the_field_reader_reads_the_top_level_and_stops_there() {
    let names = field_names(
        "Outer { first: 1, nested: Inner { hidden: 2, deeper: Deepest { buried: 3 } }, \
         last: Tuple(0x00, Other { also_hidden: 4 }) }",
    );
    let expected: BTreeSet<String> =
        ["first", "nested", "last"].into_iter().map(String::from).collect();
    assert_eq!(names, expected, "only the outermost struct's own fields may be read");
    assert!(field_names("NoFields").is_empty(), "a unit struct has no fields to read");
}

/// The gaps the table carries are the gaps it says it carries.
///
/// A verdict is a claim someone has to be able to act on, so the set of open ones is pinned by
/// name rather than by count: closing one, or discovering another, has to touch this list and the
/// table in `src/evm/AGENTS.md` together.
#[test]
fn test_the_open_gaps_are_the_ones_the_table_names() {
    let mut open = Vec::new();
    for (what, table) in [
        ("Gas", GAS_FIELDS.as_slice()),
        ("GasTracker", GAS_TRACKER_FIELDS.as_slice()),
        ("MemoryGas", MEMORY_GAS_FIELDS.as_slice()),
        ("CallInputs", CALL_INPUTS_FIELDS.as_slice()),
        ("CreateInputs", CREATE_INPUTS_FIELDS.as_slice()),
        ("InterpreterResult", INTERPRETER_RESULT_FIELDS.as_slice()),
    ] {
        for (field, coverage) in table {
            if matches!(coverage, Coverage::NotClosed(_)) {
                open.push(std::format!("{what}::{field}"));
            }
        }
    }
    open.sort();
    assert_eq!(
        open,
        [
            "CallInputs::reservoir",
            "CreateInputs::reservoir",
            "GasTracker::refunded",
            "GasTracker::reservoir",
        ],
        "the open gaps moved; `src/evm/AGENTS.md`'s table has to move with them",
    );
}

// --- the shape-level pin -------------------------------------------------------------------------

/// Which object a gas-carrying shape puts within reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Carrier {
    /// A `Gas`, whose fields the tables above classify.
    Gas,
    /// A frame's `gas_limit`, a bare `u64`.
    Envelope,
    /// Nothing that carries gas.
    None,
}

/// The gas a pending action carries, by variant — no catch-all, so a variant revm adds stops the
/// build here and the shim's own `held` has to grow an arm with it.
const fn action_carrier(action: &InterpreterAction) -> Carrier {
    match action {
        InterpreterAction::Return(_) => Carrier::Gas,
        InterpreterAction::NewFrame(input) => frame_input_carrier(input),
    }
}

/// The gas a frame input carries, by variant.
const fn frame_input_carrier(input: &FrameInput) -> Carrier {
    match input {
        FrameInput::Call(_) | FrameInput::Create(_) => Carrier::Envelope,
        FrameInput::Empty => Carrier::None,
    }
}

/// The gas a frame result carries, by variant.
const fn frame_result_carrier(result: &FrameResult) -> Carrier {
    match result {
        FrameResult::Call(_) | FrameResult::Create(_) => Carrier::Gas,
    }
}

/// Every gas-carrying shape the EVM hands a callback is reached through an enum this module
/// matches exhaustively.
///
/// The assertions are the small half; the compile is the large one. A variant added to any of the
/// three enums is a build failure here, which is the only mechanical warning `MegaETH` gets that
/// upstream grew a new way to carry gas across a callback boundary.
#[test]
fn test_every_gas_carrying_shape_is_matched_without_a_catch_all() {
    let call = FrameInput::Call(std::boxed::Box::new(sample_call_inputs()));
    let create = FrameInput::Create(std::boxed::Box::new(sample_create_inputs()));

    assert_eq!(frame_input_carrier(&call), Carrier::Envelope);
    assert_eq!(frame_input_carrier(&create), Carrier::Envelope);
    assert_eq!(frame_input_carrier(&FrameInput::Empty), Carrier::None);

    assert_eq!(action_carrier(&InterpreterAction::Return(sample_result())), Carrier::Gas);
    assert_eq!(action_carrier(&InterpreterAction::NewFrame(call)), Carrier::Envelope);

    let call_result = FrameResult::Call(CallOutcome::new(sample_result(), 0..0));
    let create_result = FrameResult::Create(CreateOutcome::new(sample_result(), None));
    assert_eq!(frame_result_carrier(&call_result), Carrier::Gas);
    assert_eq!(frame_result_carrier(&create_result), Carrier::Gas);
}

// --- the callback-set snapshot -------------------------------------------------------------------

/// Every callback the `Inspector` trait has today, in the order the trait declares them.
///
/// The same twelve rows `inspector_cheat_matrix.rs` runs its shapes over, restated here because
/// the two pins answer different questions: that one asks whether each row is exercised, this one
/// asks whether the row set is still the trait's.
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

/// Overrides every callback, so that the set is pinned by the compiler rather than by the list.
struct EveryCallback {
    seen: Vec<&'static str>,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for EveryCallback {
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

/// The callback set the shim wraps is the trait's, and it is the set the tables are written over.
///
/// A callback upstream renames or removes stops [`EveryCallback`] from compiling. A callback
/// upstream *adds* does not — the trait's default bodies see to that — and there is no compile-time
/// construct that would, which is why the upgrade obligation in `src/evm/AGENTS.md` is what covers
/// that direction, and why this list is written out rather than derived.
#[test]
fn test_the_callback_set_is_the_one_the_shim_wraps() {
    let mut probe = EveryCallback { seen: Vec::new() };
    let inspector: &mut dyn Inspector<(), EthInterpreter> = &mut probe;
    inspector.selfdestruct(Address::ZERO, Address::ZERO, U256::ZERO);
    assert_eq!(probe.seen, ["selfdestruct"], "the override must be the one that runs");

    assert_eq!(CALLBACKS.len(), 12, "the trait's callback count is part of the snapshot");
    let unique: BTreeSet<&str> = CALLBACKS.into_iter().collect();
    assert_eq!(unique.len(), CALLBACKS.len(), "no callback may be listed twice");
}
