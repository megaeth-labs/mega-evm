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

use crate::{
    common::{
        base_db, call_contract_tx, context, drive, state_view, transact, Outcome, CALLEE, CALLER,
        CONTRACT, ONE_ETH,
    },
    inspector_common::{
        assert_refused, call_then_stop, db_with_callee, deploy_then_stop, ledger_env, ledger_gas,
        ledger_intervention, ledger_refund, ledger_reservoir, ledger_result, ledger_state_gas,
        limits, plain_and_cheated, try_transact_inspected, REVERTING_INIT_CODE, REVIVED_CREATION,
    },
};
use alloy_primitives::{Address, Bytes, Log, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    InspectorLedger, Lane, MegaEvm, MegaSpecId, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, LOG1, MSTORE, MSTORE8, POP, RETURN, SSTORE, STOP},
    context::{Cfg, ContextTr, JournalTr},
    handler::FrameResult,
    interpreter::{
        interpreter_types::{Jumps, LoopControl, MemoryTr, StackTr},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        Interpreter, InterpreterAction, InterpreterResult, InterpreterTypes,
    },
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
/// Gas an interception cheat's synthetic outcome hands back over, or under, the envelope it was
/// given.
const INTERCEPTION: u64 = 4_000;
/// Refund a refund cheat adds to, or removes from, a `Gas`'s refund counter.
const REFUND: i64 = 2_000;
/// The EIP-8037 pool a reservoir cheat fills.
const RESERVOIR: u64 = 3_500;
/// The EIP-8037 spend counter a state-gas cheat writes.
const STATE_GAS: i64 = 1_200;

/// Slot the top frame writes, last of all, so a cheat that fails the top frame is visible.
const TOP_SLOT: u64 = 0x10;
/// Slot the inner `CALL`'s callee writes.
const CALLEE_SLOT: u64 = 0x20;
/// Slot the fixture's constructor writes.
const INIT_SLOT: u64 = 0x30;
/// Slot every frame sets and then clears, so each ends holding a refund the EVM itself produced —
/// which is what the refund-lowering column needs to take from.
const CLEARED_SLOT: u64 = 0x40;
/// Value every fixture write stores, so a stack cheat that bumps it is visible as `2`.
const STORED: u64 = 1;

/// The address the outcome-metadata column makes a successful creation report instead of the one
/// it deployed to.
const RELABELLED_DEPLOYMENT: Address =
    alloy_primitives::address!("00000000000000000000000000000000000f00d0");

/// The address the fixture's `CREATE` deploys to.
fn deployed_address() -> Address {
    CONTRACT.create(0)
}

/// Declares one axis of the matrix, with the list of its members derived from the declaration.
///
/// The two axes are swept exhaustively by `test_the_matrix_leaves_no_cell_unaccounted`, so a
/// variant added without a corresponding entry in `ALL` would silently shrink the sweep instead of
/// failing. Deriving the list removes that possibility.
macro_rules! axis {
    (
        $(#[$meta:meta])*
        enum $name:ident { $($(#[$vmeta:meta])* $variant:ident,)* }
    ) => {
        $(#[$meta])*
        enum $name { $($(#[$vmeta])* $variant,)* }

        impl $name {
            const ALL: &'static [Self] = &[$(Self::$variant,)*];
        }
    };
}

// --- rows and columns -----------------------------------------------------------------------

/// One row of the matrix: a callback on the `Inspector` trait.
///
/// Every method of the trait is here. `log` is the one that never fires in these fixtures — it is
/// reached only when a precompile's logs are forwarded, which no wired `MegaETH` precompile
/// produces — and [`inapplicable`] carries that, together with the reason its every column is
/// empty anyway.
axis! {
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
}

/// One column of the matrix: a shape a rewrite can take.
///
/// The columns are the rewrite's *mechanism*, not its purpose: what argument it reaches through
/// and in which direction it moves it. Two shapes that move the same argument in opposite
/// directions are separate columns because the ledger's sign convention is exactly that
/// distinction, and a lane that books one direction and drops the other is a real failure mode.
///
/// The EIP-8037 dimensions are the one place that pairing does not apply: `MegaETH` runs with the
/// EIP off, so a `Gas` reaches every callback with both of its state-gas figures at zero and there
/// is nothing to lower.
axis! {
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
    /// Return one whose gas hands back more than the envelope it was given.
    RaiseInterceptionGas,
    /// Return one whose gas hands back less.
    LowerInterceptionGas,
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
    /// Add to a `Gas`'s refund counter — what the sender is billed, which the envelope does not
    /// reach.
    RaiseRefund,
    /// Take from one.
    LowerRefund,
    /// Fill a `Gas`'s or a frame input's EIP-8037 state-gas pool.
    WriteReservoir,
    /// Write a `Gas`'s EIP-8037 spend counter.
    WriteStateGas,
    /// Edit the interpreter's stack or memory — the frame's working state, which the EVM reads
    /// back as operands and as data.
    ///
    /// Two different rewrites share this column, and the shim can see one of them. A push or a pop
    /// moves the stack's *length*, which is a constant-time reading the shim takes; overwriting a
    /// word in place moves neither length and is the contents rewrite that has no lane. Which one
    /// a row gets is decided by what the frame is doing at that callback, and each row's cell
    /// states the ledger that follows.
    EditStackOrMemory,
    /// Grow the frame's memory and the memo of how far it has been paid for, together — so the
    /// interpreter stays consistent and the next expanding opcode is charged nothing.
    GrowMemoryFree,
    /// Edit a finished outcome's metadata: the range a call's return data lands in, or the address
    /// a creation reports. Neither is part of the `InterpreterResult` the same callback holds.
    EditOutcomeMetadata,
    /// Write to the journal directly, behind the EVM's back.
    JournalWrite,
}
}

impl Shape {
    /// Whether this shape answers the frame with a synthetic outcome instead of letting the EVM
    /// build it.
    const fn is_interception(self) -> bool {
        matches!(self, Self::Intercept | Self::RaiseInterceptionGas | Self::LowerInterceptionGas)
    }

    /// Whether this shape reaches through a `Gas`'s refund counter.
    const fn is_refund(self) -> bool {
        matches!(self, Self::RaiseRefund | Self::LowerRefund)
    }

    /// Whether this shape reaches through the EIP-8037 state-gas dimension.
    const fn is_state_gas(self) -> bool {
        matches!(self, Self::WriteReservoir | Self::WriteStateGas)
    }

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

    if shape.is_interception() && !input_facing {
        return Some(
            "only a callback that runs before the frame is built can answer it instead: the \
             `*_end` callbacks are handed a result the EVM already produced",
        );
    }

    if shape.is_refund() || shape.is_state_gas() {
        if !interpreter_facing && !input_facing && !result_facing {
            return Some("no `Gas` and no frame input is reachable from this callback");
        }
        if input_facing && shape != WriteReservoir {
            return Some(
                "a frame's inputs carry no refund counter and no state-gas spend counter; the \
                 pool is the one figure of either dimension they do carry",
            );
        }
        if shape == WriteReservoir && at == Create {
            return Some(
                "`CreateInputs` keeps its pool private and offers no setter, so the only rewrite \
                 that reaches it replaces the whole struct — which is the `EditInput` column",
            );
        }
        if shape == LowerRefund && at == InitializeInterp {
            return Some(
                "a frame is handed a fresh `Gas` whose refund counter is zero, so there is \
                 nothing to lower before its first instruction runs",
            );
        }
    }

    match shape {
        InjectGas | DrainGas | EditStackOrMemory | GrowMemoryFree if !interpreter_facing => {
            Some("no live interpreter is reachable from this callback")
        }
        RaiseEnvelope | LowerEnvelope | EditInput if !input_facing => Some(
            "this callback receives no frame input it can build a frame from: the `*_end` \
             callbacks take theirs by shared reference, after the frame has already run",
        ),
        RaiseResultGas | LowerResultGas | FailResult | ReviveResult | EditOutcomeMetadata
            if !result_facing =>
        {
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
    fn hit_interpreter<CTX: ContextTr, INTR: InterpreterTypes>(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &CTX,
    ) {
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
            Shape::RaiseRefund => {
                interp.gas.record_refund(REFUND);
                self.fired += 1;
            }
            Shape::LowerRefund => {
                interp.gas.record_refund(-REFUND);
                self.fired += 1;
            }
            Shape::WriteReservoir => {
                interp.gas.set_reservoir(RESERVOIR);
                self.fired += 1;
            }
            Shape::WriteStateGas => {
                interp.gas.set_state_gas_spent(STATE_GAS);
                self.fired += 1;
            }
            Shape::EditStackOrMemory => {
                // Bump the value an `SSTORE` is about to write, so the edit is visible in the
                // produced state rather than only in the absence of an accounting change. Two
                // pops and two pushes, so the stack's length is where it was: this is the
                // contents half of the column.
                let [key, value] =
                    interp.stack.popn::<2>().expect("an SSTORE has both its operands on the stack");
                assert!(interp.stack.push(value.wrapping_add(U256::from(1))));
                assert!(interp.stack.push(key));
                self.fired += 1;
            }
            Shape::GrowMemoryFree => {
                Self::grow_memory_free(interp, context);
                self.fired += 1;
            }
            _ => unreachable!("{:?} is not an interpreter-facing shape", self.shape),
        }
    }

    /// Whether a live-interpreter callback is one this cheat's shape can land at.
    ///
    /// Two shapes are choosy about the moment rather than about the callback. A refund the
    /// interpreter is to *lose* needs one it already holds, which only exists once a frame has
    /// cleared a storage slot; and any refund edit made while a terminating action is pending is
    /// written into a counter the action has already copied, so it would land on the action's
    /// number rather than on this column's mechanism.
    fn interpreter_moment_is_right<INTR: InterpreterTypes>(
        &self,
        interp: &mut Interpreter<INTR>,
    ) -> bool {
        if !self.shape.is_refund() {
            return true;
        }
        if matches!(interp.bytecode.action(), Some(InterpreterAction::Return(_))) {
            return false;
        }
        self.shape != Shape::LowerRefund || interp.gas.refunded() >= REFUND
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

    /// Grows the frame's memory by one word and moves the memo with it, so that the interpreter
    /// is left in a state it could have reached by paying, having paid nothing.
    ///
    /// The fixture's first `MSTORE` then finds its word already paid for, and the transaction
    /// spends exactly that expansion less than the uninspected run — which is what makes this a
    /// rewrite the guard has to see rather than a curiosity.
    fn grow_memory_free<CTX: ContextTr, INTR: InterpreterTypes>(
        interp: &mut Interpreter<INTR>,
        context: &CTX,
    ) {
        let words = interp.memory.size() / 32 + 1;
        assert!(interp.memory.resize(words * 32), "the fixture must allow a one-word growth");
        // Priced through revm's own table rather than a restatement of the formula: the memo has
        // to be exactly what the EVM would have written, or a later expansion prices its
        // increment from a baseline that never existed.
        let cost = context.cfg().gas_params().memory_cost(words);
        interp.gas.memory_mut().set_words_num(words, cost);
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
            Shape::WriteReservoir => {
                inputs.reservoir += RESERVOIR;
                self.fired += 1;
                None
            }
            Shape::Intercept | Shape::RaiseInterceptionGas | Shape::LowerInterceptionGas => {
                self.fired += 1;
                Some(CallOutcome::new(
                    InterpreterResult::new(
                        InstructionResult::Stop,
                        Bytes::new(),
                        Gas::new(self.interception_gas(inputs.gas_limit)),
                    ),
                    inputs.return_memory_offset.clone(),
                ))
            }
            _ => unreachable!("{:?} is not an input-facing shape", self.shape),
        }
    }

    /// The gas an interception's synthetic outcome hands back, given the envelope it was handed.
    ///
    /// The echo — hand back exactly what was forwarded — is the convention every tool that
    /// intercepts follows, and the reason the two neighbouring columns exist: with it, the
    /// accounting closes whether or not anything measures the figure.
    fn interception_gas(&self, envelope: u64) -> u64 {
        match self.shape {
            Shape::Intercept => envelope,
            Shape::RaiseInterceptionGas => envelope + INTERCEPTION,
            Shape::LowerInterceptionGas => envelope - INTERCEPTION,
            _ => unreachable!("{:?} is not an interception", self.shape),
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
            Shape::Intercept | Shape::RaiseInterceptionGas | Shape::LowerInterceptionGas => {
                self.fired += 1;
                Some(CreateOutcome::new(
                    InterpreterResult::new(
                        InstructionResult::Stop,
                        Bytes::new(),
                        Gas::new(self.interception_gas(inputs.gas_limit())),
                    ),
                    None,
                ))
            }
            _ => unreachable!("{:?} is not an input-facing shape", self.shape),
        }
    }

    /// Applies the one result-facing shape that reaches past the `InterpreterResult` — a finished
    /// outcome's own metadata.
    ///
    /// A call's return range is shrunk to nothing rather than moved, because moving it past the
    /// caller's allocated memory is a panic in revm and this fixture's caller holds one word. The
    /// visible-effect form, where the caller then reads a word the callee never wrote, is pinned
    /// in `ledger_blind_spots.rs`.
    fn hit_outcome_metadata(&mut self, result: &mut FrameResult) {
        match result {
            FrameResult::Call(outcome) => {
                assert!(
                    !outcome.memory_offset.is_empty(),
                    "the fixture's inner CALL must ask for a return range, or there is nothing                      to shrink",
                );
                outcome.memory_offset = outcome.memory_offset.start..outcome.memory_offset.start;
            }
            FrameResult::Create(outcome) => outcome.address = Some(RELABELLED_DEPLOYMENT),
        }
        self.fired += 1;
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
            Shape::RaiseRefund => {
                result.gas.record_refund(REFUND);
                self.fired += 1;
            }
            Shape::LowerRefund => {
                assert!(
                    result.gas.refunded() >= REFUND,
                    "the fixture must hand this cell a frame that refunded something, got {}",
                    result.gas.refunded(),
                );
                result.gas.record_refund(-REFUND);
                self.fired += 1;
            }
            Shape::WriteReservoir => {
                result.gas.set_reservoir(RESERVOIR);
                self.fired += 1;
            }
            Shape::WriteStateGas => {
                result.gas.set_state_gas_spent(STATE_GAS);
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
            _ if !self.interpreter_moment_is_right(interp) => {}
            _ => self.hit_interpreter(interp, context),
        }
    }

    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.steps += 1;
        if !self.arm(At::Step) {
            return;
        }
        // The refund columns fire on the first callback that offers the moment they need, rather
        // than on a fixed ordinal.
        if self.shape.is_refund() {
            if self.interpreter_moment_is_right(interp) {
                self.hit_interpreter(interp, context);
            }
            return;
        }
        match self.shape {
            Shape::JournalWrite if self.steps == self.step_at => self.hit_journal(context),
            // Fire on the first `SSTORE` the transaction reaches, whose operands are on the stack
            // and about to be consumed.
            Shape::EditStackOrMemory if interp.bytecode.opcode() == SSTORE => {
                self.hit_interpreter(interp, context)
            }
            Shape::EditStackOrMemory | Shape::JournalWrite => {}
            _ if self.steps == self.step_at => self.hit_interpreter(interp, context),
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
        if self.shape.is_refund() {
            if self.interpreter_moment_is_right(interp) {
                self.hit_interpreter(interp, context);
            }
            return;
        }
        if self.steps != self.step_at {
            return;
        }
        match self.shape {
            Shape::JournalWrite => self.hit_journal(context),
            Shape::EditStackOrMemory => self.hit_frame_state(interp),
            _ => self.hit_interpreter(interp, context),
        }
    }

    fn log_full(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX, _log: Log) {
        if !self.arm(At::LogFull) {
            return;
        }
        match self.shape {
            Shape::JournalWrite => self.hit_journal(context),
            Shape::EditStackOrMemory => self.hit_frame_state(interp),
            _ if !self.interpreter_moment_is_right(interp) => {}
            _ => self.hit_interpreter(interp, context),
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
        if self.shape == Shape::EditOutcomeMetadata {
            self.hit_outcome_metadata(result);
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
        if self.shape == Shape::EditOutcomeMetadata {
            assert!(!outcome.memory_offset.is_empty(), "the inner CALL must ask for a range");
            outcome.memory_offset = outcome.memory_offset.start..outcome.memory_offset.start;
            self.fired += 1;
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
        if self.shape == Shape::EditOutcomeMetadata {
            outcome.address = Some(RELABELLED_DEPLOYMENT);
            self.fired += 1;
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

/// Init code that writes [`INIT_SLOT`], leaves a refund behind, and returns two bytes of runtime
/// code.
fn init_code() -> Vec<u8> {
    clear_a_slot(BytecodeBuilder::default().sstore(U256::from(INIT_SLOT), U256::from(STORED)))
        .push_number(0x6000u64)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(2u64) // size
        .push_number(30u64) // offset: the last two bytes of the word just stored
        .append(RETURN)
        .build()
        .to_vec()
}

/// Sets a slot and clears it again, which leaves the frame holding a refund the EVM produced.
///
/// Every frame in the fixture does this, because the refund-lowering column needs a refund to take
/// from wherever it lands — an interpreter's counter, a finished call's result, or a finished
/// creation's.
fn clear_a_slot(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .sstore(U256::from(CLEARED_SLOT), U256::from(STORED))
        .sstore(U256::from(CLEARED_SLOT), U256::ZERO)
}

/// The transaction's entry contract: one `LOG1`, one inner `CALL`, one `CREATE`, a slot set and
/// cleared, a second `LOG1`, and one `SSTORE`.
///
/// One fixture rather than one per row, so that every callback fires in the same transaction and
/// a cell's assertions are about the cheat rather than about which fixture it got. The second
/// `LOG1` is there so the `log_full` row has a callback that runs *after* the frame has a refund;
/// the first one runs before anything has cleared a slot.
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
        // CALL(gas, CALLEE, value=0, argsOffset=0, argsSize=0, retOffset=0, retSize=32).
        //
        // The return range is asked for so that the outcome-metadata column has one to shrink; the
        // callee returns nothing, so no byte is ever copied and no other cell's numbers move — the
        // word of memory the range covers was already allocated by the `MSTORE` above.
        .push_number(32u64)
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
    let builder = builder
        .push_number(init.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP);
    clear_a_slot(builder)
        // LOG1(offset=0, size=32, topic=2), now that the frame carries a refund.
        .push_number(2u64)
        .push_number(32u64)
        .push_number(0u64)
        .append(LOG1)
        .sstore(U256::from(TOP_SLOT), U256::from(STORED))
        .append(STOP)
        .build()
}

fn callee_code(fixture: Fixture) -> Bytes {
    let builder = clear_a_slot(
        BytecodeBuilder::default().sstore(U256::from(CALLEE_SLOT), U256::from(STORED)),
    );
    match fixture {
        Fixture::ReturningCallee => builder.append(STOP).build(),
        Fixture::RevertingCallee => builder.revert().build(),
    }
}

fn db_for(fixture: Fixture) -> MemoryDatabase {
    db_with_callee(caller_code(), callee_code(fixture))
}

// --- running one cell -----------------------------------------------------------------------

/// A storage slot as the produced state has it, taking the slot as the small integer the fixtures
/// use.
fn slot(outcome: &Outcome, address: Address, slot: u64) -> U256 {
    outcome.storage_value(address, U256::from(slot))
}

/// Whether the fixture's `CREATE` left code behind.
fn deployed(outcome: &Outcome) -> bool {
    outcome.state.get(&deployed_address()).is_some_and(|account| !account.info.is_empty_code_hash())
}

/// Runs the fixture with no inspector at all.
fn transact_plain(fixture: Fixture) -> Outcome {
    transact(MegaSpecId::REX7, db_for(fixture), limits())
}

/// Runs the fixture with `cheat` attached, on the inspected loops or with them switched off.
fn transact_cheating(fixture: Fixture, cheat: &mut Cheat, inspected: bool) -> Outcome {
    let mut db = db_for(fixture);
    let mut evm = MegaEvm::new(context(&mut db, MegaSpecId::REX7, limits())).with_inspector(cheat);
    if !inspected {
        alloy_evm::Evm::set_inspector_enabled(&mut evm, false);
    }
    drive(MegaSpecId::REX7, &mut evm, call_contract_tx(TX_GAS_LIMIT))
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
    state: fn(&Outcome, &str),
}

/// A rewrite that moved gas *and* came back changed in something the shim compares.
fn plus_intervention(mut ledger: InspectorLedger) -> InspectorLedger {
    ledger.interventions += 1;
    ledger
}

/// The fixture ran to its end and every frame committed: the callee's write, the deployment, and
/// the top frame's own write are all there.
fn state_all_committed(r: &Outcome, label: &str) {
    assert!(
        r.result.is_success(),
        "{label}: expected a successful transaction, got {:?}",
        r.result
    );
    assert_eq!(slot(r, CALLEE, CALLEE_SLOT), U256::from(STORED), "{label}: the callee's write");
    assert!(deployed(r), "{label}: the fixture's CREATE must have deployed code");
    assert_eq!(slot(r, CONTRACT, TOP_SLOT), U256::from(STORED), "{label}: the top frame's write");
}

/// The callee's frame did not commit: whatever ended it, its write is gone.
fn state_callee_write_rolled_back(r: &Outcome, label: &str) {
    assert!(
        r.result.is_success(),
        "{label}: the caller absorbs the inner failure, got {:?}",
        r.result
    );
    assert_eq!(
        slot(r, CALLEE, CALLEE_SLOT),
        U256::ZERO,
        "{label}: a frame the caller was told failed must leave no write behind",
    );
    assert!(deployed(r), "{label}: the rest of the transaction must be unaffected");
}

/// The callee's frame committed a write the EVM had decided to roll back — the journal followed
/// the rewritten result rather than the classification.
fn state_callee_write_revived(r: &Outcome, label: &str) {
    assert!(r.result.is_success(), "{label}: {:?}", r.result);
    assert_eq!(
        slot(r, CALLEE, CALLEE_SLOT),
        U256::from(STORED),
        "{label}: a reverted frame rewritten into a success must have its state committed with it",
    );
}

/// The stack edit landed on the callee's `SSTORE` operand.
fn state_callee_write_bumped(r: &Outcome, label: &str) {
    assert!(r.result.is_success(), "{label}: {:?}", r.result);
    assert_eq!(
        slot(r, CALLEE, CALLEE_SLOT),
        U256::from(STORED + 1),
        "{label}: the value the inspector put on the stack is the value the EVM wrote",
    );
}

/// The creation did not happen.
fn state_no_deployment(r: &Outcome, label: &str) {
    assert!(r.result.is_success(), "{label}: the caller absorbs it, got {:?}", r.result);
    assert!(!deployed(r), "{label}: no code may be deployed");
    assert_eq!(
        slot(r, deployed_address(), INIT_SLOT),
        U256::ZERO,
        "{label}: nor its storage write"
    );
}

/// Every cell the matrix covers.
fn matrix() -> Vec<Cell> {
    use At::*;
    use Shape::*;

    let mut cells = Vec::new();
    /// One cell of the matrix, as a data row.
    ///
    /// The fixture defaults to the returning callee and the state check to "every frame
    /// committed", because that is what a cell wants unless the rewrite it makes is one that
    /// changes what the transaction leaves behind.
    macro_rules! cell {
        ($at:expr, $shape:expr, $ledger:expr $(, $fixture:expr, $state:expr)?) => {{
            #[allow(unused_mut, unused_assignments)]
            let mut fixture = Fixture::ReturningCallee;
            #[allow(unused_mut, unused_assignments)]
            let mut state: fn(&Outcome, &str) = state_all_committed;
            $(
                fixture = $fixture;
                state = $state;
            )?
            cells.push(Cell { at: $at, shape: $shape, fixture, ledger: $ledger, state });
        }};
    }

    // The four callbacks that are handed a live interpreter.
    for at in [InitializeInterp, Step, StepEnd, LogFull] {
        cell!(at, InjectGas, ledger_gas(i128::from(INJECT)));
        cell!(at, DrainGas, ledger_gas(-i128::from(DRAIN)));
        // The two halves of the working-state column. `step` fires on an `SSTORE`'s operands and
        // pops and pushes the same two words, so both sizes come back where they were and nothing
        // is booked — the contents rewrite that has no lane. The other three rows run where there
        // is no operand to swap, so the cheat leaves a word on the stack instead, and a stack that
        // came back one word longer is a constant-time reading the shim takes.
        cell!(
            at,
            EditStackOrMemory,
            if at == InitializeInterp { ledger_intervention() } else { InspectorLedger::default() },
            Fixture::ReturningCallee,
            if at == Step { state_callee_write_bumped } else { state_all_committed }
        );
        // Growing the memory and its memo together leaves every interpreter invariant intact and
        // still skips the next expansion's charge, so no gas lane sees it and the intervention
        // counter must.
        cell!(at, GrowMemoryFree, ledger_intervention());
        cell!(at, JournalWrite, InspectorLedger::default());
        // The receipt's other two numbers, reached through the same `Gas` as the counter above.
        cell!(at, RaiseRefund, ledger_refund(i128::from(REFUND)));
        if at != InitializeInterp {
            cell!(at, LowerRefund, ledger_refund(-i128::from(REFUND)));
        }
        cell!(at, WriteReservoir, ledger_reservoir(i128::from(RESERVOIR)));
        cell!(at, WriteStateGas, ledger_state_gas(i128::from(STATE_GAS)));
    }

    // The pending action, which only `step_end` ever sees: revm's inspected loop breaks out the
    // moment one is set, so it is the one callback that runs with an instruction's action already
    // in place. Which lane the edit lands on is decided by the action's own variant — a `Return`
    // action is what the frame hands back, a `NewFrame` action is what the child is built with.
    cell!(StepEnd, RaiseActionResultGas, ledger_result(i128::from(ACTION)));
    cell!(StepEnd, LowerActionResultGas, ledger_result(-i128::from(ACTION)));
    cell!(StepEnd, RaiseActionEnvelope, ledger_env(i128::from(ACTION)));
    cell!(StepEnd, LowerActionEnvelope, ledger_env(-i128::from(ACTION)));

    // The three callbacks that are handed a frame's inputs before the frame is built. `create`
    // reaches the fixture's `CREATE`; the other two reach its inner `CALL`.
    for at in [FrameStart, Call, Create] {
        // A rewrite of what the frame will *do* is the one that changes what it leaves behind, and
        // the creation side of the sweep leaves no deployment where the call side leaves no write.
        let undone: fn(&Outcome, &str) =
            if at == Create { state_no_deployment } else { state_callee_write_rolled_back };
        cell!(at, RaiseEnvelope, ledger_env(i128::from(ENVELOPE)));
        cell!(at, LowerEnvelope, ledger_env(-i128::from(ENVELOPE)));
        cell!(at, EditInput, ledger_intervention(), Fixture::ReturningCallee, undone);
        cell!(at, Intercept, ledger_intervention(), Fixture::ReturningCallee, undone);
        // The same interception, sized against the envelope rather than echoing it. No frame is
        // built, so the whole of what the outcome hands back is the inspector's number, and the
        // difference from what the caller forwarded is what the ledger has to carry.
        for (shape, sign) in [(RaiseInterceptionGas, 1i128), (LowerInterceptionGas, -1)] {
            cell!(
                at,
                shape,
                plus_intervention(ledger_result(sign * i128::from(INTERCEPTION))),
                Fixture::ReturningCallee,
                undone
            );
        }
        cell!(at, JournalWrite, InspectorLedger::default());
        if at != Create {
            // The pool a call's inputs seed the child with. It travels to the child and back, so
            // it is booked as gas — and the inputs came back changed in a field the envelope lane
            // does not cover, which the rewrite comparison books separately.
            cell!(at, WriteReservoir, plus_intervention(ledger_reservoir(i128::from(RESERVOIR))));
        }
    }

    // The three callbacks that are handed a finished frame's result.
    for at in [FrameEnd, CallEnd, CreateEnd] {
        let undone: fn(&Outcome, &str) =
            if at == CreateEnd { state_no_deployment } else { state_callee_write_rolled_back };
        cell!(at, RaiseResultGas, ledger_result(i128::from(RESULT)));
        cell!(at, LowerResultGas, ledger_result(-i128::from(RESULT)));
        cell!(at, FailResult, ledger_intervention(), Fixture::ReturningCallee, undone);
        if at != CreateEnd {
            // Reviving a reverted *call* is honoured; the creation form is refused, and the two
            // tests that pin the refusal are named in `inapplicable`.
            cell!(
                at,
                ReviveResult,
                ledger_intervention(),
                Fixture::RevertingCallee,
                state_callee_write_revived
            );
        }
        // The half of a finished outcome that sits outside the `InterpreterResult`: where a call's
        // return data lands, and which address a creation reports. Neither moves gas, and this
        // fixture discards both — the caller asks for a range the callee never fills and pops the
        // address — so what the cell pins is the booking. `ledger_blind_spots.rs` pins the forms
        // that change the produced state.
        cell!(at, EditOutcomeMetadata, ledger_intervention());
        cell!(at, JournalWrite, InspectorLedger::default());
        cell!(at, RaiseRefund, ledger_refund(i128::from(REFUND)));
        cell!(at, LowerRefund, ledger_refund(-i128::from(REFUND)));
        cell!(at, WriteReservoir, ledger_reservoir(i128::from(RESERVOIR)));
        cell!(at, WriteStateGas, ledger_state_gas(i128::from(STATE_GAS)));
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
            reading.inspector_ledger, cell.ledger,
            "{label}: the shim must book exactly what the cheat did, and nothing else",
        );
        // The interpreter lane the cheat measured for itself and the lane the shim booked are two
        // independent readings of the same edit.
        if cheat.moved_gas != 0 {
            assert_eq!(
                reading.inspector_ledger.gas.net(),
                cheat.moved_gas,
                "{label}: the shim's reading of the counter edit must match the cheat's own",
            );
        }
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

    for &at in At::ALL {
        for &shape in Shape::ALL {
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
        (At::Call, Shape::LowerInterceptionGas, Fixture::ReturningCallee),
        (At::Create, Shape::EditInput, Fixture::ReturningCallee),
        (At::CallEnd, Shape::LowerResultGas, Fixture::ReturningCallee),
        (At::CreateEnd, Shape::FailResult, Fixture::ReturningCallee),
        (At::FrameEnd, Shape::ReviveResult, Fixture::RevertingCallee),
        (At::Step, Shape::JournalWrite, Fixture::ReturningCallee),
        (At::StepEnd, Shape::RaiseActionResultGas, Fixture::ReturningCallee),
        (At::Step, Shape::LowerRefund, Fixture::ReturningCallee),
        (At::CallEnd, Shape::WriteReservoir, Fixture::ReturningCallee),
        (At::FrameEnd, Shape::WriteStateGas, Fixture::ReturningCallee),
        (At::Step, Shape::GrowMemoryFree, Fixture::ReturningCallee),
        (At::CreateEnd, Shape::EditOutcomeMetadata, Fixture::ReturningCallee),
    ];

    let mut plain: BTreeMap<Fixture, Outcome> = BTreeMap::new();
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
            off.inspector_ledger.is_zero(),
            "{label}: an inert inspector books nothing: {:?}",
            off.inspector_ledger
        );
        assert_eq!(
            std::format!("{:?}", off.result),
            std::format!("{:?}", reference.result),
            "{label}"
        );
        assert_eq!(off.compute_gas, reference.compute_gas, "{label}");
        assert_eq!(off.enforced(), reference.enforced(), "{label}");
        assert_eq!(off.destroyed, reference.destroyed, "{label}");
        assert_eq!(off.data_size, reference.data_size, "{label}");
        assert_eq!(off.kv_updates, reference.kv_updates, "{label}");
        assert_eq!(off.state_growth, reference.state_growth, "{label}");
        assert_eq!(off.gas_used, reference.gas_used, "{label}");
        assert_eq!(off.total_gas_spent, reference.total_gas_spent, "{label}");
        assert_eq!(off.terms, reference.terms, "{label}");
        assert_eq!(
            state_view(&off.state),
            state_view(&reference.state),
            "{label}: the produced state"
        );
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

    let reverting = transact_plain(Fixture::RevertingCallee);
    assert!(reverting.result.is_success(), "the caller absorbs the revert: {:?}", reverting.result);
    assert_eq!(
        slot(&reverting, CALLEE, CALLEE_SLOT),
        U256::ZERO,
        "the reverting callee's write must be rolled back without an inspector",
    );
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

    let db = base_db(deploy_then_stop(&REVERTING_INIT_CODE));

    assert_refused(REVIVED_CREATION, || {
        try_transact_inspected(db.clone(), limits(), &mut LateReviver)
    });
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
    let code = call_then_stop(CALLEE, INNER_CALL_GAS);
    let build_db = || {
        db_with_callee(code.clone(), callee.clone()).account_balance(CALLEE, U256::from(ONE_ETH))
    };

    let mut watcher = Watcher::default();
    let (plain, watched) = plain_and_cheated(build_db, &mut watcher);

    assert_eq!(watcher.seen.len(), 1, "the shim must forward the callback: {:?}", watcher.seen);
    assert_eq!(watcher.seen[0].0, CALLEE, "the self-destructing contract");
    assert_eq!(watcher.seen[0].1, CALLER, "the beneficiary");
    assert!(
        watched.inspector_ledger.is_zero(),
        "nothing to measure: {:?}",
        watched.inspector_ledger
    );
    assert_eq!(watched.compute_gas, plain.compute_gas);
    assert_eq!(watched.total_gas_spent, plain.total_gas_spent);
    assert_eq!(state_view(&watched.state), state_view(&plain.state));
}
