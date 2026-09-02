//! The measurement shim every inspector handed to `MegaETH` is wrapped in.
//!
//! # Why a shim
//!
//! An inspector is not a passive observer. Every callback that receives a live interpreter can
//! write to its gas counter and to the action it is holding, and every callback that receives a
//! frame's inputs can change the gas limit the frame is about to be built with. `MegaETH` meters
//! compute gas by watching those exact counters, and derives what a transaction destroyed from the
//! envelope it spent, so an unmeasured edit shows up as the EVM having done less work than it did,
//! or as a transaction having spent less gas than it did.
//!
//! # Why the callback boundary is enough
//!
//! The EVM does not execute inside an inspector callback. Anything that changes between the moment
//! the shim delegates to the user's inspector and the moment control comes back is therefore the
//! inspector's doing — not by attribution, but by construction. The shim snapshots the counters it
//! cares about on the way in, compares on the way out, and books the difference.
//!
//! That is why the shim lives at the `Inspector` implementation layer and not inside revm's
//! dispatch loop: wrapping the object is sufficient to sit on every boundary, and mirroring
//! `inspect_instructions` would take on a core dispatch loop for no additional reach.
//!
//! # What the shim does with what it measures
//!
//! - Interpreter-counter edits go to [`AdditionalLimit::record_inspector_gas_adjustment`], which
//!   books them, keeps them out of the compute-gas measurement, and re-derives the gas clamp so
//!   injected gas is not spendable past the compute headroom.
//! - Pending-action edits go to [`book_pending_action`], which routes them by the action the
//!   callback left behind — a frame's counter, a child's envelope, or the result its caller will
//!   reclaim from. The counter and the action between them hold everything a frame has, and
//!   [`held`] is the identity the two lanes split.
//! - Frame-envelope edits go to [`AdditionalLimit::record_inspector_env_adjustment`].
//! - Refund edits go to [`book_refund`], on their own lane: a refund moves what the sender pays
//!   without moving the envelope the conservation law is stated over, so it needs a lane of its own
//!   and no term in the law. [`held_refund`] is the reading it is taken against.
//! - The EIP-8037 state-gas dimension — a `Gas`'s `reservoir` and `state_gas_spent`, and a frame
//!   input's `reservoir` — is *not* measured here. `MegaETH` runs with EIP-8037 off, so it produces
//!   none of it and there is no difference to take; the transaction's own settlement point books
//!   whatever of it survives. See
//!   [`InspectorLedger::reservoir`](crate::InspectorLedger::reservoir).
//! - A callback that answers a frame itself stages the envelope it was handed, through
//!   [`AdditionalLimit::stage_inspector_interception_envelope`], so that the frame init it
//!   short-circuited can settle the gas its synthetic outcome carries against what the transaction
//!   funded.
//! - Rewrites that move no gas go to [`book_intervention`]: a frame result's classification or
//!   output, a frame's inputs outside their gas limit, a finished outcome's metadata
//!   ([`OutcomeMetadata`]) — and the constant-time readings the shim can take off a live
//!   interpreter ([`WorkingSet`]), which is what makes a frame's memory grown for free visible.
//! - One rewrite shape is refused outright: see [`MeasuredInspector::create_end`].
//!
//! Nothing here changes what the inspector is allowed to do to the EVM, and nothing here runs on
//! the uninspected path — revm's plain interpreter loop never calls an inspector at all.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{string::String, vec::Vec};

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, Log, U256};
use core::ops::Range;
use revm::{
    context::{ContextError, ContextTr},
    handler::FrameResult,
    interpreter::{
        interpreter_types::{LoopControl, MemoryTr, StackTr},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        Interpreter, InterpreterAction, InterpreterResult, InterpreterTypes,
    },
    Inspector,
};

use crate::{ExternalEnvTypes, MegaContext, MegaSpecId};

/// The message a refused `create_end` rewrite surfaces as `EVMError::Custom`.
pub(crate) const FORBIDDEN_CREATE_REVIVAL: &str =
    "inspector rewrote a failed contract creation into a successful one";

/// Wraps a user inspector so that what it does to gas accounting is measured and booked.
///
/// `MegaETH` applies this itself — [`MegaEvm::with_inspector`](crate::MegaEvm::with_inspector) and
/// [`InspectEvm::set_inspector`](revm::InspectEvm::set_inspector) take the user's inspector by
/// value and store it wrapped, and the accessors hand back the unwrapped inspector — so the wrapper
/// is not something a caller opts into or can opt out of.
///
/// Derefs to the wrapped inspector, so `evm.inspector().whatever()` reaches the user's own type.
#[derive(Clone, Copy, Debug, Default, derive_more::Deref, derive_more::DerefMut)]
pub struct MeasuredInspector<I> {
    #[deref]
    #[deref_mut]
    inner: I,
}

impl<I> MeasuredInspector<I> {
    /// Wraps `inner` in the measurement shim.
    pub const fn new(inner: I) -> Self {
        Self { inner }
    }

    /// The wrapped inspector.
    pub const fn inner(&self) -> &I {
        &self.inner
    }

    /// The wrapped inspector, mutably.
    pub const fn inner_mut(&mut self) -> &mut I {
        &mut self.inner
    }

    /// Unwraps the shim, returning the inspector it was measuring.
    pub fn into_inner(self) -> I {
        self.inner
    }
}

/// Where the gas an interpreter is holding will go next, read off the action it is holding.
///
/// This is the one thing that decides how gas measured at a live-interpreter callback is booked,
/// for both of the objects such a callback can write gas into — the interpreter's own counter and
/// the pending action. The three variants are the three places a frame's budget can be sitting
/// when a callback runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionLane {
    /// No action pending: the frame carries straight on, so what it holds is its counter.
    Counter,
    /// A `NewFrame` action: the frame suspends, and the action carries the envelope a child is
    /// about to be built with. The frame itself resumes on its counter afterwards.
    Envelope,
    /// A `Return` action: the frame is over, and the action carries what its caller reclaims.
    Result,
}

impl ActionLane {
    /// The lane an interpreter holding `action` is on.
    #[inline]
    const fn of(action: Option<&InterpreterAction>) -> Self {
        match action {
            None => Self::Counter,
            Some(InterpreterAction::NewFrame(_)) => Self::Envelope,
            Some(InterpreterAction::Return(_)) => Self::Result,
        }
    }

    /// Whether an edit to this interpreter's gas counter can still reach the transaction's
    /// envelope.
    ///
    /// It cannot exactly on [`Result`](Self::Result). revm's inspected loop runs the terminating
    /// instruction first and the callback after it, and that instruction has already copied the
    /// counter into the action it set — the action is what becomes the frame's result and what the
    /// caller reclaims from. Whatever the callback writes into the counter afterwards is written
    /// into an object nobody will read again.
    ///
    /// The two neighbouring shapes are live and must stay booked. With no action pending the frame
    /// carries straight on; with a `NewFrame` action pending it suspends into a child and then
    /// resumes on this very counter. "The loop is about to break" is therefore not the question —
    /// revm breaks out of the instruction loop in both the suspending and the terminating case —
    /// and a rule phrased that way would stop booking edits that really do move a frame's budget.
    ///
    /// Read *after* the callback returns, because the question is about the counter the callback
    /// left behind: an inspector that sets or clears an action has changed what the EVM does next,
    /// and the answer has to follow it.
    ///
    /// # Why this decides booking and not measurement
    ///
    /// Only the ledger is gated on it. `MegaETH`'s own tail settlement measures a frame's work as
    /// a drop in this same counter and does read it after the action is set, so the settlement
    /// baseline has to shift for a dead-window edit exactly as it does for a live one — otherwise
    /// gas the inspector wrote in would read as work the frame performed, which is the opposite
    /// error.
    ///
    /// # The gas the counter no longer speaks for
    ///
    /// What a dead-window counter edit cannot reach, an edit to the action itself can — and
    /// [`measure_pending_action`] measures exactly that, against the same counter reading, so the
    /// two together account for every unit of gas the frame holds. See [`held`] for the identity
    /// they split.
    #[inline]
    const fn counter_reaches_envelope(self) -> bool {
        !matches!(self, Self::Result)
    }
}

/// The gas a frame and its pending continuation hold, given the action it is carrying and its own
/// counter.
///
/// This is the quantity the two live-interpreter lanes partition between them:
///
/// ```text
/// held(None,           counter) = counter
/// held(NewFrame(f),    counter) = counter + f.gas_limit
/// held(Return(r),      counter) = r.gas.remaining()
/// ```
///
/// A frame with no action pending will spend its counter. A suspending frame will spend its
/// counter when it resumes and has additionally handed the child's envelope on. A terminating
/// frame will spend nothing more — the action's own copy is what its caller reclaims, and the
/// counter is dead.
///
/// Both readings [`measure_pending_action`] takes use the counter *the EVM left behind*, so the
/// counter cancels out of the difference wherever it appears on both sides. What is left is
/// exactly the part of the movement that is not already on the counter lane, whatever the callback
/// did to the action's shape.
#[inline]
fn held(action: Option<&InterpreterAction>, counter: u64) -> i128 {
    match action {
        None => i128::from(counter),
        Some(InterpreterAction::NewFrame(frame_input)) => {
            i128::from(counter) + frame_input_gas_limit(frame_input).map_or(0, i128::from)
        }
        Some(InterpreterAction::Return(result)) => i128::from(result.gas.remaining()),
    }
}

/// The refund a frame and its pending continuation hold, given the action it is carrying and its
/// own gas object.
///
/// [`held`]'s counterpart on the refund dimension, and it has the same three cases for the same
/// reason — the object the EVM will read next is the one the frame is holding:
///
/// ```text
/// held_refund(None,        gas) = gas.refunded()
/// held_refund(NewFrame(_), gas) = gas.refunded()
/// held_refund(Return(r),   gas) = r.gas.refunded()
/// ```
///
/// The middle case differs from [`held`]'s: a `NewFrame` action carries a child's *envelope* but
/// no refund of its own, and the suspending frame resumes on this very counter with the child's
/// refund added to it. So the counter is the live object in two of the three cases, and only a
/// terminating action displaces it.
///
/// Read on both sides of a callback, the difference is the refund the inspector wrote — wherever
/// it wrote it, and whichever of the two objects the EVM goes on to read.
#[inline]
fn held_refund(action: Option<&InterpreterAction>, gas: &Gas) -> i64 {
    match action {
        None | Some(InterpreterAction::NewFrame(_)) => gas.refunded(),
        Some(InterpreterAction::Return(result)) => result.gas.refunded(),
    }
}

/// Books what a callback did to a refund counter.
///
/// Nominal: the figure booked is what the inspector wrote, not what survives the EIP-3529 cap or
/// the chain of frame returns between here and the receipt — see
/// [`InspectorLedger::refund`](crate::InspectorLedger::refund) for why neither of those is a
/// quantity a boundary can measure, and why over-stating is the safe direction for the one
/// consumer this lane has.
#[inline]
fn book_refund<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &MegaContext<DB, ExtEnvs>,
    before: i64,
    after: i64,
) {
    if before != after {
        context
            .additional_limit
            .borrow_mut()
            .record_inspector_refund_adjustment(i128::from(after) - i128::from(before));
    }
}

/// The refund a synthetic outcome carries, for a callback that answered a frame itself.
///
/// There is no "before" to difference against: no frame is built, so the EVM produced no refund
/// here at all and the whole of what the outcome carries is the inspector's — the same argument
/// the interception's gas baseline rests on, with the baseline being zero rather than the
/// envelope because a frame that never ran has refunded nothing.
#[inline]
fn book_synthetic_refund<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &MegaContext<DB, ExtEnvs>,
    refunded: i64,
) {
    book_refund(context, 0, refunded);
}

/// What a live-interpreter callback did to the interpreter's pending action.
#[derive(Clone, Copy, Debug)]
struct ActionChange {
    /// Gas the callback moved through the action, over and above anything it did to the counter.
    gas: i128,
    /// Whether the action came back describing something other than what the EVM decided to do.
    rewritten: bool,
    /// Where that gas is now sitting, which is what decides how it is booked.
    lane: ActionLane,
}

/// Measures what a callback did to the pending action, against the counter the EVM left behind.
///
/// The EVM does not execute inside a callback, so the action is either the one the last
/// instruction set or one the callback wrote — and the difference between the two readings of
/// [`held`] is what the callback moved. Taking both readings at the *pre-callback* counter is
/// what keeps this lane and the counter lane from overlapping:
/// [`AdditionalLimit::record_inspector_gas_adjustment`] books the counter's own movement exactly
/// when [`ActionLane::counter_reaches_envelope`] says the EVM will read it again, and it cancels
/// out of this difference in precisely the cases where it does.
#[inline]
fn measure_pending_action(
    before: Option<InterpreterAction>,
    after: Option<&InterpreterAction>,
    counter: u64,
) -> ActionChange {
    let gas = held(after, counter) - held(before.as_ref(), counter);
    ActionChange { gas, rewritten: action_rewritten(before, after), lane: ActionLane::of(after) }
}

/// Whether a callback left behind an action describing something other than what the EVM decided.
///
/// Gas is excluded, exactly as it is at every other boundary: it travels on the lanes
/// [`measure_pending_action`] routes it to, and counting it here as well would report one rewrite
/// twice. A callback that installed, removed or swapped an action has rewritten what the EVM does
/// next as thoroughly as it is possible to, so every shape change counts.
#[inline]
fn action_rewritten(before: Option<InterpreterAction>, after: Option<&InterpreterAction>) -> bool {
    match (before, after) {
        (None, None) => false,
        (Some(InterpreterAction::Return(before)), Some(InterpreterAction::Return(after))) => {
            result_rewritten((before.result, &before.output), after)
        }
        (Some(InterpreterAction::NewFrame(before)), Some(InterpreterAction::NewFrame(after))) => {
            frame_input_rewritten(before, after)
        }
        _ => true,
    }
}

/// Books what a callback did to the interpreter's pending action.
///
/// The gas goes to the lane the action the callback *left behind* names, because that is where the
/// number now lives and therefore what decides when it can still be settled:
///
/// - [`ActionLane::Result`] is staged for the frame's settlement point, like an edit made at the
///   frame's last callback — whether it moves anything depends on the classification the caller
///   ends up seeing, which no callback here knows;
/// - [`ActionLane::Envelope`] is staged for the frame-start callback of the child the action is
///   about to build, which is where an envelope edit is booked from;
/// - [`ActionLane::Counter`] is booked on the spot, on the interpreter lane: with no action left,
///   the frame carries on spending what it holds, which is exactly what a counter edit does. This
///   is the algebra's third case rather than a shape an inspector can reach through the API —
///   `reset_action` only clears revm's `continue_execution` flag and leaves the action in place, so
///   emptying the slot means writing `None` into it and desynchronising the two.
#[inline]
fn book_pending_action<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &MegaContext<DB, ExtEnvs>,
    change: ActionChange,
) {
    if change.gas != 0 {
        let mut limit = context.additional_limit.borrow_mut();
        match change.lane {
            ActionLane::Result => limit.stage_inspector_action_result_adjustment(change.gas),
            ActionLane::Envelope => limit.stage_inspector_action_env_adjustment(change.gas),
            ActionLane::Counter => limit.record_inspector_action_counter_adjustment(change.gas),
        }
    }
    book_intervention(context, change.rewritten);
}

/// The gas limit a frame input carries, for the two variants that have one.
#[inline]
fn frame_input_gas_limit(frame_input: &FrameInput) -> Option<u64> {
    match frame_input {
        FrameInput::Call(inputs) => Some(inputs.gas_limit),
        FrameInput::Create(inputs) => Some(inputs.gas_limit()),
        FrameInput::Empty => None,
    }
}

/// Books what a callback did to a frame's envelope, together with whatever an earlier callback
/// staged into the same envelope through the pending `NewFrame` action — and, when the callback
/// answered the frame itself, stages that envelope for the frame's settlement point.
///
/// `intercepted` is true when the callback returned a synthetic outcome: the frame is skipped
/// entirely and the EVM never reads the inputs it edited, so the edit by itself moves nothing on
/// this lane — see [`InspectorLedger::env`](crate::InspectorLedger::env).
///
/// The staged amount is booked either way, and the asymmetry is not an oversight. An interception
/// discards inputs *this* callback edited a moment earlier, which is why that edit reaches
/// nothing. The staged amount was written by a different callback into the action the caller's
/// `CALL` / `CREATE` opcode had already produced — the caller's debit is behind it, `MegaETH`'s own
/// CALL settlement excluded the pre-edit amount from the caller's work, and a callback deciding
/// later to answer the frame itself cannot un-make that. It is simply the earliest of the two
/// edits to the one envelope, and the last thing to touch that envelope is what its holder is
/// sized from.
///
/// # Why an interception stages a baseline rather than booking a difference
///
/// Every other lane measures a difference across the callback, because the EVM produced the
/// object on both sides of it. An interception has no such object: the frame is never built, and
/// the result the caller reclaims from is one the inspector wrote from nothing. What the
/// transaction funded is the envelope on the way in; what it gets back is whatever gas that
/// result turns out to carry once the last callback has run. The difference between the two is
/// the measurement, and only the frame init that asked can take it — so the way in is staged
/// here, and [`AdditionalLimit::stage_inspector_interception_envelope`] says what the number is.
#[inline]
fn book_env_adjustment<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &MegaContext<DB, ExtEnvs>,
    before: Option<u64>,
    after: Option<u64>,
    intercepted: bool,
) {
    let staged = context.additional_limit.borrow_mut().take_inspector_action_env_adjustment();
    let callback = match (intercepted, before, after) {
        (false, Some(before), Some(after)) => i128::from(after) - i128::from(before),
        _ => 0,
    };
    if let (true, Some(before)) = (intercepted, before) {
        context.additional_limit.borrow_mut().stage_inspector_interception_envelope(before);
    }
    if staged + callback == 0 {
        return;
    }
    context.additional_limit.borrow_mut().record_inspector_env_adjustment(staged + callback);
}

/// Books one rewrite that changes what the execution *did* rather than what it cost — see
/// [`InspectorLedger::interventions`](crate::InspectorLedger::interventions).
///
/// Every caller answers the same question about the argument it was handed: did it come back
/// describing something other than what the EVM was about to do? Two things are deliberately not
/// part of that question:
///
/// - **Gas.** A frame input's gas limit and a frame result's remaining gas are booked as gas, on
///   the ledger's own lanes; counting them here as well would report one rewrite twice.
/// - **Anything neither the argument nor a constant-time reading off it describes.** The contents
///   of the interpreter's stack and memory, and the journal. Telling whether those came back
///   changed needs a snapshot of unbounded state, which no callback boundary can take at a cost the
///   inspected path can carry. Their *sizes* are a constant-time reading and are covered, by
///   [`WorkingSet`]; so is a finished outcome's metadata, by [`OutcomeMetadata`].
#[inline]
fn book_intervention<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &MegaContext<DB, ExtEnvs>,
    changed: bool,
) {
    if changed {
        context.additional_limit.borrow_mut().record_inspector_intervention();
    }
}

/// The part of a live interpreter's state a callback boundary can read in constant time.
///
/// The interpreter's stack and memory *contents* are outside the shim's reach — telling whether
/// either came back changed needs a snapshot of unbounded state — but their sizes are not, and
/// neither is the memo of how far the memory has been paid for. Those four readings are `O(1)`,
/// so the shim takes them on the way in and on the way out and books a difference as an
/// intervention.
///
/// The pair that made them necessary is the memory and its memo, moved together.
///
/// The memo (`Gas::memory`) is what the next expanding opcode compares its requirement against. An
/// inspector that raises it without growing the memory desynchronises the two and the EVM reads out
/// of bounds; one that grows the memory without raising it is charged for the growth twice over.
/// Moving *both*, together, is neither — the interpreter is in a state it could have reached by
/// paying, having paid nothing, and every later expansion inside the new bound is free. That pair
/// moves no gas anywhere at the moment it is made, so no gas lane can see it; what it changes is
/// what the EVM charges afterwards.
///
/// The stack's length is here for the same reason and at the same cost, and it moves whenever a
/// callback pushes or pops.
///
/// A stack or memory edit that leaves both sizes where they were stays invisible: it is a rewrite
/// of contents, which is the row of the shape table that has no lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkingSet {
    /// How many words the frame has on its stack.
    stack_len: usize,
    /// How many bytes of memory the frame has.
    memory_size: usize,
    /// How many words of that memory the frame has been charged for.
    memory_words: usize,
    /// What that charge came to.
    memory_expansion_cost: u64,
}

impl WorkingSet {
    /// Reads the four numbers off a live interpreter.
    #[inline]
    fn of<INTR: InterpreterTypes>(interp: &Interpreter<INTR>) -> Self {
        let memory = interp.gas.memory();
        Self {
            stack_len: interp.stack.len(),
            memory_size: interp.memory.size(),
            memory_words: memory.words_num,
            memory_expansion_cost: memory.expansion_cost,
        }
    }
}

/// Everything a `CallOutcome` carries besides the `InterpreterResult` inside it.
///
/// The result is compared on its own, by [`result_rewritten`]; this is the rest of the object, and
/// it is not bookkeeping. `memory_offset` is where the caller copies the callee's output to, so
/// moving it feeds the caller a word the callee never wrote. `charged_new_account_state_gas` tells
/// the caller whether to refund an EIP-8037 upfront charge. `was_precompile_called` and
/// `precompile_call_logs` decide which logs an inspector is shown next.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CallMetadata {
    memory_offset: Range<usize>,
    was_precompile_called: bool,
    precompile_call_logs: Vec<Log>,
    charged_new_account_state_gas: bool,
}

impl CallMetadata {
    #[inline]
    fn of(outcome: &CallOutcome) -> Self {
        Self {
            memory_offset: outcome.memory_offset.clone(),
            was_precompile_called: outcome.was_precompile_called,
            precompile_call_logs: outcome.precompile_call_logs.clone(),
            charged_new_account_state_gas: outcome.charged_new_account_state_gas,
        }
    }
}

/// [`CallMetadata`] for the generic callback, which is handed the variant rather than the outcome.
///
/// A creation's own metadata is one field: the address the caller's stack is about to receive.
/// Rewriting it reports a contract at an address holding no code, while the code the EVM deployed
/// stays where it was — a split the result's classification cannot express, and one no gas lane
/// sees.
///
/// Matched without a catch-all, so a `FrameResult` variant added upstream stops the build here.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OutcomeMetadata {
    Call(CallMetadata),
    Create(Option<Address>),
}

impl OutcomeMetadata {
    #[inline]
    fn of(result: &FrameResult) -> Self {
        match result {
            FrameResult::Call(outcome) => Self::Call(CallMetadata::of(outcome)),
            FrameResult::Create(outcome) => Self::Create(outcome.address),
        }
    }
}

/// Whether two output buffers are the same buffer.
///
/// Compared by address and length rather than by content. `Bytes` is immutable, so a callback can
/// only change an output by putting a different buffer there, and the caller holds its snapshot
/// across the comparison — which keeps the original alive, so its address cannot be reused
/// underneath. A replacement that copies the same bytes reads as unchanged, which is what it is.
#[inline]
fn same_buffer(before: &Bytes, after: &Bytes) -> bool {
    before.as_ptr() == after.as_ptr() && before.len() == after.len()
}

/// Whether a callback rewrote what a finished frame *did*: the classification its caller will see,
/// or the output it will read.
///
/// The classification is the one that carries the most: it decides whether the caller sees a
/// success, and — through the frame's settlement point — whether the frame's state is committed
/// and whether its remainder is handed back or destroyed. None of that moves gas by itself, so
/// none of it leaves a trace in any gas lane.
#[inline]
fn result_rewritten(before: (InstructionResult, &Bytes), after: &InterpreterResult) -> bool {
    before.0 != after.result || !same_buffer(before.1, &after.output)
}

/// Whether a callback edited a call frame's inputs anywhere but in their gas limit.
///
/// Everything else a call input carries — who is called, with what value, under which scheme, with
/// what calldata, in a static context or not — describes what the frame will do.
#[inline]
fn call_inputs_rewritten(mut before: CallInputs, after: &CallInputs) -> bool {
    before.gas_limit = after.gas_limit;
    before != *after
}

/// Whether a callback edited a creation's inputs anywhere but in their gas limit.
#[inline]
fn create_inputs_rewritten(mut before: CreateInputs, after: &CreateInputs) -> bool {
    before.set_gas_limit(after.gas_limit());
    before != *after
}

/// [`call_inputs_rewritten`] / [`create_inputs_rewritten`] for the generic callback, which is
/// handed the variant rather than the inputs. A callback that swapped the variant itself has
/// rewritten the frame as thoroughly as it is possible to.
#[inline]
fn frame_input_rewritten(before: FrameInput, after: &FrameInput) -> bool {
    match (before, after) {
        (FrameInput::Call(before), FrameInput::Call(after)) => {
            call_inputs_rewritten(*before, after)
        }
        (FrameInput::Create(before), FrameInput::Create(after)) => {
            create_inputs_rewritten(*before, after)
        }
        (FrameInput::Empty, FrameInput::Empty) => false,
        _ => true,
    }
}

/// Refuses a rewrite that turns a non-successful contract creation into a successful one, and says
/// so loudly.
///
/// The rewrite is forbidden rather than supported because there is no state behind it. By the time
/// `create_end` runs, revm has already reverted the frame's journal checkpoint and has already
/// declined to deposit the code — the size limit, the `0xEF` prefix rule and the code-deposit
/// charge are all evaluated before the callback. A result rewritten to success therefore reports a
/// deployment that did not happen, at an address holding no code, with the constructor's state
/// changes rolled back. Honouring it would hand the caller a contract that does not exist.
///
/// Detection only; nothing here compensates the journal. The original classification is restored,
/// the ledger counts the refusal, and the context's error slot carries the reason so that the
/// transaction fails with an error rather than with a fabricated receipt. Debug builds assert:
/// this is a detector, and a test corpus that produces this shape should stop rather than quietly
/// take the rejection path.
///
/// Gated to REX7+. On a frozen spec, an inspector's rewrite reaches no accounting lane that can be
/// made unsound by it, and the specs' behaviour — including on the inspected path — is closed.
#[inline]
fn reject_forbidden_create_rewrite<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &mut MegaContext<DB, ExtEnvs>,
    before: InstructionResult,
    result: &mut InterpreterResult,
) {
    if !context.spec.is_enabled(MegaSpecId::REX7) || before.is_ok() || !result.result.is_ok() {
        return;
    }
    result.result = before;
    context.additional_limit.borrow_mut().record_inspector_rejected_rewrite();
    let slot = context.error();
    if slot.is_ok() {
        *slot = Err(ContextError::Custom(String::from(FORBIDDEN_CREATE_REVIVAL)));
    }
    debug_assert!(
        false,
        "{FORBIDDEN_CREATE_REVIVAL}: {before:?} was rewritten to a success, which no journal \
         entry and no deposited code stands behind",
    );
}

impl<DB, ExtEnvs, INTR, I> Inspector<MegaContext<DB, ExtEnvs>, INTR> for MeasuredInspector<I>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    INTR: InterpreterTypes,
    I: Inspector<MegaContext<DB, ExtEnvs>, INTR>,
{
    /// Measured, but without settling a segment: this runs after the frame is built and before its
    /// settlement window is opened, so there is nothing open to close. The frame's own entry hook
    /// opens the window on whatever counter this callback leaves behind.
    #[inline]
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
    ) {
        let action = interp.bytecode.action().clone();
        let refund_before = held_refund(action.as_ref(), &interp.gas);
        let before = interp.gas.remaining();
        let working_set = WorkingSet::of(interp);
        self.inner.initialize_interp(interp, context);
        book_intervention(context, WorkingSet::of(interp) != working_set);
        let change = measure_pending_action(action, interp.bytecode.action().as_ref(), before);
        book_pending_action(context, change);
        book_refund(
            context,
            refund_before,
            held_refund(interp.bytecode.action().as_ref(), &interp.gas),
        );
        context.additional_limit.borrow_mut().record_inspector_gas_adjustment::<false>(
            &mut interp.gas,
            before,
            change.lane.counter_reaches_envelope(),
        );
    }

    #[inline]
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut MegaContext<DB, ExtEnvs>) {
        let action = interp.bytecode.action().clone();
        let refund_before = held_refund(action.as_ref(), &interp.gas);
        let before = interp.gas.remaining();
        let working_set = WorkingSet::of(interp);
        self.inner.step(interp, context);
        book_intervention(context, WorkingSet::of(interp) != working_set);
        let change = measure_pending_action(action, interp.bytecode.action().as_ref(), before);
        book_pending_action(context, change);
        book_refund(
            context,
            refund_before,
            held_refund(interp.bytecode.action().as_ref(), &interp.gas),
        );
        context.additional_limit.borrow_mut().record_inspector_gas_adjustment::<true>(
            &mut interp.gas,
            before,
            change.lane.counter_reaches_envelope(),
        );
    }

    /// The one callback that runs with an action already pending: revm runs it after the
    /// instruction that set the frame's action, so on a terminating opcode the counter it hands
    /// out has already been copied into the result the caller will be given — see
    /// [`ActionLane::counter_reaches_envelope`] — and the action holding that copy is reachable
    /// through `LoopControl`. Both objects are measured, on the lanes
    /// [`book_pending_action`] routes them to.
    #[inline]
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut MegaContext<DB, ExtEnvs>) {
        let action = interp.bytecode.action().clone();
        let refund_before = held_refund(action.as_ref(), &interp.gas);
        let before = interp.gas.remaining();
        let working_set = WorkingSet::of(interp);
        self.inner.step_end(interp, context);
        book_intervention(context, WorkingSet::of(interp) != working_set);
        let change = measure_pending_action(action, interp.bytecode.action().as_ref(), before);
        book_pending_action(context, change);
        book_refund(
            context,
            refund_before,
            held_refund(interp.bytecode.action().as_ref(), &interp.gas),
        );
        context.additional_limit.borrow_mut().record_inspector_gas_adjustment::<true>(
            &mut interp.gas,
            before,
            change.lane.counter_reaches_envelope(),
        );
    }

    /// No interpreter and no frame inputs are reachable here, so there is nothing to measure —
    /// this callback can only touch the context, which the shim does not police.
    #[inline]
    fn log(&mut self, context: &mut MegaContext<DB, ExtEnvs>, log: Log) {
        self.inner.log(context, log);
    }

    #[inline]
    fn log_full(
        &mut self,
        interpreter: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
        log: Log,
    ) {
        let action = interpreter.bytecode.action().clone();
        let refund_before = held_refund(action.as_ref(), &interpreter.gas);
        let before = interpreter.gas.remaining();
        let working_set = WorkingSet::of(interpreter);
        self.inner.log_full(interpreter, context, log);
        book_intervention(context, WorkingSet::of(interpreter) != working_set);
        let change = measure_pending_action(action, interpreter.bytecode.action().as_ref(), before);
        book_pending_action(context, change);
        book_refund(
            context,
            refund_before,
            held_refund(interpreter.bytecode.action().as_ref(), &interpreter.gas),
        );
        context.additional_limit.borrow_mut().record_inspector_gas_adjustment::<true>(
            &mut interpreter.gas,
            before,
            change.lane.counter_reaches_envelope(),
        );
    }

    #[inline]
    fn frame_start(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        let before = frame_input.clone();
        let outcome = self.inner.frame_start(context, frame_input);
        book_env_adjustment(
            context,
            frame_input_gas_limit(&before),
            frame_input_gas_limit(frame_input),
            outcome.is_some(),
        );
        if let Some(outcome) = &outcome {
            book_synthetic_refund(context, outcome.gas().refunded());
        }
        book_intervention(context, outcome.is_some() || frame_input_rewritten(before, frame_input));
        outcome
    }

    #[inline]
    fn frame_end(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        frame_input: &FrameInput,
        frame_result: &mut FrameResult,
    ) {
        let before = frame_result.instruction_result();
        let output = frame_result.interpreter_result().output.clone();
        let metadata = OutcomeMetadata::of(frame_result);
        let refund_before = frame_result.gas().refunded();
        self.inner.frame_end(context, frame_input, frame_result);
        book_refund(context, refund_before, frame_result.gas().refunded());
        book_intervention(
            context,
            result_rewritten((before, &output), frame_result.interpreter_result()),
        );
        book_intervention(context, OutcomeMetadata::of(frame_result) != metadata);
        // `frame_end` runs after `create_end` and is the last chance to rewrite a creation's
        // classification, so the same refusal applies here.
        if let FrameResult::Create(outcome) = frame_result {
            reject_forbidden_create_rewrite(context, before, &mut outcome.result);
        }
    }

    #[inline]
    fn call(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        let before = inputs.clone();
        let outcome = self.inner.call(context, inputs);
        book_env_adjustment(
            context,
            Some(before.gas_limit),
            Some(inputs.gas_limit),
            outcome.is_some(),
        );
        if let Some(outcome) = &outcome {
            book_synthetic_refund(context, outcome.result.gas.refunded());
        }
        book_intervention(context, outcome.is_some() || call_inputs_rewritten(before, inputs));
        outcome
    }

    /// `CallInputs` is immutable here and the frame's result gas is deliberately not booked at this
    /// boundary — see [`InspectorLedger::env`](crate::InspectorLedger::env) — so the only thing to
    /// measure is what the callback did to the result's classification and output.
    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let before = (outcome.result.result, outcome.result.output.clone());
        let metadata = CallMetadata::of(outcome);
        let refund_before = outcome.result.gas.refunded();
        self.inner.call_end(context, inputs, outcome);
        book_refund(context, refund_before, outcome.result.gas.refunded());
        book_intervention(context, result_rewritten((before.0, &before.1), &outcome.result));
        book_intervention(context, CallMetadata::of(outcome) != metadata);
    }

    #[inline]
    fn create(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        let before = inputs.clone();
        let outcome = self.inner.create(context, inputs);
        book_env_adjustment(
            context,
            Some(before.gas_limit()),
            Some(inputs.gas_limit()),
            outcome.is_some(),
        );
        if let Some(outcome) = &outcome {
            book_synthetic_refund(context, outcome.result.gas.refunded());
        }
        book_intervention(context, outcome.is_some() || create_inputs_rewritten(before, inputs));
        outcome
    }

    /// Forwards, then refuses a rewrite that turned a failed contract creation into a successful
    /// one: by this point the journal is already reverted and no code was deposited, so the
    /// original classification is restored and the transaction is failed with an error rather than
    /// allowed to report a deployment that did not happen.
    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        let before = (outcome.result.result, outcome.result.output.clone());
        let address = outcome.address;
        let refund_before = outcome.result.gas.refunded();
        self.inner.create_end(context, inputs, outcome);
        book_refund(context, refund_before, outcome.result.gas.refunded());
        book_intervention(context, result_rewritten((before.0, &before.1), &outcome.result));
        book_intervention(context, outcome.address != address);
        reject_forbidden_create_rewrite(context, before.0, &mut outcome.result);
    }

    /// Everything this callback receives is passed by value, so it cannot change execution state.
    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.inner.selfdestruct(contract, target, value);
    }
}
