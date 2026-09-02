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
//!   ([`OutcomeMetadata`]) — and every constant-time reading the shim can take off a live
//!   interpreter ([`WorkingSet`]), which is what makes a frame's memory grown for free, its program
//!   counter stepped past an instruction, or its return buffer conjured all visible.
//! - One rewrite shape is refused outright: see [`MeasuredInspector::create_end`].
//!
//! # The one inspector the shim does not measure
//!
//! An inspector type whose author has declared it read-only, by implementing [`TrustedObserver`]
//! in source, is delegated to without any of the above. The declaration is the only way to reach
//! that path: it is a bound on [`MeasuredInspector::new_trusted`], so an inspector chosen by a
//! request or by configuration cannot arrive on it. Debug builds take the measuring path anyway
//! and assert the ledger stayed empty, so a type declared wrongly fails where it is exercised
//! rather than where it is deployed.
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
        interpreter_types::{
            InputsTr, Jumps, LegacyBytecode, LoopControl, MemoryTr, ReturnData, RuntimeFlag,
            StackTr,
        },
        CallInput, CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput,
        InstructionResult, Interpreter, InterpreterAction, InterpreterResult, InterpreterTypes,
    },
    primitives::hardfork::SpecId,
    Inspector,
};

use crate::{ExternalEnvTypes, MegaContext, MegaSpecId};

/// The message a refused `create_end` rewrite surfaces as `EVMError::Custom`.
///
/// Public because a refusal is a designed outcome and not an execution failure: a harness that
/// drives rewriting inspectors over a corpus has to tell the two apart, and the error's message is
/// what carries the difference.
pub const FORBIDDEN_CREATE_REVIVAL: &str =
    "inspector rewrote a failed contract creation into a successful one";

/// The message a refused rewrite of a frame-init result surfaces as `EVMError::Custom`.
///
/// Public for the same reason as [`FORBIDDEN_CREATE_REVIVAL`].
pub const FORBIDDEN_FRAME_INIT_REWRITE: &str =
    "inspector moved the classification of a result frame init produced";

/// A promise, made in source about one inspector type, that none of its callbacks writes anything
/// back to the EVM.
///
/// # What implementing this declares
///
/// Every callback of this type leaves the EVM exactly as it found it: it writes nothing to an
/// interpreter's gas counter or its pending action, nothing to a frame's inputs, nothing to a
/// frame result's classification, gas, output or metadata, nothing to a refund, and it never
/// answers a frame with a synthetic outcome. It may read whatever it likes and it may write to
/// its own state. That is the whole of the promise, and it is exactly the "read-only observation"
/// row of the shape table in `evm/AGENTS.md`.
///
/// A type that keeps the promise is measured to zero on every lane of
/// [`InspectorLedger`](crate::InspectorLedger), which is the same thing the shim would have
/// concluded by measuring it — so declaring it changes what the measurement *costs* and never
/// what it *says*.
///
/// # What it buys
///
/// [`MeasuredInspector`] delegates to a declared type without taking any of its readings, which
/// puts the inspected path back on revm's own cost. Measuring costs about a nanosecond per
/// reading per opcode and there are sixteen readings taken twice per opcode, which adds between a
/// third and two thirds to a production tracer's run.
///
/// # Why a declaration and not a detection
///
/// There is nothing to detect. The shim measures at a callback boundary precisely because it
/// cannot see inside the callback, so "does this type write anything back" is not a question it
/// can ask ahead of time — only one it can answer afterwards, at the cost the declaration exists
/// to avoid.
///
/// # The rules this trait is under
///
/// - **No blanket implementation, ever.** Each implementation names one concrete type, so a
///   declaration is a line someone wrote about a type they had read. A blanket implementation would
///   make the promise about types nobody has looked at.
/// - **Not reachable from data.** The only route to the fast path is
///   [`MeasuredInspector::new_trusted`], whose bound is this trait, so an inspector selected by a
///   request, a configuration file or any other run-time value cannot arrive on it — an
///   RPC-supplied tracer is a value, and no value can carry an implementation.
/// - **Do not implement it for anything that intercepts.** An inspector that answers a frame
///   itself, edits inputs, or rewrites a result is a rewriting inspector however little it
///   rewrites; those are supported, measured, and must stay measured.
/// - **A foreign inspector needs a newtype.** The orphan rule wants one of the trait and the type
///   to be local, and for a `revm-inspectors` tracer neither is — so a node declares a newtype of
///   its own that forwards every callback. `benches/common/subject.rs` does exactly that, and is
///   the shape to copy.
///
/// # Verified in debug builds
///
/// A declared type still takes the full measurement under `debug_assertions`, and the shim
/// asserts the ledger stayed empty after every callback. A wrong declaration therefore fails in
/// tests, in CI and under the chaos sweep, at the callback that broke it.
pub trait TrustedObserver {}

/// The inspector `MegaETH` runs with when none was supplied observes nothing at all.
impl TrustedObserver for revm::inspector::NoOpInspector {}

/// A declared observer stays declared when it is handed over by reference.
///
/// revm implements `Inspector` for `&mut I`, which is how a caller keeps an inspector it can read
/// back afterwards. This lifts the declaration to the same shape, and it grants nothing: `&mut T`
/// is declared exactly when `T` is, so no type becomes trusted that was not trusted already.
impl<T: TrustedObserver + ?Sized> TrustedObserver for &mut T {}

/// Wraps a user inspector so that what it does to gas accounting is measured and booked.
///
/// `MegaETH` applies this itself — [`MegaEvm::with_inspector`](crate::MegaEvm::with_inspector) and
/// [`InspectEvm::set_inspector`](revm::InspectEvm::set_inspector) take the user's inspector by
/// value and store it wrapped, and the accessors hand back the unwrapped inspector — so the wrapper
/// is not something a caller opts into or can opt out of.
///
/// What a caller *can* opt into is being measured more cheaply, by declaring the inspector's type
/// [`TrustedObserver`] and building the shim with [`new_trusted`](Self::new_trusted). See
/// [`measures`](Self::measures) for what that changes and where it stops.
///
/// Derefs to the wrapped inspector, so `evm.inspector().whatever()` reaches the user's own type.
#[derive(Clone, Copy, Debug, Default, derive_more::Deref, derive_more::DerefMut)]
pub struct MeasuredInspector<I> {
    #[deref]
    #[deref_mut]
    inner: I,
    /// Whether the wrapped type's author declared it [`TrustedObserver`].
    ///
    /// A flag rather than a type parameter, because the type the shim is asked about is chosen by
    /// whoever calls `with_inspector`, and `MegaEvm` names the shim as `MeasuredInspector<INSP>`
    /// for whatever `INSP` that is. Answering it in the type system would mean either a bound on
    /// every inspector `MegaETH` can be handed — including the foreign ones it cannot implement
    /// anything for — or a second impl of `Inspector` that overlaps the first. So the question is
    /// answered where it is asked, at the one constructor whose bound is the declaration, and
    /// carried here.
    ///
    /// `Default` leaves it false, which is the safe direction: an unbuilt shim measures.
    trusted: bool,
}

impl<I> MeasuredInspector<I> {
    /// Wraps `inner` in the measurement shim.
    pub const fn new(inner: I) -> Self {
        Self { inner, trusted: false }
    }

    /// Whether this callback takes the measuring path.
    ///
    /// False only for a declared [`TrustedObserver`] in a release build. Under `debug_assertions`
    /// every inspector is measured, declared or not, and a declared one is additionally asserted
    /// to have booked nothing — which is what makes the declaration a claim the build system
    /// checks rather than a comment.
    ///
    /// Both halves are compile-time constants beside a `bool` the shim was built with, so the
    /// release fast path is one predictable branch and the debug path folds away entirely.
    ///
    /// This and [`verify_trusted`] read the same `debug_assertions` flag, which is what keeps the
    /// two builds from disagreeing: a profile that turns assertions on in an optimised build gets
    /// the measured path *and* the check, never one without the other.
    #[inline(always)]
    const fn measures(&self) -> bool {
        !self.trusted || cfg!(debug_assertions)
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

    /// Whether the wrapped inspector's type was declared [`TrustedObserver`].
    ///
    /// True only for a shim built by [`new_trusted`](Self::new_trusted). What it is for is the
    /// transaction-level backstop in `MegaEvm::execute_transaction`: the per-callback verification
    /// names the callback that broke a declaration, and this catches one broken at a callback
    /// whose verification is missing.
    pub const fn is_trusted(&self) -> bool {
        self.trusted
    }
}

impl<I: TrustedObserver> MeasuredInspector<I> {
    /// Wraps `inner` in a shim that delegates to it without measuring, on the strength of its
    /// type's [`TrustedObserver`] declaration.
    ///
    /// This is the only constructor that produces the fast path, and its bound is the only way to
    /// reach it. Debug builds measure anyway and assert the result is empty.
    pub const fn new_trusted(inner: I) -> Self {
        Self { inner, trusted: true }
    }
}

/// Asserts, in debug builds, that a declared [`TrustedObserver`] really booked nothing.
///
/// Called after every measured callback. The ledger accumulates over the whole transaction and is
/// reset at its start, so the first callback that breaks the promise is the one that fails —
/// later ones would fail too, but this one names the site.
///
/// Compiled out of release builds together with the measurement it checks: a declared type never
/// reaches a measuring body there at all.
#[inline]
fn verify_trusted<DB: Database, ExtEnvs: ExternalEnvTypes>(
    trusted: bool,
    context: &MegaContext<DB, ExtEnvs>,
    callback: &'static str,
) {
    #[cfg(debug_assertions)]
    if trusted {
        let ledger = context.additional_limit.borrow().inspector_ledger();
        assert!(
            ledger.is_zero(),
            "an inspector declared `TrustedObserver` wrote something back at `{callback}`: \
             {ledger:?}",
        );
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (trusted, context, callback);
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
    /// [`LiveReading`] measures exactly that, against the same counter reading, so the two
    /// together account for every unit of gas the frame holds. See [`held`] for the identity they
    /// split.
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
/// Both readings [`LiveReading`] takes use the counter *the EVM left behind*, so the counter
/// cancels out of the difference wherever it appears on both sides. What is left is exactly the
/// part of the movement that is not already on the counter lane, whatever the callback did to the
/// action's shape.
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

/// The refund a frame and its pending continuation hold, given the action it is carrying and the
/// refund on its own gas counter.
///
/// [`held`]'s counterpart on the refund dimension, and it has the same three cases for the same
/// reason — the object the EVM will read next is the one the frame is holding:
///
/// ```text
/// held_refund(None,        counter) = counter
/// held_refund(NewFrame(_), counter) = counter
/// held_refund(Return(r),   counter) = r.gas.refunded()
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
fn held_refund(action: Option<&InterpreterAction>, counter: i64) -> i64 {
    match action {
        None | Some(InterpreterAction::NewFrame(_)) => counter,
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

/// The pending action a callback was handed, in the form the boundary compares it in.
///
/// Only one question is asked of the way-in reading that the numbers beside it in [`LiveReading`]
/// do not already answer: did the action come back describing something other than what the EVM
/// decided? So this holds what that comparison reads and nothing else — the gas is taken as
/// [`held`] at the moment the reading is made, and never needs the action again.
///
/// Which matters because the way-in reading is taken twice per opcode. Copying the action itself
/// would carry an `InterpreterResult`'s output buffer and a frame input's boxed inputs across
/// every one of them; here the shape a running frame is almost always in costs a discriminant, and
/// the two that carry something are taken once per frame and once per call.
///
/// The output buffer is held rather than reduced to its identity, and that is the point of holding
/// it: [`same_buffer`] compares by address, and only an owner keeps the address it compares from
/// being reused underneath it.
#[derive(Clone, Debug)]
enum ActionSnapshot {
    /// No action pending: the frame carries straight on.
    Empty,
    /// A `Return` action — the classification and output the frame's caller will be handed.
    Return(InstructionResult, Bytes),
    /// A `NewFrame` action — the inputs a child is about to be built from. The one comparison
    /// here that needs the object rather than a reading off it.
    NewFrame(FrameInput),
}

impl ActionSnapshot {
    /// Takes the way-in reading off the action an interpreter is holding.
    #[inline(always)]
    fn of(action: Option<&InterpreterAction>) -> Self {
        match action {
            None => Self::Empty,
            Some(InterpreterAction::Return(result)) => {
                Self::Return(result.result, result.output.clone())
            }
            Some(InterpreterAction::NewFrame(frame_input)) => Self::NewFrame(frame_input.clone()),
        }
    }

    /// Whether a callback left behind an action describing something other than what the EVM
    /// decided.
    ///
    /// Gas is excluded, exactly as it is at every other boundary: it travels on the lanes
    /// [`book_pending_action`] routes it to, and counting it here as well would report one rewrite
    /// twice. A callback that installed, removed or swapped an action has rewritten what the EVM
    /// does next as thoroughly as it is possible to, so every shape change counts.
    #[inline(always)]
    fn rewritten(self, after: Option<&InterpreterAction>) -> bool {
        match (self, after) {
            (Self::Empty, None) => false,
            (Self::Return(result, output), Some(InterpreterAction::Return(after))) => {
                result_rewritten((result, &output), after)
            }
            (Self::NewFrame(before), Some(InterpreterAction::NewFrame(after))) => {
                frame_input_rewritten(before, after)
            }
            _ => true,
        }
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

/// An `O(1)` identity for a byte buffer: where it starts and how long it is.
///
/// The same comparison [`same_buffer`] makes, in a form that can be stored in a snapshot. Neither
/// reads a byte: a buffer's *contents* at an unchanged address and length are content-class, which
/// is the row of the shape table that has no lane. What this does catch is the buffer being
/// replaced, which is the only way an inspector can change one that is immutable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BufferId {
    /// Where the buffer starts, as a bare address rather than a live pointer.
    addr: usize,
    /// How long it is.
    len: usize,
}

impl BufferId {
    #[inline]
    fn of(bytes: &[u8]) -> Self {
        Self { addr: bytes.as_ptr() as usize, len: bytes.len() }
    }
}

/// An `O(1)` identity for a frame's calldata, which is either an owned buffer or a window into the
/// shared one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallInputId {
    /// An owned buffer, identified the way every other buffer here is.
    Bytes(BufferId),
    /// A window into the context's shared memory buffer, identified by its bounds. Where the
    /// window points is the reading; what is inside it lives in the shared buffer and is
    /// content-class like any other buffer's contents.
    SharedBuffer(usize, usize),
}

impl CallInputId {
    #[inline]
    fn of(input: &CallInput) -> Self {
        match input {
            CallInput::Bytes(bytes) => Self::Bytes(BufferId::of(bytes)),
            CallInput::SharedBuffer(range) => Self::SharedBuffer(range.start, range.end),
        }
    }
}

/// Every constant-time reading a callback boundary can take off a live interpreter.
///
/// # The rule
///
/// **Every `O(1)` reading of the interpreter's working set is in this snapshot.** Not a list of
/// the readings someone thought of — the readings themselves, enumerated field by field against
/// revm's `Interpreter` and the traits each field is reachable through, and pinned that way by
/// `tests/rex7/gas_surface.rs`.
///
/// The rule is stated over readings rather than over fields because that is the shape of what a
/// boundary can do. An inspector reaches the whole interpreter; the shim can only compare what it
/// can *read back* in constant time, and a snapshot the inspected path takes twice per opcode
/// cannot walk unbounded state. So the line is drawn at the cost of the reading, and everything on
/// the cheap side of it is taken.
///
/// The earlier version of this snapshot held four readings and was written as an enumeration: the
/// stack's length, the memory's size, and the two halves of the memo of how far that memory has
/// been paid for. Enumerations of this kind are only as complete as whoever wrote them, and this
/// one was not — the `bytecode` field was not in it at all, so an inspector could step the program
/// counter past an instruction and delete it from the frame with every lane reading zero. Stating
/// the rule over the cost of the reading is what closes that class rather than that instance.
///
/// # What is here, by the field it is read from
///
/// - `bytecode` — the program counter, the code's identity, and revm's `continue_execution` flag,
///   which is what the inspected loop breaks on and is a separate object from the pending action.
/// - `stack` — its length.
/// - `return_data` — the buffer's identity. A frame's `RETURNDATASIZE` and `RETURNDATACOPY` read
///   it, so putting a buffer there hands the frame data no call produced.
/// - `memory` — its size, and the offset of the frame's window into the shared buffer.
/// - `gas` — the memory memo's two halves. The budget half of a `Gas` is not here: it moves on the
///   gas lanes, and reading it here as well would report one edit twice.
/// - `input` — the four addresses and values a frame's identity is made of, and its calldata's
///   identity. `target_address` is the one every storage instruction resolves against, so moving it
///   redirects the frame's writes to another account.
/// - `runtime_flag` — the static flag and the spec id.
///
/// `extend` is the one field with no reading, by construction: `InterpreterTypes::Extend` carries
/// no trait bound at all, so a shim generic over the interpreter has nothing it can call on it.
///
/// # What is deliberately not here
///
/// The *contents* of the stack, the memory, the return buffer, the calldata and the code. Telling
/// whether any of those came back changed means walking unbounded state, which is the one thing a
/// per-opcode boundary cannot do. Their identities and sizes are here; what is inside them is the
/// row of the shape table that has no lane.
///
/// # The pair that made the snapshot necessary
///
/// The memory and its memo, moved together. The memo (`Gas::memory`) is what the next expanding
/// opcode compares its requirement against. An inspector that raises it without growing the memory
/// desynchronises the two and the EVM reads out of bounds; one that grows the memory without
/// raising it is charged for the growth twice over. Moving *both* is neither — the interpreter is
/// in a state it could have reached by paying, having paid nothing, and every later expansion
/// inside the new bound is free. That pair moves no gas at the moment it is made, so no gas lane
/// can see it; what it changes is what the EVM charges afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkingSet {
    /// Where in its code the frame is about to execute.
    pc: usize,
    /// Which code that is.
    code: BufferId,
    /// Whether revm's inspected instruction loop will take another turn.
    running: bool,
    /// How many words the frame has on its stack.
    stack_len: usize,
    /// The return data the frame's `RETURNDATASIZE` and `RETURNDATACOPY` will read.
    return_data: BufferId,
    /// How many bytes of memory the frame has.
    memory_size: usize,
    /// Where the frame's window into the shared memory buffer starts.
    memory_offset: usize,
    /// How many words of that memory the frame has been charged for.
    memory_words: usize,
    /// What that charge came to.
    memory_expansion_cost: u64,
    /// The account the frame's storage instructions resolve against.
    target_address: Address,
    /// The account whose code the frame is running.
    bytecode_address: Option<Address>,
    /// Who called it.
    caller_address: Address,
    /// With what value.
    call_value: U256,
    /// And with what calldata.
    call_input: CallInputId,
    /// Whether the frame may write state.
    is_static: bool,
    /// Which gas schedule and opcode set it runs under.
    spec_id: SpecId,
}

impl WorkingSet {
    /// Whether a live interpreter still reads the way this snapshot recorded it.
    ///
    /// The same question as `*self == Self::of(interp)`, asked without building the second
    /// snapshot. That is the whole difference, and it is worth stating because this is the way-out
    /// half of a measurement taken twice per opcode: a comparison written that way materialises
    /// two hundred bytes onto the stack for the length of one `==`, and the optimiser does not
    /// reliably take them away again.
    ///
    /// Every reading in [`of`](Self::of) is compared here, in the same order, and neither list may
    /// be shortened without the other. What holds them together is the unit tests below: each of
    /// them moves one reading and asserts both that [`moved`] names it and that this returns
    /// `false`, so a reading present in the snapshot and missing here is a reading the shim takes
    /// and never compares.
    ///
    /// Left as `inline` rather than `inline(always)` on purpose. Forced into all four callbacks it
    /// is faster beside an empty inspector and slower beside a real tracer, which is the inspector
    /// the inspected path actually carries; one copy per interpreter type is faster beside both.
    #[inline]
    fn unchanged<INTR: InterpreterTypes>(&self, interp: &Interpreter<INTR>) -> bool {
        let memory = interp.gas.memory();
        (self.pc == interp.bytecode.pc()) &&
            (self.code == BufferId::of(interp.bytecode.bytecode_slice())) &&
            (self.running == interp.bytecode.is_not_end()) &&
            (self.stack_len == interp.stack.len()) &&
            (self.return_data == BufferId::of(interp.return_data.buffer())) &&
            (self.memory_size == interp.memory.size()) &&
            (self.memory_offset == interp.memory.local_memory_offset()) &&
            (self.memory_words == memory.words_num) &&
            (self.memory_expansion_cost == memory.expansion_cost) &&
            (self.target_address == interp.input.target_address()) &&
            (self.bytecode_address.as_ref() == interp.input.bytecode_address()) &&
            (self.caller_address == interp.input.caller_address()) &&
            (self.call_value == interp.input.call_value()) &&
            (self.call_input == CallInputId::of(interp.input.input())) &&
            (self.is_static == interp.runtime_flag.is_static()) &&
            (self.spec_id == interp.runtime_flag.spec_id())
    }

    /// Takes every reading off a live interpreter.
    #[inline]
    fn of<INTR: InterpreterTypes>(interp: &Interpreter<INTR>) -> Self {
        let memory = interp.gas.memory();
        Self {
            pc: interp.bytecode.pc(),
            code: BufferId::of(interp.bytecode.bytecode_slice()),
            running: interp.bytecode.is_not_end(),
            stack_len: interp.stack.len(),
            return_data: BufferId::of(interp.return_data.buffer()),
            memory_size: interp.memory.size(),
            memory_offset: interp.memory.local_memory_offset(),
            memory_words: memory.words_num,
            memory_expansion_cost: memory.expansion_cost,
            target_address: interp.input.target_address(),
            bytecode_address: interp.input.bytecode_address().copied(),
            caller_address: interp.input.caller_address(),
            call_value: interp.input.call_value(),
            call_input: CallInputId::of(interp.input.input()),
            is_static: interp.runtime_flag.is_static(),
            spec_id: interp.runtime_flag.spec_id(),
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

/// What an inspector can ask `MegaETH` about the frame result it is holding.
///
/// One question, with one purpose: telling a result a frame *ran* to produce apart from one frame
/// init produced without ever building a frame. The two arrive at the same callback holding the
/// same type, and the difference decides what a rewrite of the classification does — a running
/// frame's journal decision is still outstanding and follows the rewrite, while an init-produced
/// result's was taken before the callback existed and is refused (see
/// [`MeasuredInspector`](MeasuredInspector#impl-Inspector)).
///
/// A tool that only observes never needs this. One that rewrites classifications does, because
/// otherwise the only way to find out which kind of result it is holding is to have its
/// transaction refused.
pub trait FrameResultOriginTr {
    /// Whether the frame result the `*_end` callbacks are being handed came out of frame init.
    ///
    /// False everywhere else, including at every callback that is not one of those three.
    fn is_frame_init_result(&self) -> bool;
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> FrameResultOriginTr for MegaContext<DB, ExtEnvs> {
    #[inline]
    fn is_frame_init_result(&self) -> bool {
        self.additional_limit.borrow().is_settling_frame_init_result()
    }
}

/// Which of the three things a frame's result says, which is the granularity the refusal below is
/// stated over.
///
/// A result's gas and its returned output move freely — those are what the lanes measure. What
/// cannot move is which of these three the caller is handed, because that is the question the
/// journal decision answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultClass {
    /// The frame returned, and its writes stand.
    Success,
    /// The frame reverted, and its writes are rolled back with its gas handed back.
    Revert,
    /// The frame halted exceptionally, and its writes are rolled back with its gas destroyed.
    Halt,
}

impl ResultClass {
    #[inline]
    const fn of(result: InstructionResult) -> Self {
        if result.is_ok() {
            Self::Success
        } else if result.is_revert() {
            Self::Revert
        } else {
            Self::Halt
        }
    }
}

/// Refuses a rewrite that moves a frame-init result across the success / revert / halt boundary.
///
/// Every other classification rewrite is supported because REX7 withholds the journal decision
/// until the result is final: the frame loops park it and `frame_return_result` carries it out
/// after the last callback, so a frame rewritten into a revert has its state rolled back with it.
///
/// A result that comes out of frame *init* has no such window, and cannot be given one from here.
/// Upstream takes the decision inside `make_call_frame`, statements before it returns — a
/// value-transferring call into an empty-code account commits the transfer and returns `Stop`, a
/// precompile that fails reverts it and returns its own failure — and `MegaETH`'s system contract
/// interceptors take theirs before they return, the `KeylessDeploy` one by merging a whole
/// sandbox's state into the journal. All of it has happened by the time a callback sees the
/// result. Honouring a rewrite would hand the caller an answer the state behind it contradicts:
/// a transfer the recipient keeps and the sender is told failed, or a deployment the caller is
/// told reverted and that stands anyway.
///
/// A result an inspector answered the frame with itself is deliberately outside the refusal, even
/// though it too reaches a callback with no frame having run: nothing in the EVM decided anything
/// for it — no checkpoint was opened, no state written — so its classification is the inspector's
/// to state and rewriting it contradicts nothing. What separates the two is which of the two
/// callback sites in `inspect_frame_init` ran, and that is where the window is opened; nothing
/// here can tell them apart on its own.
///
/// Detection only; nothing here compensates the journal. The original classification is restored,
/// the ledger counts the refusal, and the context's error slot carries the reason so the
/// transaction fails with an error rather than with a receipt built on the rewrite.
///
/// Deliberately loud but not fatal, which is where it differs from
/// [`reject_forbidden_create_rewrite`]. That shape is a mistake with no reading behind it and
/// asserting on it costs nothing. This one is the most ordinary rewrite a tool makes — failing a
/// call — landing on the one kind of frame it cannot be applied to, so a corpus that produces it
/// should be able to report it rather than die on it.
///
/// Gated to REX7+. On a frozen spec, an inspector's rewrite reaches no accounting lane that can be
/// made unsound by it, and the specs' behaviour — including on the inspected path — is closed.
#[inline]
fn reject_forbidden_frame_init_rewrite<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &mut MegaContext<DB, ExtEnvs>,
    before: InstructionResult,
    result: &mut InterpreterResult,
) {
    if !context.spec.is_enabled(MegaSpecId::REX7) ||
        ResultClass::of(before) == ResultClass::of(result.result) ||
        !context.additional_limit.borrow().is_settling_frame_init_result()
    {
        return;
    }
    result.result = before;
    context.additional_limit.borrow_mut().record_inspector_rejected_rewrite();
    let slot = context.error();
    if slot.is_ok() {
        *slot = Err(ContextError::Custom(String::from(FORBIDDEN_FRAME_INIT_REWRITE)));
    }
    debug_assert_eq!(
        ResultClass::of(result.result),
        ResultClass::of(before),
        "{FORBIDDEN_FRAME_INIT_REWRITE}: the refusal must leave the caller holding the \
         classification the EVM produced",
    );
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

/// What the shim reads off a live interpreter on the way into a callback, and settles on the way
/// out.
///
/// The four callbacks that are handed a live interpreter run the same measurement, and it is
/// written once here rather than four times: [`enter`](Self::enter) takes the way-in readings, the
/// user's inspector runs, and [`leave`](Self::leave) takes them again and books the differences.
/// The four used to carry a copy of that body each, which is four places for a boundary to be
/// measured differently at.
///
/// `IN_OPEN_SEGMENT` on [`leave`](Self::leave) is the one thing that differs between the four:
/// `initialize_interp` runs before the frame's settlement window is opened, so there is no open
/// segment for a counter edit to be moved out of.
struct LiveReading {
    /// Every constant-time reading of the interpreter's working set.
    working_set: WorkingSet,
    /// The pending action, in the form the rewrite comparison reads it in.
    action: ActionSnapshot,
    /// [`held`], taken at the counter below — the way-in half of the action lane's difference.
    held: i128,
    /// [`held_refund`], taken at the same moment.
    refund: i64,
    /// The interpreter's own gas counter, which is also the counter both [`held`] readings are
    /// taken at.
    gas: u64,
}

impl LiveReading {
    /// Takes every way-in reading off a live interpreter.
    #[inline(always)]
    fn enter<INTR: InterpreterTypes>(interp: &mut Interpreter<INTR>) -> Self {
        let working_set = WorkingSet::of(interp);
        let gas = interp.gas.remaining();
        let refunded = interp.gas.refunded();
        let action = interp.bytecode.action().as_ref();
        Self {
            working_set,
            held: held(action, gas),
            refund: held_refund(action, refunded),
            action: ActionSnapshot::of(action),
            gas,
        }
    }

    /// Takes the readings again and books what the callback moved.
    #[inline(always)]
    fn leave<const IN_OPEN_SEGMENT: bool, DB, ExtEnvs, INTR>(
        self,
        interp: &mut Interpreter<INTR>,
        context: &MegaContext<DB, ExtEnvs>,
    ) where
        DB: Database,
        ExtEnvs: ExternalEnvTypes,
        INTR: InterpreterTypes,
    {
        let moved = !self.working_set.unchanged(interp);
        let gas = interp.gas.remaining();
        let refunded = interp.gas.refunded();
        let action = interp.bytecode.action().as_ref();
        let lane = ActionLane::of(action);
        let change = ActionChange {
            // Both readings are taken at the counter the EVM left behind, so the counter cancels
            // out of the difference wherever it appears on both sides.
            gas: held(action, self.gas) - self.held,
            rewritten: self.action.rewritten(action),
            lane,
        };
        let refund = held_refund(action, refunded);

        book_intervention(context, moved);
        book_pending_action(context, change);
        book_refund(context, self.refund, refund);
        // `record_inspector_gas_adjustment` returns on its own when the counter did not move, and
        // the borrow it would take to find that out is not free on a path taken twice per opcode.
        if gas != self.gas {
            context
                .additional_limit
                .borrow_mut()
                .record_inspector_gas_adjustment::<IN_OPEN_SEGMENT>(
                    &mut interp.gas,
                    self.gas,
                    lane.counter_reaches_envelope(),
                );
        }
    }
}

/// The measuring bodies of the four live-interpreter callbacks, kept out of line.
///
/// Each callback is two things: a branch on the declaration, and — when it is taken — the
/// measurement. Only the branch belongs in revm's instruction loop, and `inline(never)` is what
/// puts it there alone. Inlined, the measurement's two hundred bytes of readings are laid down
/// inside the loop for a declared observer that never executes them, and the loop pays for them
/// in registers and instruction cache all the same.
///
/// That cost is most of what a declared observer was paying. On `interpreter_hotloop` with an
/// empty inspector, outlining takes the declared path from 1.13× the pre-shim inspected loop to
/// 1.04× on REX6, and from 1.07× to 1.00× on REX7 — the branch alone is nearly free, and the
/// bloat was not.
///
/// It is not free for the undeclared path, and the trade was measured both ways rather than
/// assumed. Beside a real tracer — the inspector the inspected path carries in production — the
/// undeclared path gets 3–6% *faster*, for the same reason the declared one does. Beside an empty
/// inspector it gets 12–19% slower, because there the call is the whole of the work. The empty
/// inspector is an instrument for isolating this shim's cost and not a workload, and it is the
/// only row that moves the wrong way.
///
/// Written out four times rather than taken as a function pointer, which would cost the
/// undeclared path the inlining of the inner inspector's own callback.
impl<I> MeasuredInspector<I> {
    #[inline(never)]
    fn initialize_interp_measured<DB, ExtEnvs, INTR>(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
    ) where
        DB: Database,
        ExtEnvs: ExternalEnvTypes,
        INTR: InterpreterTypes,
        I: Inspector<MegaContext<DB, ExtEnvs>, INTR>,
    {
        let reading = LiveReading::enter(interp);
        self.inner.initialize_interp(interp, context);
        reading.leave::<false, _, _, _>(interp, context);
        verify_trusted(self.trusted, context, "initialize_interp");
    }

    #[inline(never)]
    fn step_measured<DB, ExtEnvs, INTR>(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
    ) where
        DB: Database,
        ExtEnvs: ExternalEnvTypes,
        INTR: InterpreterTypes,
        I: Inspector<MegaContext<DB, ExtEnvs>, INTR>,
    {
        let reading = LiveReading::enter(interp);
        self.inner.step(interp, context);
        reading.leave::<true, _, _, _>(interp, context);
        verify_trusted(self.trusted, context, "step");
    }

    #[inline(never)]
    fn step_end_measured<DB, ExtEnvs, INTR>(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
    ) where
        DB: Database,
        ExtEnvs: ExternalEnvTypes,
        INTR: InterpreterTypes,
        I: Inspector<MegaContext<DB, ExtEnvs>, INTR>,
    {
        let reading = LiveReading::enter(interp);
        self.inner.step_end(interp, context);
        reading.leave::<true, _, _, _>(interp, context);
        verify_trusted(self.trusted, context, "step_end");
    }

    #[inline(never)]
    fn log_full_measured<DB, ExtEnvs, INTR>(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
        log: Log,
    ) where
        DB: Database,
        ExtEnvs: ExternalEnvTypes,
        INTR: InterpreterTypes,
        I: Inspector<MegaContext<DB, ExtEnvs>, INTR>,
    {
        let reading = LiveReading::enter(interp);
        self.inner.log_full(interp, context, log);
        reading.leave::<true, _, _, _>(interp, context);
        verify_trusted(self.trusted, context, "log_full");
    }
}

/// Books what an entry callback did to the frame it was handed.
///
/// `frame_start`, `call` and `create` are one measurement over three argument types, and this is
/// the body: the envelope moved, whether anything else about the inputs came back changed, and —
/// when the callback answered the frame itself — the refund its synthetic outcome carries.
/// `intercepted_refund` is `Some` exactly when it did.
#[inline]
fn book_frame_entry<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &MegaContext<DB, ExtEnvs>,
    before: Option<u64>,
    after: Option<u64>,
    intercepted_refund: Option<i64>,
    rewritten: bool,
) {
    let intercepted = intercepted_refund.is_some();
    book_env_adjustment(context, before, after, intercepted);
    if let Some(refund) = intercepted_refund {
        book_synthetic_refund(context, refund);
    }
    book_intervention(context, intercepted || rewritten);
}

/// What a finished frame reads as on the way into an `*_end` callback.
///
/// The three `*_end` callbacks are one measurement over three argument types, the way the three
/// entry callbacks are. `M` is whatever the object carries outside the `InterpreterResult` the
/// three of them share.
struct FrameEnding<M> {
    result: InstructionResult,
    output: Bytes,
    metadata: M,
    refund: i64,
}

impl<M: PartialEq> FrameEnding<M> {
    /// Books what the callback did to a finished frame, and refuses the rewrites that are
    /// forbidden.
    ///
    /// `is_create` selects the second refusal. Both read the classification as it stood on the way
    /// in, and the frame-init one runs first because it restores whatever it refuses — which
    /// leaves the creation refusal nothing to see.
    #[inline]
    fn book<DB: Database, ExtEnvs: ExternalEnvTypes>(
        self,
        context: &mut MegaContext<DB, ExtEnvs>,
        result: &mut InterpreterResult,
        metadata: M,
        is_create: bool,
    ) {
        book_refund(context, self.refund, result.gas.refunded());
        book_intervention(context, result_rewritten((self.result, &self.output), result));
        book_intervention(context, metadata != self.metadata);
        reject_forbidden_frame_init_rewrite(context, self.result, result);
        if is_create {
            reject_forbidden_create_rewrite(context, self.result, result);
        }
    }
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
    #[inline(always)]
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
    ) {
        if !self.measures() {
            return self.inner.initialize_interp(interp, context);
        }
        self.initialize_interp_measured(interp, context);
    }

    #[inline(always)]
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut MegaContext<DB, ExtEnvs>) {
        if !self.measures() {
            return self.inner.step(interp, context);
        }
        self.step_measured(interp, context);
    }

    /// The one callback that runs with an action already pending: revm runs it after the
    /// instruction that set the frame's action, so on a terminating opcode the counter it hands
    /// out has already been copied into the result the caller will be given — see
    /// [`ActionLane::counter_reaches_envelope`] — and the action holding that copy is reachable
    /// through `LoopControl`. Both objects are measured, on the lanes
    /// [`book_pending_action`] routes them to.
    #[inline(always)]
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut MegaContext<DB, ExtEnvs>) {
        if !self.measures() {
            return self.inner.step_end(interp, context);
        }
        self.step_end_measured(interp, context);
    }

    /// No interpreter and no frame inputs are reachable here, so there is nothing to measure —
    /// this callback can only touch the context, which the shim does not police.
    #[inline]
    fn log(&mut self, context: &mut MegaContext<DB, ExtEnvs>, log: Log) {
        self.inner.log(context, log);
    }

    #[inline(always)]
    fn log_full(
        &mut self,
        interpreter: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
        log: Log,
    ) {
        if !self.measures() {
            return self.inner.log_full(interpreter, context, log);
        }
        self.log_full_measured(interpreter, context, log);
    }

    #[inline]
    fn frame_start(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        if !self.measures() {
            return self.inner.frame_start(context, frame_input);
        }
        let before = frame_input.clone();
        let outcome = self.inner.frame_start(context, frame_input);
        book_frame_entry(
            context,
            frame_input_gas_limit(&before),
            frame_input_gas_limit(frame_input),
            outcome.as_ref().map(|outcome| outcome.gas().refunded()),
            frame_input_rewritten(before, frame_input),
        );
        verify_trusted(self.trusted, context, "frame_start");
        outcome
    }

    #[inline]
    fn frame_end(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        frame_input: &FrameInput,
        frame_result: &mut FrameResult,
    ) {
        if !self.measures() {
            return self.inner.frame_end(context, frame_input, frame_result);
        }
        let entry = FrameEnding {
            result: frame_result.instruction_result(),
            output: frame_result.interpreter_result().output.clone(),
            metadata: OutcomeMetadata::of(frame_result),
            refund: frame_result.gas().refunded(),
        };
        self.inner.frame_end(context, frame_input, frame_result);
        let metadata = OutcomeMetadata::of(frame_result);
        let is_create = matches!(frame_result, FrameResult::Create(_));
        entry.book(context, frame_result.interpreter_result_mut(), metadata, is_create);
        verify_trusted(self.trusted, context, "frame_end");
    }

    #[inline]
    fn call(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        if !self.measures() {
            return self.inner.call(context, inputs);
        }
        let before = inputs.clone();
        let outcome = self.inner.call(context, inputs);
        book_frame_entry(
            context,
            Some(before.gas_limit),
            Some(inputs.gas_limit),
            outcome.as_ref().map(|outcome| outcome.result.gas.refunded()),
            call_inputs_rewritten(before, inputs),
        );
        verify_trusted(self.trusted, context, "call");
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
        if !self.measures() {
            return self.inner.call_end(context, inputs, outcome);
        }
        let entry = FrameEnding {
            result: outcome.result.result,
            output: outcome.result.output.clone(),
            metadata: CallMetadata::of(outcome),
            refund: outcome.result.gas.refunded(),
        };
        self.inner.call_end(context, inputs, outcome);
        let metadata = CallMetadata::of(outcome);
        entry.book(context, &mut outcome.result, metadata, false);
        verify_trusted(self.trusted, context, "call_end");
    }

    #[inline]
    fn create(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        if !self.measures() {
            return self.inner.create(context, inputs);
        }
        let before = inputs.clone();
        let outcome = self.inner.create(context, inputs);
        book_frame_entry(
            context,
            Some(before.gas_limit()),
            Some(inputs.gas_limit()),
            outcome.as_ref().map(|outcome| outcome.result.gas.refunded()),
            create_inputs_rewritten(before, inputs),
        );
        verify_trusted(self.trusted, context, "create");
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
        if !self.measures() {
            return self.inner.create_end(context, inputs, outcome);
        }
        let entry = FrameEnding {
            result: outcome.result.result,
            output: outcome.result.output.clone(),
            metadata: outcome.address,
            refund: outcome.result.gas.refunded(),
        };
        self.inner.create_end(context, inputs, outcome);
        let address = outcome.address;
        entry.book(context, &mut outcome.result, address, true);
        verify_trusted(self.trusted, context, "create_end");
    }

    /// Everything this callback receives is passed by value, so it cannot change execution state.
    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.inner.selfdestruct(contract, target, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{
        bytecode::Bytecode,
        interpreter::{
            interpreter::{EthInterpreter, ExtBytecode},
            InputsImpl, InterpreterAction, SharedMemory,
        },
    };

    /// A second address, for the cases that move one of the frame's identifying addresses.
    const OTHER: Address = Address::repeat_byte(0x0B);

    /// How many bytes of memory the probe starts with.
    ///
    /// Non-zero so that the window-offset case can move the frame's checkpoint into the shared
    /// buffer without the size moving with it.
    const PROBE_MEMORY: usize = 32;

    /// The interpreter every case moves a reading on.
    fn probe() -> Interpreter<EthInterpreter> {
        let mut interp = Interpreter::new(
            SharedMemory::new(),
            ExtBytecode::new(Bytecode::new_raw(Bytes::from_static(&[0x5B, 0x5B, 0x00]))),
            InputsImpl::default(),
            false,
            SpecId::default(),
            1_000_000,
        );
        interp.memory.resize(PROBE_MEMORY);
        interp
    }

    /// The readings that differ between two snapshots, by name.
    ///
    /// Destructured exhaustively rather than compared with `PartialEq`, and that is the whole
    /// point: a reading added to [`WorkingSet`] is a compile error here until it is named, and one
    /// removed is a compile error too. Rust cannot enumerate a struct's fields at run time, and
    /// this is the substitute — the same role the derived `Debug` rendering plays for the foreign
    /// structs in `tests/rex7/gas_surface.rs`.
    fn moved(before: &WorkingSet, after: &WorkingSet) -> Vec<&'static str> {
        let WorkingSet {
            pc,
            code,
            running,
            stack_len,
            return_data,
            memory_size,
            memory_offset,
            memory_words,
            memory_expansion_cost,
            target_address,
            bytecode_address,
            caller_address,
            call_value,
            call_input,
            is_static,
            spec_id,
        } = *before;
        [
            ("pc", pc == after.pc),
            ("code", code == after.code),
            ("running", running == after.running),
            ("stack_len", stack_len == after.stack_len),
            ("return_data", return_data == after.return_data),
            ("memory_size", memory_size == after.memory_size),
            ("memory_offset", memory_offset == after.memory_offset),
            ("memory_words", memory_words == after.memory_words),
            ("memory_expansion_cost", memory_expansion_cost == after.memory_expansion_cost),
            ("target_address", target_address == after.target_address),
            ("bytecode_address", bytecode_address == after.bytecode_address),
            ("caller_address", caller_address == after.caller_address),
            ("call_value", call_value == after.call_value),
            ("call_input", call_input == after.call_input),
            ("is_static", is_static == after.is_static),
            ("spec_id", spec_id == after.spec_id),
        ]
        .into_iter()
        .filter_map(|(name, same)| (!same).then_some(name))
        .collect()
    }

    /// One case: the name of a reading, and a rewrite that moves it.
    type Case = (&'static str, fn(&mut Interpreter<EthInterpreter>));

    /// One rewrite per reading, each moving the reading it is named for and nothing else.
    ///
    /// Every one is something an inspector can do to a live interpreter through the traits the
    /// shim itself reads through, and several are rewrites with teeth: stepping the program
    /// counter deletes an instruction from the frame, clearing the static flag lets a
    /// `STATICCALL` write state, and moving the target address redirects every storage
    /// instruction to another account.
    const CASES: [Case; 16] = [
        ("pc", |interp| interp.bytecode.relative_jump(1)),
        ("code", |interp| {
            interp.bytecode = ExtBytecode::new(Bytecode::new_raw(Bytes::from_static(&[0x00])));
        }),
        ("running", |interp| {
            let gas = interp.gas;
            interp.bytecode.set_action(InterpreterAction::new_halt(InstructionResult::Stop, gas));
        }),
        ("stack_len", |interp| assert!(interp.stack.push(U256::ZERO))),
        ("return_data", |interp| interp.return_data.set_buffer(Bytes::from_static(&[0x01]))),
        ("memory_size", |interp| interp.memory.resize(PROBE_MEMORY * 2)),
        ("memory_offset", |interp| {
            // A child window over the same shared buffer, resized to the size the parent had, so
            // that the frame's memory looks the same and starts somewhere else.
            let mut child = interp.memory.new_child_context();
            child.resize(PROBE_MEMORY);
            interp.memory = child;
        }),
        ("memory_words", |interp| interp.gas.memory_mut().words_num += 1),
        ("memory_expansion_cost", |interp| interp.gas.memory_mut().expansion_cost += 1),
        ("target_address", |interp| interp.input.target_address = OTHER),
        ("bytecode_address", |interp| interp.input.bytecode_address = Some(OTHER)),
        ("caller_address", |interp| interp.input.caller_address = OTHER),
        ("call_value", |interp| interp.input.call_value = U256::from(1)),
        ("call_input", |interp| {
            interp.input.input = CallInput::Bytes(Bytes::from_static(&[0x01]));
        }),
        ("is_static", |interp| interp.runtime_flag.is_static = true),
        ("spec_id", |interp| interp.runtime_flag.spec_id = SpecId::FRONTIER),
    ];

    /// ★ Every reading the snapshot holds is one the shim really takes off the interpreter.
    ///
    /// The rule the snapshot is built on is "every `O(1)` reading of the interpreter's working
    /// set", and a rule of that shape fails in two ways: a reading that is declared and never
    /// read, and a reading that is read and never declared. Each case moves exactly one reading
    /// and asserts that exactly that one name comes back — so a field dropped from
    /// [`WorkingSet::of`] leaves its case detecting nothing, and a field left out of the snapshot
    /// entirely never compiles past [`moved`].
    ///
    /// Each case also asserts that [`WorkingSet::unchanged`] sees the same movement, which is the
    /// third way the rule can fail. That comparison is written out rather than derived from the
    /// snapshot, so a reading missing from it is a reading the shim takes, stores, and never
    /// compares — [`moved`] would still name it and the shim would still book nothing.
    #[test]
    fn test_every_reading_moves_exactly_the_reading_it_is_named_for() {
        let unchanged = probe();
        assert!(
            moved(&WorkingSet::of(&unchanged), &WorkingSet::of(&unchanged)).is_empty(),
            "a snapshot compared against itself must report nothing moved",
        );
        assert!(
            WorkingSet::of(&unchanged).unchanged(&unchanged),
            "and an interpreter nothing touched must still read the way it was recorded",
        );
        assert_ne!(SpecId::default(), SpecId::FRONTIER, "the spec-id case must move something");

        for (name, rewrite) in CASES {
            let mut interp = probe();
            let before = WorkingSet::of(&interp);
            rewrite(&mut interp);
            let after = WorkingSet::of(&interp);
            assert_eq!(
                moved(&before, &after),
                [name],
                "the rewrite for {name} must move that reading and no other",
            );
            assert!(
                !before.unchanged(&interp),
                "and the comparison the shim makes must see {name} move",
            );
        }
    }

    /// One rewrite per field a finished call carries besides its `InterpreterResult`.
    ///
    /// `CallMetadata` is the same kind of snapshot as [`WorkingSet`] over a different object, and
    /// it fails the same way: a field held and never compared is a rewrite the shim is handed and
    /// books nothing for. Three of these move nothing `MegaETH` produces today — it runs with
    /// EIP-8037 off and no wired precompile emits a log — so no fixture can show their effect, and
    /// a per-field check is the only thing that holds the claim that they are seen at all.
    const OUTCOME_CASES: [(&str, fn(&mut CallOutcome)); 4] = [
        ("memory_offset", |outcome| outcome.memory_offset = 1..2),
        ("was_precompile_called", |outcome| outcome.was_precompile_called = true),
        ("precompile_call_logs", |outcome| {
            outcome.precompile_call_logs.push(Log::new_unchecked(OTHER, Vec::new(), Bytes::new()));
        }),
        ("charged_new_account_state_gas", |outcome| {
            outcome.charged_new_account_state_gas = true;
        }),
    ];

    fn call_outcome() -> CallOutcome {
        CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Stop,
                Bytes::new(),
                revm::interpreter::Gas::new(0),
            ),
            0..0,
        )
    }

    /// Every field of a finished call outside its result is compared, and each one on its own.
    ///
    /// The derived `PartialEq` is what makes a new upstream field visible: it joins the struct,
    /// joins the equality, and the identical pair below still agrees until someone gives it a
    /// case. What the cases add is that each existing field is compared *individually* — one of
    /// them moving is enough on its own.
    #[test]
    fn test_every_finished_call_field_outside_the_result_is_compared() {
        let base = call_outcome();
        assert_eq!(
            CallMetadata::of(&base),
            CallMetadata::of(&call_outcome()),
            "two identical outcomes must compare equal",
        );
        for (name, rewrite) in OUTCOME_CASES {
            let mut moved = call_outcome();
            rewrite(&mut moved);
            assert_ne!(
                CallMetadata::of(&base),
                CallMetadata::of(&moved),
                "a rewritten {name} must be visible to the shim",
            );
        }
    }

    /// The case list covers the snapshot, and covers each reading once.
    ///
    /// [`moved`]'s destructuring is what keeps [`WorkingSet`] and this module in step at compile
    /// time; this is the run-time half of the same closure. The snapshot below is written out
    /// field by field with every reading different, so what [`moved`] reports on it is the set of
    /// readings [`moved`] can see at all — and that set has to be exactly the set the cases above
    /// exercise. A reading added to the snapshot is a compile error in two places before it gets
    /// here, and an unexercised one fails this.
    #[test]
    fn test_the_case_list_covers_every_reading_exactly_once() {
        let mut names: Vec<&str> = CASES.iter().map(|(name, _)| *name).collect();
        let declared = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), declared, "no reading may be listed twice");

        let before = WorkingSet::of(&probe());
        let everything = WorkingSet {
            pc: before.pc + 1,
            code: BufferId { addr: before.code.addr + 1, len: before.code.len + 1 },
            running: !before.running,
            stack_len: before.stack_len + 1,
            return_data: BufferId {
                addr: before.return_data.addr + 1,
                len: before.return_data.len + 1,
            },
            memory_size: before.memory_size + 1,
            memory_offset: before.memory_offset + 1,
            memory_words: before.memory_words + 1,
            memory_expansion_cost: before.memory_expansion_cost + 1,
            target_address: OTHER,
            bytecode_address: Some(OTHER),
            caller_address: OTHER,
            call_value: before.call_value + U256::from(1),
            call_input: CallInputId::SharedBuffer(0, 1),
            is_static: !before.is_static,
            spec_id: SpecId::FRONTIER,
        };
        let mut all = moved(&before, &everything);
        all.sort_unstable();
        assert_eq!(all, names, "every reading the snapshot holds must have a case, and vice versa");
    }
}
