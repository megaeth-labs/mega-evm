//! Every rewrite shape, at every callback that can carry it.
//!
//! `tests/rex7/measured_inspector.rs` pins one mechanism per test, chosen because each is a
//! different half of the measurement shim. This module asks the complementary question: not "does
//! each mechanism work" but "is there a callback on the `Inspector` trait, or a rewrite shape a
//! callback admits, that nothing measures". So the cases here are laid out as a matrix over the
//! trait's own surface — one row per callback, one column per rewrite shape — rather than over the
//! shapes any particular tool is known to use. A callback added upstream, or a shape a callback
//! newly admits, shows up as an empty cell.
//!
//! The matrix is machine-checked, not documented: [`test_the_matrix_leaves_no_cell_unaccounted`]
//! enumerates every row × column pair and requires each to be either covered by a case below or
//! named in [`inapplicable`] with the reason it cannot exist. A doc table would go stale the first
//! time a callback grew a mutable argument.
//!
//! # What every cell asserts
//!
//! - **The ledger recorded what the cheat did**, on the lane it belongs to and to the gas — an
//!   under-booked lane is what makes a transaction's reported numbers a fiction, and an over-booked
//!   one is the same failure with the sign flipped.
//! - **The conservation law closes** against the envelope the receipt reports. This is the
//!   assertion the whole ledger exists to keep true, and it is the one that goes red when a lane is
//!   missed.
//! - **The state agrees with the result the caller was handed.** A cheat that fails a frame must
//!   leave that frame's writes rolled back, and one that revives a reverted frame must leave them
//!   committed — the journal decision is taken after the last rewrite, so it has to follow it.
//!
//! # Which loop runs
//!
//! The matrix runs on the inspected loops, because that is where a callback exists at all.
//! [`test_the_matrix_is_inert_with_the_inspected_loops_switched_off`] samples it against the plain
//! loop through `set_inspector_enabled`: the same inspector, the same context, the same
//! transaction, and no callback — every sampled cell must then be bit-identical to a run with no
//! inspector attached, which is what says the shim itself contributes nothing.

use crate::common::{CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Address, Bytes, Log, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    AdditionalLimit, ConservationTerms, EmptyExternalEnv, EvmTxRuntimeLimits, InspectorLedger,
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
    MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, LOG1, MSTORE, MSTORE8, POP, RETURN, SSTORE, STOP},
    context::{result::ExecutionResult, tx::TxEnvBuilder, ContextTr, JournalTr},
    handler::{EvmTr, FrameResult},
    interpreter::{
        interpreter_types::{Jumps, LoopControl, MemoryTr, StackTr},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        Interpreter, InterpreterAction, InterpreterResult, InterpreterTypes,
    },
    state::EvmState,
    Inspector,
};
use std::{collections::BTreeMap, string::String, vec::Vec};

/// High enough that EVM gas is never what binds.
const TX_GAS_LIMIT: u64 = 100_000_000;
/// Gas the fixture's inner `CALL` forwards.
const INNER_CALL_GAS: u64 = 2_000_000;

/// Gas an injecting cheat writes into a live interpreter's counter.
const INJECT: u64 = 3_000;
/// Gas a draining cheat takes out of one.
const DRAIN: u64 = 1_000;
/// Gas an envelope cheat adds to, or removes from, a frame input's `gas_limit`.
const ENVELOPE: u64 = 5_000;
/// Gas a result cheat adds to, or removes from, a frame result's remaining gas.
const RESULT: u64 = 2_000;
/// Gas an action cheat adds to, or removes from, the gas a pending `InterpreterAction` carries.
const ACTION: u64 = 1_500;

/// Slot the top frame writes, last of all, so a cheat that fails the top frame is visible.
const TOP_SLOT: u64 = 0x10;
/// Slot the inner `CALL`'s callee writes.
const CALLEE_SLOT: u64 = 0x20;
/// Slot the fixture's constructor writes.
const INIT_SLOT: u64 = 0x30;
/// Value every fixture write stores, so a stack cheat that bumps it is visible as `2`.
const STORED: u64 = 1;

/// The address the fixture's `CREATE` deploys to.
fn deployed_address() -> Address {
    CONTRACT.create(0)
}

// --- rows and columns -----------------------------------------------------------------------

/// One row of the matrix: a callback on the `Inspector` trait.
///
/// Every method of the trait is here. `log` is the one that never fires in these fixtures — it is
/// reached only when a precompile's logs are forwarded, which no wired `MegaETH` precompile
/// produces — and [`inapplicable`] carries that, together with the reason its every column is
/// empty anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum At {
    InitializeInterp,
    Step,
    StepEnd,
    Log,
    LogFull,
    FrameStart,
    FrameEnd,
    Call,
    CallEnd,
    Create,
    CreateEnd,
    Selfdestruct,
}

impl At {
    const ALL: [Self; 12] = [
        Self::InitializeInterp,
        Self::Step,
        Self::StepEnd,
        Self::Log,
        Self::LogFull,
        Self::FrameStart,
        Self::FrameEnd,
        Self::Call,
        Self::CallEnd,
        Self::Create,
        Self::CreateEnd,
        Self::Selfdestruct,
    ];
}

/// One column of the matrix: a shape a rewrite can take.
///
/// The columns are the rewrite's *mechanism*, not its purpose: what argument it reaches through
/// and in which direction it moves it. Two shapes that move the same argument in opposite
/// directions are separate columns because the ledger's sign convention is exactly that
/// distinction, and a lane that books one direction and drops the other is a real failure mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Shape {
    /// Write gas into a live interpreter's counter.
    InjectGas,
    /// Take gas out of one.
    DrainGas,
    /// Raise the `gas_limit` a frame is about to be built with.
    RaiseEnvelope,
    /// Lower it.
    LowerEnvelope,
    /// Edit a semantic field of a frame's inputs — what the frame will do, not what it costs.
    EditInput,
    /// Return a synthetic outcome, so no frame is built at all.
    Intercept,
    /// Raise the gas a finished frame hands back to its caller.
    RaiseResultGas,
    /// Lower it.
    LowerResultGas,
    /// Rewrite a successful frame result into a failure.
    FailResult,
    /// Rewrite a failed frame result into a success.
    ReviveResult,
    /// Raise the gas a pending `Return` action carries — the gas the frame it ends will hand back.
    RaiseActionResultGas,
    /// Lower it.
    LowerActionResultGas,
    /// Raise the `gas_limit` a pending `NewFrame` action carries — the envelope of the child the
    /// frame is suspending into.
    RaiseActionEnvelope,
    /// Lower it.
    LowerActionEnvelope,
    /// Edit the interpreter's stack or memory — the frame's working state, which the EVM reads
    /// back as operands and as data.
    EditStackOrMemory,
    /// Write to the journal directly, behind the EVM's back.
    JournalWrite,
}

impl Shape {
    const ALL: [Self; 16] = [
        Self::InjectGas,
        Self::DrainGas,
        Self::RaiseEnvelope,
        Self::LowerEnvelope,
        Self::EditInput,
        Self::Intercept,
        Self::RaiseResultGas,
        Self::LowerResultGas,
        Self::FailResult,
        Self::ReviveResult,
        Self::RaiseActionResultGas,
        Self::LowerActionResultGas,
        Self::RaiseActionEnvelope,
        Self::LowerActionEnvelope,
        Self::EditStackOrMemory,
        Self::JournalWrite,
    ];

    /// Whether this shape reaches through the interpreter's *pending action* rather than through
    /// the interpreter itself.
    const fn is_pending_action(self) -> bool {
        matches!(
            self,
            Self::RaiseActionResultGas |
                Self::LowerActionResultGas |
                Self::RaiseActionEnvelope |
                Self::LowerActionEnvelope
        )
    }
}

/// Why a row × column pair cannot be covered, for every pair the matrix leaves out.
///
/// A cell is left out only when the callback's signature makes the shape unreachable, or when
/// reaching it is a different mechanism that its own test already pins. Anything else is a hole.
fn inapplicable(at: At, shape: Shape) -> Option<&'static str> {
    use At::*;
    use Shape::*;

    // Callbacks that receive nothing mutable but the context.
    if at == Log {
        return match shape {
            JournalWrite => Some(
                "`log` is reached only by the precompile-log forwarding, which no wired MegaETH \
                 precompile produces; the forwarding itself is pinned by \
                 `execution.rs::test_precompile_logs_reach_the_inspector_from_both_places_they_live`",
            ),
            _ => Some("`log` takes the log by value and no interpreter or frame input at all"),
        };
    }
    if at == Selfdestruct {
        return Some(
            "`selfdestruct` takes every argument by value and is handed no context, so it has no \
             mutable surface; `test_a_selfdestruct_only_inspector_moves_nothing` pins that the \
             shim still forwards it",
        );
    }

    let interpreter_facing = matches!(at, InitializeInterp | Step | StepEnd | LogFull);
    let input_facing = matches!(at, FrameStart | Call | Create);
    let result_facing = matches!(at, FrameEnd | CallEnd | CreateEnd);

    if shape.is_pending_action() {
        return match at {
            StepEnd => None,
            InitializeInterp | Step | LogFull => Some(
                "no action is pending at this callback: revm's inspected loop breaks out as soon \
                 as one is set, so `step` and `log_full` only ever run with none, and \
                 `initialize_interp` runs before the loop on a fresh interpreter",
            ),
            _ => Some("no live interpreter is reachable from this callback"),
        };
    }

    match shape {
        InjectGas | DrainGas | EditStackOrMemory if !interpreter_facing => {
            Some("no live interpreter is reachable from this callback")
        }
        RaiseEnvelope | LowerEnvelope | EditInput | Intercept if !input_facing => Some(
            "this callback receives no frame input it can build a frame from: the `*_end` \
             callbacks take theirs by shared reference, after the frame has already run",
        ),
        RaiseResultGas | LowerResultGas | FailResult | ReviveResult if !result_facing => {
            Some("no frame result exists yet at this callback")
        }
        ReviveResult if at == CreateEnd => Some(
            "refused outright rather than measured — `test_reviving_a_failed_creation_is_refused` \
             in `measured_inspector.rs` pins the refusal at `create_end`, and \
             `test_reviving_a_failed_creation_is_refused_at_frame_end` pins it one callback later",
        ),
        _ => None,
    }
}

// --- the cheating inspector -----------------------------------------------------------------

/// Applies one cell's rewrite, once, and records what it actually did.
///
/// Firing once rather than on every callback is what makes the ledger assertions exact: the cell
/// says "this many gas, on this lane", and a trickle would only support an inequality.
#[derive(Debug)]
struct Cheat {
    at: At,
    shape: Shape,
    /// How many times the cheat fired. Every cell asserts this is 1, so a fixture that stops
    /// reaching a callback fails loudly instead of passing as a run that cheated nothing.
    fired: u32,
    /// Ordinal of the `step` / `step_end` callback the interpreter-facing rows fire at.
    step_at: u64,
    steps: u64,
    /// Gas actually moved on the interpreter lane, as the cheat measured it.
    moved_gas: i128,
}

impl Cheat {
    fn new(at: At, shape: Shape) -> Self {
        Self { at, shape, fired: 0, step_at: 8, steps: 0, moved_gas: 0 }
    }

    /// Whether this callback is the cheat's row, and the cheat has not fired yet.
    fn arm(&self, at: At) -> bool {
        at == self.at && self.fired == 0
    }

    /// Applies an interpreter-facing shape.
    fn hit_interpreter<INTR: InterpreterTypes>(&mut self, interp: &mut Interpreter<INTR>) {
        match self.shape {
            Shape::InjectGas => {
                interp.gas.erase_cost(INJECT);
                self.moved_gas += i128::from(INJECT);
                self.fired += 1;
            }
            Shape::DrainGas => {
                assert!(
                    interp.gas.record_regular_cost(DRAIN),
                    "the fixture must leave enough gas for a {DRAIN} gas removal to land",
                );
                self.moved_gas -= i128::from(DRAIN);
                self.fired += 1;
            }
            Shape::EditStackOrMemory => {
                // Bump the value an `SSTORE` is about to write, so the edit is visible in the
                // produced state rather than only in the absence of an accounting change.
                let [key, value] =
                    interp.stack.popn::<2>().expect("an SSTORE has both its operands on the stack");
                assert!(interp.stack.push(value.wrapping_add(U256::from(1))));
                assert!(interp.stack.push(key));
                self.fired += 1;
            }
            _ => unreachable!("{:?} is not an interpreter-facing shape", self.shape),
        }
    }

    /// Overwrites the frame's first memory word — the edit the three interpreter-facing rows
    /// that cannot safely touch the stack use instead.
    ///
    /// `initialize_interp` runs before the frame has any memory and before any operand exists, so
    /// there it leaves a word at the bottom of the stack, under everything the frame will push;
    /// `step_end` and `log_full` run between opcodes, where a pushed word would be consumed as the
    /// next opcode's operand and would change the fixture rather than cheat inside it.
    fn hit_frame_state<INTR: InterpreterTypes>(&mut self, interp: &mut Interpreter<INTR>) {
        if interp.memory.size() >= 32 {
            interp.memory.set(0, &[0xAB; 32]);
        } else {
            assert!(interp.stack.push(U256::from(0xDEADu64)));
        }
        self.fired += 1;
    }

    /// Applies a shape that reaches through the interpreter's *pending action* — the object the
    /// terminating or suspending instruction just left behind, which carries its own copy of the
    /// gas the frame is handing on.
    ///
    /// Leaves the action alone and does not count as fired when the pending action is not the
    /// variant this shape targets, so the cheat lands on the first `step_end` that offers the
    /// right one rather than on whichever comes first.
    fn hit_pending_action<INTR: InterpreterTypes>(&mut self, interp: &mut Interpreter<INTR>) {
        match (self.shape, interp.bytecode.action()) {
            (Shape::RaiseActionResultGas, Some(InterpreterAction::Return(result))) => {
                result.gas.erase_cost(ACTION);
            }
            (Shape::LowerActionResultGas, Some(InterpreterAction::Return(result))) => {
                assert!(
                    result.gas.record_regular_cost(ACTION),
                    "the fixture must leave the action enough gas for a {ACTION} gas removal",
                );
            }
            (
                Shape::RaiseActionEnvelope,
                Some(InterpreterAction::NewFrame(FrameInput::Call(inputs))),
            ) => inputs.gas_limit += ACTION,
            (
                Shape::LowerActionEnvelope,
                Some(InterpreterAction::NewFrame(FrameInput::Call(inputs))),
            ) => inputs.gas_limit -= ACTION,
            _ => return,
        }
        self.fired += 1;
    }

    /// Applies the one shape that reaches past the EVM entirely.
    fn hit_journal<CTX: ContextTr>(&mut self, context: &mut CTX) {
        context.journal_mut().tstore(CONTRACT, U256::from(0xF00Du64), U256::from(1));
        self.fired += 1;
    }

    /// Applies an input-facing shape to a call's inputs, or intercepts the frame.
    fn hit_call_inputs(&mut self, inputs: &mut CallInputs) -> Option<CallOutcome> {
        match self.shape {
            Shape::RaiseEnvelope => {
                inputs.gas_limit += ENVELOPE;
                self.fired += 1;
                None
            }
            Shape::LowerEnvelope => {
                inputs.gas_limit -= ENVELOPE;
                self.fired += 1;
                None
            }
            Shape::EditInput => {
                // A static call: the callee's `SSTORE` now fails, which is a change to what the
                // frame does rather than to what it is allowed to spend.
                inputs.is_static = true;
                self.fired += 1;
                None
            }
            Shape::Intercept => {
                self.fired += 1;
                Some(CallOutcome::new(
                    InterpreterResult::new(
                        InstructionResult::Stop,
                        Bytes::new(),
                        Gas::new(inputs.gas_limit),
                    ),
                    inputs.return_memory_offset.clone(),
                ))
            }
            _ => unreachable!("{:?} is not an input-facing shape", self.shape),
        }
    }

    /// Applies an input-facing shape to a creation's inputs, or intercepts the frame.
    fn hit_create_inputs(&mut self, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        match self.shape {
            Shape::RaiseEnvelope => {
                inputs.set_gas_limit(inputs.gas_limit() + ENVELOPE);
                self.fired += 1;
                None
            }
            Shape::LowerEnvelope => {
                inputs.set_gas_limit(inputs.gas_limit() - ENVELOPE);
                self.fired += 1;
                None
            }
            Shape::EditInput => {
                // Init code that reverts immediately: PUSH1 0, PUSH1 0, REVERT.
                inputs.set_init_code(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]));
                self.fired += 1;
                None
            }
            Shape::Intercept => {
                self.fired += 1;
                Some(CreateOutcome::new(
                    InterpreterResult::new(
                        InstructionResult::Stop,
                        Bytes::new(),
                        Gas::new(inputs.gas_limit()),
                    ),
                    None,
                ))
            }
            _ => unreachable!("{:?} is not an input-facing shape", self.shape),
        }
    }

    /// Applies a result-facing shape to a finished frame's result.
    fn hit_result(&mut self, result: &mut InterpreterResult) {
        match self.shape {
            Shape::RaiseResultGas => {
                result.gas.erase_cost(RESULT);
                self.fired += 1;
            }
            Shape::LowerResultGas => {
                assert!(
                    result.gas.record_regular_cost(RESULT),
                    "the fixture must leave the frame enough gas for a {RESULT} gas removal",
                );
                self.fired += 1;
            }
            Shape::FailResult => {
                assert!(
                    result.result.is_ok(),
                    "the fixture must hand this cell a successful frame"
                );
                result.result = InstructionResult::Revert;
                self.fired += 1;
            }
            Shape::ReviveResult => {
                assert!(
                    result.result.is_revert(),
                    "the fixture must hand this cell a reverted frame, got {:?}",
                    result.result,
                );
                result.result = InstructionResult::Stop;
                self.fired += 1;
            }
            _ => unreachable!("{:?} is not a result-facing shape", self.shape),
        }
    }
}

/// Whether a frame input is the fixture's inner call — the one frame every input-facing and
/// result-facing cell targets, so the top-level frame is never the one rewritten.
fn is_inner_call(input: &FrameInput) -> bool {
    matches!(input, FrameInput::Call(inputs) if inputs.target_address == CALLEE)
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for Cheat {
    fn initialize_interp(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        if !self.arm(At::InitializeInterp) {
            return;
        }
        match self.shape {
            Shape::JournalWrite => self.hit_journal(context),
            Shape::EditStackOrMemory => self.hit_frame_state(interp),
            _ => self.hit_interpreter(interp),
        }
    }

    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.steps += 1;
        if !self.arm(At::Step) {
            return;
        }
        match self.shape {
            Shape::JournalWrite if self.steps == self.step_at => self.hit_journal(context),
            // Fire on the first `SSTORE` the transaction reaches, whose operands are on the stack
            // and about to be consumed.
            Shape::EditStackOrMemory if interp.bytecode.opcode() == SSTORE => {
                self.hit_interpreter(interp)
            }
            Shape::EditStackOrMemory | Shape::JournalWrite => {}
            _ if self.steps == self.step_at => self.hit_interpreter(interp),
            _ => {}
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        if !self.arm(At::StepEnd) {
            return;
        }
        // The action shapes fire on the first `step_end` that offers the action variant they
        // target, not on a fixed ordinal — only a handful of opcodes leave an action behind.
        if self.shape.is_pending_action() {
            self.hit_pending_action(interp);
            return;
        }
        if self.steps != self.step_at {
            return;
        }
        match self.shape {
            Shape::JournalWrite => self.hit_journal(context),
            Shape::EditStackOrMemory => self.hit_frame_state(interp),
            _ => self.hit_interpreter(interp),
        }
    }

    fn log_full(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX, _log: Log) {
        if !self.arm(At::LogFull) {
            return;
        }
        match self.shape {
            Shape::JournalWrite => self.hit_journal(context),
            Shape::EditStackOrMemory => self.hit_frame_state(interp),
            _ => self.hit_interpreter(interp),
        }
    }

    fn frame_start(
        &mut self,
        context: &mut CTX,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        if !self.arm(At::FrameStart) || !is_inner_call(frame_input) {
            return None;
        }
        if self.shape == Shape::JournalWrite {
            self.hit_journal(context);
            return None;
        }
        let FrameInput::Call(inputs) = frame_input else { unreachable!() };
        self.hit_call_inputs(inputs).map(FrameResult::Call)
    }

    fn frame_end(&mut self, context: &mut CTX, frame_input: &FrameInput, result: &mut FrameResult) {
        if !self.arm(At::FrameEnd) || !is_inner_call(frame_input) {
            return;
        }
        if self.shape == Shape::JournalWrite {
            self.hit_journal(context);
            return;
        }
        self.hit_result(result.interpreter_result_mut());
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if !self.arm(At::Call) || inputs.target_address != CALLEE {
            return None;
        }
        if self.shape == Shape::JournalWrite {
            self.hit_journal(context);
            return None;
        }
        self.hit_call_inputs(inputs)
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if !self.arm(At::CallEnd) || inputs.target_address != CALLEE {
            return;
        }
        if self.shape == Shape::JournalWrite {
            self.hit_journal(context);
            return;
        }
        self.hit_result(&mut outcome.result);
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        if !self.arm(At::Create) {
            return None;
        }
        if self.shape == Shape::JournalWrite {
            self.hit_journal(context);
            return None;
        }
        self.hit_create_inputs(inputs)
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if !self.arm(At::CreateEnd) {
            return;
        }
        if self.shape == Shape::JournalWrite {
            self.hit_journal(context);
            return;
        }
        self.hit_result(&mut outcome.result);
    }
}

// --- fixtures -------------------------------------------------------------------------------

/// Which contract the fixture's inner `CALL` reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Fixture {
    /// The callee writes storage and returns. Every cell that rewrites a *successful* frame uses
    /// this one.
    ReturningCallee,
    /// The callee writes storage and reverts. The two `ReviveResult` cells need a failed frame to
    /// revive, and need the write behind it to be one the journal has already decided to roll
    /// back — so that a revival that commits it is visible.
    RevertingCallee,
}

/// Init code that writes [`INIT_SLOT`] and returns two bytes of runtime code.
fn init_code() -> Vec<u8> {
    BytecodeBuilder::default()
        .sstore(U256::from(INIT_SLOT), U256::from(STORED))
        .push_number(0x6000u64)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(2u64) // size
        .push_number(30u64) // offset: the last two bytes of the word just stored
        .append(RETURN)
        .build()
        .to_vec()
}

/// The transaction's entry contract: one `LOG1`, one inner `CALL`, one `CREATE`, one `SSTORE`.
///
/// One fixture rather than one per row, so that every callback fires in the same transaction and
/// a cell's assertions are about the cheat rather than about which fixture it got.
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
        .append(LOG1)
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
    // The init code, byte by byte, into memory at offset 0.
    for (offset, byte) in init.iter().enumerate() {
        builder = builder.push_number(u64::from(*byte)).push_number(offset as u64).append(MSTORE8);
    }
    builder
        .push_number(init.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .sstore(U256::from(TOP_SLOT), U256::from(STORED))
        .append(STOP)
        .build()
}

fn callee_code(fixture: Fixture) -> Bytes {
    let builder = BytecodeBuilder::default().sstore(U256::from(CALLEE_SLOT), U256::from(STORED));
    match fixture {
        Fixture::ReturningCallee => builder.append(STOP).build(),
        Fixture::RevertingCallee => builder.revert().build(),
    }
}

fn db_for(fixture: Fixture) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, caller_code())
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        .account_code(CALLEE, callee_code(fixture))
}

// --- running one cell -----------------------------------------------------------------------

/// Everything one transaction reports, plus what the shim booked for it.
struct Reading {
    result: ExecutionResult<MegaHaltReason>,
    compute_gas: u64,
    enforced: u64,
    destroyed: u64,
    data_size: u64,
    kv_updates: u64,
    state_growth: u64,
    gas_used: u64,
    total_gas_spent: u64,
    terms: ConservationTerms,
    ledger: InspectorLedger,
    state: EvmState,
}

impl Reading {
    /// A storage slot as the produced state has it, zero when the transaction never wrote it.
    fn slot(&self, address: Address, slot: u64) -> U256 {
        self.state
            .get(&address)
            .and_then(|account| account.storage.get(&U256::from(slot)))
            .map(|value| value.present_value())
            .unwrap_or_default()
    }

    /// Whether the fixture's `CREATE` left code behind.
    fn deployed(&self) -> bool {
        self.state
            .get(&deployed_address())
            .is_some_and(|account| !account.info.is_empty_code_hash())
    }

    /// The produced state, rendered order-independently for a bit-for-bit comparison.
    fn render_state(&self) -> String {
        let canonical: BTreeMap<Address, String> = self
            .state
            .iter()
            .map(|(address, account)| {
                let storage: BTreeMap<U256, U256> = account
                    .storage
                    .iter()
                    .map(|(slot, value)| (*slot, value.present_value()))
                    .collect();
                (
                    *address,
                    std::format!(
                        "{:?}/{}/{:?}/{storage:?}",
                        account.info.balance,
                        account.info.nonce,
                        account.info.code_hash
                    ),
                )
            })
            .collect();
        std::format!("{canonical:?}")
    }
}

/// The conservation identity, stated with the term the measurement shim contributes.
///
/// This is the assertion that goes red when a lane the shim should have booked went unbooked: the
/// two sides then disagree by precisely the unbooked amount.
fn assert_identity(label: &str, r: &Reading) {
    assert_eq!(
        r.compute_gas,
        r.enforced + r.destroyed,
        "{label}: reported compute must split into enforced + destroyed",
    );
    assert_eq!(
        r.terms.inspector_conjured_gas,
        r.ledger.conjured_gas(),
        "{label}: the law's `I` term is the ledger's net, and nothing else",
    );
    assert_eq!(
        r.terms.envelope_for(r.destroyed),
        i128::from(r.total_gas_spent),
        "{label}: the law must close against the envelope the receipt reports; \
         reported compute={} destroyed={} envelope={} ({})",
        r.compute_gas,
        r.destroyed,
        r.total_gas_spent,
        r.terms,
    );
}

fn tx() -> MegaTransaction {
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(TX_GAS_LIMIT).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn context(db: &mut MemoryDatabase) -> MegaContext<&mut MemoryDatabase, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, MegaSpecId::REX7)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context
}

fn read(limit: &AdditionalLimit, outcome: MegaTransactionOutcome) -> Reading {
    assert_eq!(
        outcome.inspector_ledger,
        limit.inspector_ledger(),
        "the outcome must report the ledger the shim booked, unchanged",
    );
    let gas_used = outcome.result_and_state.result.tx_gas_used();
    let total_gas_spent = outcome.result_and_state.result.gas().total_gas_spent();
    Reading {
        result: outcome.result_and_state.result,
        compute_gas: outcome.compute_gas_used,
        enforced: outcome.compute_gas_enforced,
        destroyed: outcome.compute_gas_destroyed,
        data_size: outcome.data_size,
        kv_updates: outcome.kv_updates,
        state_growth: outcome.state_growth_used,
        gas_used,
        total_gas_spent,
        terms: limit.conservation_terms(),
        ledger: outcome.inspector_ledger,
        state: outcome.result_and_state.state,
    }
}

/// Runs the fixture with no inspector at all.
fn transact_plain(fixture: Fixture) -> Reading {
    let mut db = db_for(fixture);
    let mut evm = MegaEvm::new(context(&mut db));
    let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
    let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
    reading
}

/// Runs the fixture with `cheat` attached, on the inspected loops or with them switched off.
fn transact_cheating(fixture: Fixture, cheat: &mut Cheat, inspected: bool) -> Reading {
    let mut db = db_for(fixture);
    let mut evm = MegaEvm::new(context(&mut db)).with_inspector(cheat);
    if !inspected {
        alloy_evm::Evm::set_inspector_enabled(&mut evm, false);
    }
    let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
    let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
    reading
}

// --- the matrix -----------------------------------------------------------------------------

/// One cell: a callback, a shape, and what the two must produce together.
struct Cell {
    at: At,
    shape: Shape,
    fixture: Fixture,
    /// The ledger the shim must have booked, exactly.
    ledger: InspectorLedger,
    /// What the produced state must show, given that the cheat landed.
    state: fn(&Reading, &str),
}

fn ledger_gas(gas: i128) -> InspectorLedger {
    InspectorLedger { gas, ..InspectorLedger::default() }
}

fn ledger_env(env: i128) -> InspectorLedger {
    InspectorLedger { env, ..InspectorLedger::default() }
}

fn ledger_result(result: i128) -> InspectorLedger {
    InspectorLedger { result, ..InspectorLedger::default() }
}

/// The ledger of a rewrite that moves no gas: the shim saw the argument it was handed come back
/// changed, and that is the whole of what it books.
///
/// These are the cells that would otherwise be indistinguishable from an observation-only run, and
/// the reason the canonical block path could not tell them apart before this lane existed.
fn ledger_intervention() -> InspectorLedger {
    InspectorLedger { interventions: 1, ..InspectorLedger::default() }
}

/// The fixture ran to its end and every frame committed: the callee's write, the deployment, and
/// the top frame's own write are all there.
fn state_all_committed(r: &Reading, label: &str) {
    assert!(
        r.result.is_success(),
        "{label}: expected a successful transaction, got {:?}",
        r.result
    );
    assert_eq!(r.slot(CALLEE, CALLEE_SLOT), U256::from(STORED), "{label}: the callee's write");
    assert!(r.deployed(), "{label}: the fixture's CREATE must have deployed code");
    assert_eq!(r.slot(CONTRACT, TOP_SLOT), U256::from(STORED), "{label}: the top frame's write");
}

/// The callee's frame did not commit: whatever ended it, its write is gone.
fn state_callee_write_rolled_back(r: &Reading, label: &str) {
    assert!(
        r.result.is_success(),
        "{label}: the caller absorbs the inner failure, got {:?}",
        r.result
    );
    assert_eq!(
        r.slot(CALLEE, CALLEE_SLOT),
        U256::ZERO,
        "{label}: a frame the caller was told failed must leave no write behind",
    );
    assert!(r.deployed(), "{label}: the rest of the transaction must be unaffected");
}

/// The callee's frame committed a write the EVM had decided to roll back — the journal followed
/// the rewritten result rather than the classification.
fn state_callee_write_revived(r: &Reading, label: &str) {
    assert!(r.result.is_success(), "{label}: {:?}", r.result);
    assert_eq!(
        r.slot(CALLEE, CALLEE_SLOT),
        U256::from(STORED),
        "{label}: a reverted frame rewritten into a success must have its state committed with it",
    );
}

/// The stack edit landed on the callee's `SSTORE` operand.
fn state_callee_write_bumped(r: &Reading, label: &str) {
    assert!(r.result.is_success(), "{label}: {:?}", r.result);
    assert_eq!(
        r.slot(CALLEE, CALLEE_SLOT),
        U256::from(STORED + 1),
        "{label}: the value the inspector put on the stack is the value the EVM wrote",
    );
}

/// The creation did not happen.
fn state_no_deployment(r: &Reading, label: &str) {
    assert!(r.result.is_success(), "{label}: the caller absorbs it, got {:?}", r.result);
    assert!(!r.deployed(), "{label}: no code may be deployed");
    assert_eq!(r.slot(deployed_address(), INIT_SLOT), U256::ZERO, "{label}: nor its storage write");
}

/// Every cell the matrix covers.
fn matrix() -> Vec<Cell> {
    use At::*;
    use Shape::*;

    let mut cells = Vec::new();
    let mut push = |at, shape, fixture, ledger, state: fn(&Reading, &str)| {
        cells.push(Cell { at, shape, fixture, ledger, state });
    };

    // The four callbacks that are handed a live interpreter.
    for at in [InitializeInterp, Step, StepEnd, LogFull] {
        push(
            at,
            InjectGas,
            Fixture::ReturningCallee,
            ledger_gas(i128::from(INJECT)),
            state_all_committed,
        );
        push(
            at,
            DrainGas,
            Fixture::ReturningCallee,
            ledger_gas(-i128::from(DRAIN)),
            state_all_committed,
        );
        push(
            at,
            EditStackOrMemory,
            Fixture::ReturningCallee,
            InspectorLedger::default(),
            if at == Step { state_callee_write_bumped } else { state_all_committed },
        );
        push(
            at,
            JournalWrite,
            Fixture::ReturningCallee,
            InspectorLedger::default(),
            state_all_committed,
        );
    }

    // The pending action, which only `step_end` ever sees: revm's inspected loop breaks out the
    // moment one is set, so it is the one callback that runs with an instruction's action already
    // in place. Which lane the edit lands on is decided by the action's own variant — a `Return`
    // action is what the frame hands back, a `NewFrame` action is what the child is built with.
    push(
        StepEnd,
        RaiseActionResultGas,
        Fixture::ReturningCallee,
        ledger_result(i128::from(ACTION)),
        state_all_committed,
    );
    push(
        StepEnd,
        LowerActionResultGas,
        Fixture::ReturningCallee,
        ledger_result(-i128::from(ACTION)),
        state_all_committed,
    );
    push(
        StepEnd,
        RaiseActionEnvelope,
        Fixture::ReturningCallee,
        ledger_env(i128::from(ACTION)),
        state_all_committed,
    );
    push(
        StepEnd,
        LowerActionEnvelope,
        Fixture::ReturningCallee,
        ledger_env(-i128::from(ACTION)),
        state_all_committed,
    );

    // The three callbacks that are handed a frame's inputs before the frame is built. `create`
    // reaches the fixture's `CREATE`; the other two reach its inner `CALL`.
    for at in [FrameStart, Call, Create] {
        let deployment_side = at == Create;
        push(
            at,
            RaiseEnvelope,
            Fixture::ReturningCallee,
            ledger_env(i128::from(ENVELOPE)),
            state_all_committed,
        );
        push(
            at,
            LowerEnvelope,
            Fixture::ReturningCallee,
            ledger_env(-i128::from(ENVELOPE)),
            state_all_committed,
        );
        push(
            at,
            EditInput,
            Fixture::ReturningCallee,
            ledger_intervention(),
            if deployment_side { state_no_deployment } else { state_callee_write_rolled_back },
        );
        push(
            at,
            Intercept,
            Fixture::ReturningCallee,
            ledger_intervention(),
            if deployment_side { state_no_deployment } else { state_callee_write_rolled_back },
        );
        push(
            at,
            JournalWrite,
            Fixture::ReturningCallee,
            InspectorLedger::default(),
            state_all_committed,
        );
    }

    // The three callbacks that are handed a finished frame's result.
    for at in [FrameEnd, CallEnd, CreateEnd] {
        let creation = at == CreateEnd;
        push(
            at,
            RaiseResultGas,
            Fixture::ReturningCallee,
            ledger_result(i128::from(RESULT)),
            state_all_committed,
        );
        push(
            at,
            LowerResultGas,
            Fixture::ReturningCallee,
            ledger_result(-i128::from(RESULT)),
            state_all_committed,
        );
        push(
            at,
            FailResult,
            Fixture::ReturningCallee,
            ledger_intervention(),
            if creation { state_no_deployment } else { state_callee_write_rolled_back },
        );
        if !creation {
            // Reviving a reverted *call* is honoured; the creation form is refused, and the two
            // tests that pin the refusal are named in `inapplicable`.
            push(
                at,
                ReviveResult,
                Fixture::RevertingCallee,
                ledger_intervention(),
                state_callee_write_revived,
            );
        }
        push(
            at,
            JournalWrite,
            Fixture::ReturningCallee,
            InspectorLedger::default(),
            state_all_committed,
        );
    }

    cells
}

fn label(at: At, shape: Shape) -> String {
    std::format!("{at:?} × {shape:?}")
}

// --- the tests ------------------------------------------------------------------------------

/// Every cell of the matrix, run: the ledger books exactly what the cheat did, the conservation
/// law closes against the receipt, and the state agrees with the result the caller was handed.
#[test]
fn test_every_cheat_shape_is_booked_and_the_law_still_closes() {
    for cell in matrix() {
        let label = label(cell.at, cell.shape);
        let mut cheat = Cheat::new(cell.at, cell.shape);
        let reading = transact_cheating(cell.fixture, &mut cheat, true);

        assert_eq!(cheat.fired, 1, "{label}: the fixture must reach this callback exactly once");
        assert_eq!(
            reading.ledger, cell.ledger,
            "{label}: the shim must book exactly what the cheat did, and nothing else",
        );
        // The interpreter lane the cheat measured for itself and the lane the shim booked are two
        // independent readings of the same edit.
        if cheat.moved_gas != 0 {
            assert_eq!(
                reading.ledger.gas, cheat.moved_gas,
                "{label}: the shim's reading of the counter edit must match the cheat's own",
            );
        }
        assert_identity(&label, &reading);
        (cell.state)(&reading, &label);
    }
}

/// The matrix has an entry, or a stated reason not to, for every callback × shape pair.
///
/// This is what keeps the coverage honest as the trait moves: a callback revm adds, or a mutable
/// argument a callback grows, produces a pair that is neither covered nor explained, and this test
/// names it. A table in a doc comment could not.
#[test]
fn test_the_matrix_leaves_no_cell_unaccounted() {
    let covered: Vec<(At, Shape)> = matrix().iter().map(|c| (c.at, c.shape)).collect();
    let mut holes = Vec::new();
    let (mut tested, mut excused) = (0usize, 0usize);

    for at in At::ALL {
        for shape in Shape::ALL {
            match (covered.contains(&(at, shape)), inapplicable(at, shape)) {
                (true, None) => tested += 1,
                (false, Some(_)) => excused += 1,
                (true, Some(reason)) => holes.push(std::format!(
                    "{} is both covered and excused ({reason})",
                    label(at, shape)
                )),
                (false, None) => holes.push(std::format!(
                    "{} has no case and no stated reason it cannot have one",
                    label(at, shape)
                )),
            }
        }
    }

    assert!(holes.is_empty(), "the matrix has holes:\n  {}", holes.join("\n  "));
    assert_eq!(
        tested + excused,
        At::ALL.len() * Shape::ALL.len(),
        "every pair must fall into exactly one of the two buckets",
    );
    assert_eq!(tested, matrix().len(), "no cell may be listed twice");
}

/// With the inspected loops switched off, the same inspector is inert — and inertness is
/// bit-for-bit, against a run with no inspector attached at all.
///
/// This is the two-loop half of the matrix. The sample is one cell per callback family, chosen so
/// that every kind of cheat is represented: a counter edit, an envelope edit, a result edit, a
/// classification rewrite, and a journal write.
#[test]
fn test_the_matrix_is_inert_with_the_inspected_loops_switched_off() {
    let sample = [
        (At::Step, Shape::InjectGas, Fixture::ReturningCallee),
        (At::StepEnd, Shape::DrainGas, Fixture::ReturningCallee),
        (At::Call, Shape::RaiseEnvelope, Fixture::ReturningCallee),
        (At::FrameStart, Shape::Intercept, Fixture::ReturningCallee),
        (At::Create, Shape::EditInput, Fixture::ReturningCallee),
        (At::CallEnd, Shape::LowerResultGas, Fixture::ReturningCallee),
        (At::CreateEnd, Shape::FailResult, Fixture::ReturningCallee),
        (At::FrameEnd, Shape::ReviveResult, Fixture::RevertingCallee),
        (At::Step, Shape::JournalWrite, Fixture::ReturningCallee),
        (At::StepEnd, Shape::RaiseActionResultGas, Fixture::ReturningCallee),
    ];

    let mut plain: BTreeMap<Fixture, Reading> = BTreeMap::new();
    for fixture in [Fixture::ReturningCallee, Fixture::RevertingCallee] {
        plain.insert(fixture, transact_plain(fixture));
    }

    for (at, shape, fixture) in sample {
        let label = label(at, shape);
        let mut cheat = Cheat::new(at, shape);
        let off = transact_cheating(fixture, &mut cheat, false);
        let reference = &plain[&fixture];

        assert_eq!(cheat.fired, 0, "{label}: no callback may run with the inspected loops off");
        assert!(
            off.ledger.is_zero(),
            "{label}: an inert inspector books nothing: {:?}",
            off.ledger
        );
        assert_eq!(
            std::format!("{:?}", off.result),
            std::format!("{:?}", reference.result),
            "{label}"
        );
        assert_eq!(off.compute_gas, reference.compute_gas, "{label}");
        assert_eq!(off.enforced, reference.enforced, "{label}");
        assert_eq!(off.destroyed, reference.destroyed, "{label}");
        assert_eq!(off.data_size, reference.data_size, "{label}");
        assert_eq!(off.kv_updates, reference.kv_updates, "{label}");
        assert_eq!(off.state_growth, reference.state_growth, "{label}");
        assert_eq!(off.gas_used, reference.gas_used, "{label}");
        assert_eq!(off.total_gas_spent, reference.total_gas_spent, "{label}");
        assert_eq!(off.terms, reference.terms, "{label}");
        assert_eq!(off.render_state(), reference.render_state(), "{label}: the produced state");
        assert_identity(&label, &off);
    }
}

/// The two fixtures are what the cells assume they are, checked without an inspector in the way.
///
/// Every "the cheat moved this" assertion is a comparison against this baseline; if the returning
/// callee ever stopped committing its write, or the reverting one started, half the matrix would
/// assert the fixture rather than the mechanism and would still be green.
#[test]
fn test_the_fixtures_behave_as_the_cells_assume() {
    let returning = transact_plain(Fixture::ReturningCallee);
    state_all_committed(&returning, "returning callee");
    assert_identity("returning callee", &returning);

    let reverting = transact_plain(Fixture::RevertingCallee);
    assert!(reverting.result.is_success(), "the caller absorbs the revert: {:?}", reverting.result);
    assert_eq!(
        reverting.slot(CALLEE, CALLEE_SLOT),
        U256::ZERO,
        "the reverting callee's write must be rolled back without an inspector",
    );
    assert_identity("reverting callee", &reverting);
}

/// `frame_end` is the last callback that can rewrite a creation's classification, and the refusal
/// covers it too.
///
/// `measured_inspector.rs` pins the `create_end` form. This is the one callback later: revm calls
/// `create_end` first and `frame_end` after it, so an inspector that leaves `create_end` alone and
/// rewrites in `frame_end` would slip past a refusal wired only to the earlier one.
#[test]
fn test_reviving_a_failed_creation_is_refused_at_frame_end() {
    /// Rewrites a failed creation into a success, from `frame_end` only.
    struct LateReviver;

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for LateReviver {
        fn frame_end(
            &mut self,
            _context: &mut CTX,
            _frame_input: &FrameInput,
            frame_result: &mut FrameResult,
        ) {
            let FrameResult::Create(outcome) = frame_result else { return };
            if !outcome.result.result.is_ok() {
                outcome.result.result = InstructionResult::Stop;
            }
        }
    }

    // Init code that reverts immediately: PUSH1 0, PUSH1 0, REVERT.
    let init: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xfd];
    let mut builder = BytecodeBuilder::default();
    for (offset, byte) in init.iter().enumerate() {
        builder = builder.push_number(u64::from(*byte)).push_number(offset as u64).append(MSTORE8);
    }
    let code = builder
        .push_number(init.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build();

    let run = move || {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10 * ONE_ETH))
            .account_code(CONTRACT, code.clone())
            .account_balance(CONTRACT, U256::from(ONE_ETH));
        let mut inspector = LateReviver;
        let mut evm = MegaEvm::new(context(&mut db)).with_inspector(&mut inspector);
        evm.execute_transaction(tx()).map(|_| ()).map_err(|e| std::format!("{e:?}"))
    };

    assert_refused(run);
}

/// Drives `run` and asserts the shim refused the rewrite, however this build surfaces a refusal:
/// a debug build asserts (the shape is a detector, and a corpus that produces it should stop), a
/// release build fails the transaction with the same message.
fn assert_refused(run: impl Fn() -> Result<(), String>) {
    const MESSAGE: &str = "inspector rewrote a failed contract creation into a successful one";
    if cfg!(debug_assertions) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(std::boxed::Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
        std::panic::set_hook(previous);
        let payload = panicked.expect_err("the detector must fire in debug builds");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(message.contains(MESSAGE), "the assertion must name the shape; got {message:?}");
    } else {
        let error = run().expect_err("the refusal must surface as an EVMError in release builds");
        assert!(error.contains(MESSAGE), "the error must name the shape; got {error:?}");
    }
}

/// An inspector that implements only `selfdestruct` moves nothing.
///
/// The callback takes every argument by value and is handed no context, so there is nothing for
/// the shim to measure — which is exactly why it is worth pinning that the shim still *forwards*
/// it. A wrapper that dropped the callback would be invisible to every accounting assertion in
/// this file.
#[test]
fn test_a_selfdestruct_only_inspector_moves_nothing() {
    use revm::bytecode::opcode::SELFDESTRUCT;

    #[derive(Default)]
    struct Watcher {
        seen: Vec<(Address, Address, U256)>,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Watcher {
        fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
            self.seen.push((contract, target, value));
        }
    }

    let callee = BytecodeBuilder::default().push_address(CALLER).append(SELFDESTRUCT).build();
    let code = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(CALLEE)
        .push_number(u128::from(INNER_CALL_GAS))
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let build_db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10 * ONE_ETH))
            .account_code(CONTRACT, code.clone())
            .account_balance(CONTRACT, U256::from(ONE_ETH))
            .account_code(CALLEE, callee.clone())
            .account_balance(CALLEE, U256::from(ONE_ETH))
    };

    let plain = {
        let mut db = build_db();
        let mut evm = MegaEvm::new(context(&mut db));
        let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
        let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
        reading
    };

    let mut watcher = Watcher::default();
    let watched = {
        let mut db = build_db();
        let mut evm = MegaEvm::new(context(&mut db)).with_inspector(&mut watcher);
        let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
        let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
        reading
    };

    assert_eq!(watcher.seen.len(), 1, "the shim must forward the callback: {:?}", watcher.seen);
    assert_eq!(watcher.seen[0].0, CALLEE, "the self-destructing contract");
    assert_eq!(watcher.seen[0].1, CALLER, "the beneficiary");
    assert!(watched.ledger.is_zero(), "nothing to measure: {:?}", watched.ledger);
    assert_eq!(watched.compute_gas, plain.compute_gas);
    assert_eq!(watched.total_gas_spent, plain.total_gas_spent);
    assert_eq!(watched.render_state(), plain.render_state());
    assert_identity("selfdestruct", &watched);
}
