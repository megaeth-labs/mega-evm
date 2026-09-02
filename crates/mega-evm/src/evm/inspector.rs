//! The measurement shim every inspector handed to `MegaETH` is wrapped in.
//!
//! An inspector is not a passive observer. Every callback that receives a live interpreter can
//! write to its gas counter and to the action it is holding, and every callback that receives a
//! frame's inputs can change the gas limit the frame is about to be built with. `MegaETH` meters
//! compute gas by watching those exact counters and derives what a transaction destroyed from the
//! envelope it spent, so an unmeasured edit reads as the EVM having done less work than it did.
//!
//! The EVM does not execute inside a callback, so anything that changes between the moment the
//! shim delegates and the moment control comes back is the inspector's by construction. The shim
//! snapshots on the way in, compares on the way out, and books the difference — which is why it
//! sits at the `Inspector` implementation layer: wrapping the object reaches every boundary, and
//! mirroring `inspect_instructions` would take on a core dispatch loop for no additional reach.
//!
//! Where each measurement goes is [`InspectorLedger`](crate::InspectorLedger)'s own documentation.
//! Two things here are not differences across a boundary. A callback that answers a frame itself
//! stages the envelope it was handed, because no frame is built and there is no other side to
//! compare against. And the EIP-8037 state-gas dimension is settled once by the transaction,
//! because revm propagates it by replacement and a boundary difference would book edits the EVM
//! goes on to erase.
//!
//! An inspector type whose author has declared it read-only, by implementing [`TrustedObserver`]
//! in source, is delegated to without any of this. Debug builds measure it anyway and assert the
//! ledger stayed empty, so a wrong declaration fails where it is exercised rather than where it is
//! deployed.
//!
//! Nothing here changes what an inspector may do to the EVM, and nothing here runs on the
//! uninspected path — revm's plain interpreter loop never calls an inspector at all.
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

use crate::{AdditionalLimit, ExternalEnvTypes, MegaContext, MegaSpecId};

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
/// Every callback of a declared type leaves the EVM exactly as it found it: it writes nothing to
/// an interpreter's gas counter or its pending action, nothing to a frame's inputs, nothing to a
/// frame result's classification, gas, output or metadata, nothing to a refund, and it never
/// answers a frame with a synthetic outcome. It may read whatever it likes and write to its own
/// state.
///
/// What the declaration buys is the cost of the measurement, never its verdict: a type that keeps
/// the promise measures to zero on every lane anyway. It is a declaration rather than a detection
/// because the shim measures at a boundary precisely because it cannot see inside a callback, so
/// "does this write anything back" is not a question it can ask ahead of time.
///
/// # The rules this trait is under
///
/// - **No blanket implementation, ever.** Each implementation names one concrete type, so a
///   declaration is a line someone wrote about a type they had read.
/// - **Not reachable from data.** The only route to the fast path is
///   [`MeasuredInspector::new_trusted`], whose bound is this trait — an RPC-supplied tracer is a
///   value, and no value can carry an implementation.
/// - **Do not implement it for anything that intercepts.** An inspector that answers a frame
///   itself, edits inputs, or rewrites a result is a rewriting inspector however little it
///   rewrites; those are supported, measured, and must stay measured.
/// - **A foreign inspector needs a wrapper.** The orphan rule wants one of the trait and the type
///   to be local, and for a `revm-inspectors` tracer neither is, so a node wraps it in
///   [`DeclaredObserver`], which is local here and carries the declaration.
///
/// Debug builds measure a declared type anyway and assert the ledger stayed empty after every
/// callback, so a wrong declaration fails at the callback that broke it.
pub trait TrustedObserver {}

/// The inspector `MegaETH` runs with when none was supplied observes nothing at all.
impl TrustedObserver for revm::inspector::NoOpInspector {}

/// A declared observer stays declared when it is handed over by reference.
///
/// revm implements `Inspector` for `&mut I`, which is how a caller keeps an inspector it can read
/// back afterwards. This lifts the declaration to the same shape, and it grants nothing: `&mut T`
/// is declared exactly when `T` is, so no type becomes trusted that was not trusted already.
impl<T: TrustedObserver + ?Sized> TrustedObserver for &mut T {}

/// Carries a [`TrustedObserver`] declaration for an inspector whose type cannot carry one.
///
/// The orphan rule wants one of the trait and the type to be local, and for a `revm-inspectors`
/// tracer used from a node neither is. This is the local half, supplied once here so that every
/// embedder does not write it again: it forwards every callback of the `Inspector` trait to the
/// inspector inside it and adds nothing of its own.
///
/// The declaration is still an assertion someone makes in source about one concrete inspector —
/// `DeclaredObserver` only moves where it is written, from a newtype's definition to the line that
/// wraps the value. `DeclaredObserver(tracer)` says "I have read this tracer and it writes nothing
/// back to the EVM", exactly as a hand-written forwarding newtype did, and it is subject to the
/// same rules: wrapping something that intercepts or rewrites is a false declaration, and a debug
/// build will fail at the callback that breaks it. It is a way of writing the promise, not a way
/// around it.
///
/// ```ignore
/// let executor = factory.create_executor_with_trusted_inspector(
///     db,
///     block_ctx,
///     evm_env,
///     DeclaredObserver(TracingInspector::new(TracingInspectorConfig::all())),
/// );
/// ```
///
/// Forwarding by hand is what this replaces, and the reason is that a hand-written forwarder fails
/// quietly: every `Inspector` method has a default body, so a callback revm adds and a forwarder
/// misses is not a compile error but a callback the wrapped inspector stops receiving.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeclaredObserver<I>(pub I);

impl<I> DeclaredObserver<I> {
    /// Declares `inner` read-only and wraps it.
    pub const fn new(inner: I) -> Self {
        Self(inner)
    }

    /// The declared inspector.
    pub const fn inner(&self) -> &I {
        &self.0
    }

    /// The declared inspector, mutably.
    pub const fn inner_mut(&mut self) -> &mut I {
        &mut self.0
    }

    /// Unwraps the declaration, returning the inspector it was made about.
    pub fn into_inner(self) -> I {
        self.0
    }
}

/// The whole of what the wrapper is for.
impl<I> TrustedObserver for DeclaredObserver<I> {}

/// Every callback of revm 40's `Inspector`, forwarded unchanged.
///
/// Written out in full rather than left to the trait's default bodies: a default body does not
/// forward, it does nothing, so an unlisted callback would be one the wrapped inspector silently
/// stops receiving. `tests/block_executor/declared_observer.rs` compares the callback sequence a
/// recording inspector sees wrapped against the one it sees bare, which is what turns an upstream
/// callback added here and not forwarded into a failing test.
impl<CTX, INTR, FI, FR, I> Inspector<CTX, INTR, FI, FR> for DeclaredObserver<I>
where
    INTR: InterpreterTypes,
    I: Inspector<CTX, INTR, FI, FR>,
{
    fn initialize_interp(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.0.initialize_interp(interp, context);
    }

    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.0.step(interp, context);
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.0.step_end(interp, context);
    }

    fn log(&mut self, context: &mut CTX, log: Log) {
        self.0.log(context, log);
    }

    fn log_full(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX, log: Log) {
        self.0.log_full(interp, context, log);
    }

    fn frame_start(&mut self, context: &mut CTX, frame_input: &mut FI) -> Option<FR> {
        self.0.frame_start(context, frame_input)
    }

    fn frame_end(&mut self, context: &mut CTX, frame_input: &FI, frame_result: &mut FR) {
        self.0.frame_end(context, frame_input, frame_result);
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.0.call(context, inputs)
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.0.call_end(context, inputs, outcome);
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.0.create(context, inputs)
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.0.create_end(context, inputs, outcome);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.0.selfdestruct(contract, target, value);
    }
}

/// Wraps a user inspector so that what it does to gas accounting is measured and booked.
///
/// `MegaETH` applies this itself, so the wrapper is not something a caller opts into or can opt
/// out of. What a caller *can* opt into is being measured more cheaply, by declaring the
/// inspector's type [`TrustedObserver`] and building the shim with
/// [`new_trusted`](Self::new_trusted).
///
/// Derefs to the wrapped inspector, so `evm.inspector().whatever()` reaches the user's own type.
#[derive(Clone, Copy, Debug, Default, derive_more::Deref, derive_more::DerefMut)]
pub struct MeasuredInspector<I> {
    #[deref]
    #[deref_mut]
    inner: I,
    /// Whether the wrapped type's author declared it [`TrustedObserver`].
    ///
    /// A flag rather than a type parameter: answering it in the type system would need either a
    /// bound on every inspector `MegaETH` can be handed, including the foreign ones it cannot
    /// implement anything for, or a second overlapping impl of `Inspector`. So it is answered at
    /// the one constructor whose bound is the declaration and carried here. `Default` leaves it
    /// false, which is the safe direction: an unbuilt shim measures.
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
    /// every inspector is measured and a declared one is additionally asserted to have booked
    /// nothing, which is what makes the declaration a checked claim rather than a comment.
    ///
    /// This and [`verify_trusted`] read the same flag, so a profile that turns assertions on in an
    /// optimised build gets the measured path *and* the check, never one without the other.
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
    /// Read by the transaction-level backstop, which catches a declaration broken at a callback
    /// whose own verification is missing.
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
/// Called after every measured callback, so the first one to break the promise is the one that
/// fails and names the site. Compiled out of release builds along with the measurement it checks.
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
    /// It cannot exactly on [`Result`](Self::Result): the terminating instruction has already
    /// copied the counter into the action that becomes the frame's result, so a callback writing
    /// to the counter afterwards writes into an object nobody reads again. The other two shapes
    /// are live — a frame with no action carries straight on, and a suspending one resumes on this
    /// very counter — so "the loop is about to break" is not the question, and a rule phrased that
    /// way would stop booking edits that really do move a budget.
    ///
    /// Read *after* the callback returns, because the question is about the counter it left
    /// behind. Only the ledger is gated on this: the checkpoint baseline shifts for a dead-window
    /// edit exactly as it does for a live one, or gas the inspector wrote in would read as work
    /// the frame performed.
    #[inline]
    const fn counter_reaches_envelope(self) -> bool {
        !matches!(self, Self::Result)
    }
}

/// The gas a frame and its pending continuation hold, given the action it is carrying and its own
/// counter.
///
/// ```text
/// held(None,        counter) = counter          // will spend its counter
/// held(NewFrame(f), counter) = counter + f.gas_limit  // and has handed the child's on
/// held(Return(r),   counter) = r.gas.remaining()      // the caller reclaims the action's copy
/// ```
///
/// Both readings [`LiveReading`] takes use the counter *the EVM left behind*, so the counter
/// cancels out of the difference wherever it appears on both sides. What is left is the part of
/// the movement that is not already on the counter lane, whatever the callback did to the action's
/// shape.
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

/// [`held`]'s counterpart on the refund dimension.
///
/// The middle case differs from [`held`]'s: a `NewFrame` action carries a child's envelope but no
/// refund of its own, so the counter is the live object in two of the three cases and only a
/// terminating action displaces it.
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
/// the chain of frame returns between here and the receipt. Neither of those is a quantity a
/// boundary can measure, and over-stating is the safe direction for the lane's one consumer.
#[inline]
fn book_refund(limit: &mut AdditionalLimit, before: i64, after: i64) {
    if before != after {
        limit.record_inspector_refund_adjustment(i128::from(after) - i128::from(before));
    }
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
/// Holds only what the rewrite comparison reads — the gas is taken as [`held`] when the reading is
/// made and never needs the action again. That matters because this reading is taken twice per
/// opcode: copying the action itself would carry an output buffer and a frame input's boxed inputs
/// across every one, while the shape a running frame is almost always in costs a discriminant.
///
/// The output buffer is held rather than reduced to its identity because [`same_buffer`] compares
/// by address, and only an owner keeps that address from being reused underneath it.
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
    /// Gas is excluded, as it is at every other boundary: it travels on the lanes
    /// [`book_pending_action`] routes it to, and counting it here would report one rewrite twice.
    /// Every shape change counts — installing, removing or swapping an action rewrites what the
    /// EVM does next as thoroughly as it is possible to.
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
/// number now lives and so what decides when it can still be settled:
///
/// - [`ActionLane::Result`] is staged for the frame's settlement point, like an edit at the frame's
///   last callback — whether it moves anything depends on the classification the caller ends up
///   seeing, which no callback here knows;
/// - [`ActionLane::Envelope`] is staged for the frame-start callback of the child it will build;
/// - [`ActionLane::Counter`] is booked on the spot: with no action left the frame carries on
///   spending what it holds, which is what a counter edit does. This is the algebra's third case
///   rather than a shape the API offers — `reset_action` leaves the action in place, so emptying
///   the slot means writing `None` and desynchronising the two.
#[inline]
fn book_pending_action(limit: &mut AdditionalLimit, change: ActionChange) {
    if change.gas != 0 {
        match change.lane {
            ActionLane::Result => limit.stage_inspector_action_result_adjustment(change.gas),
            ActionLane::Envelope => limit.stage_inspector_action_env_adjustment(change.gas),
            ActionLane::Counter => limit.record_inspector_action_counter_adjustment(change.gas),
        }
    }
    book_intervention(limit, change.rewritten);
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
/// `intercepted` is true when the callback returned a synthetic outcome: the frame is skipped and
/// the EVM never reads the inputs it edited, so that edit moves nothing on this lane. The staged
/// amount is booked either way, and the asymmetry is not an oversight — it was written by a
/// *different* callback into the action the caller's `CALL` / `CREATE` opcode had already
/// produced, so the caller's debit is behind it and a later decision to answer the frame cannot
/// un-make that.
///
/// An interception stages a baseline rather than booking a difference because it has no object on
/// the other side: the frame is never built, and what the caller reclaims from is a result the
/// inspector wrote from nothing. What the transaction funded is the envelope on the way in, and
/// only the frame init that asked can take that difference.
#[inline]
fn book_env_adjustment(
    limit: &mut AdditionalLimit,
    before: Option<u64>,
    after: Option<u64>,
    intercepted: bool,
) {
    let staged = limit.take_inspector_action_env_adjustment();
    let callback = match (intercepted, before, after) {
        (false, Some(before), Some(after)) => i128::from(after) - i128::from(before),
        _ => 0,
    };
    if let (true, Some(before)) = (intercepted, before) {
        limit.stage_inspector_interception_envelope(before);
    }
    // Booked separately rather than summed first: the two were written in different callbacks, so
    // an envelope raised in one and lowered back in the other is two edits, not none. The staged
    // half already counted its own traffic where it was measured, so only its movement lands here.
    if staged != 0 {
        limit.record_staged_inspector_env_movement(staged);
    }
    if callback != 0 {
        limit.record_inspector_env_adjustment(callback);
    }
}

/// Books one rewrite that changes what the execution *did* rather than what it cost.
///
/// Every caller answers the same question about the argument it was handed: did it come back
/// describing something other than what the EVM was about to do? Two things are deliberately not
/// part of it. **Gas**, because it is booked on the ledger's own lanes and counting it here would
/// report one rewrite twice. And **anything neither the argument nor a constant-time reading off
/// it describes** — the contents of the interpreter's stack and memory, and the journal — because
/// telling whether those came back changed needs a snapshot of unbounded state that no per-opcode
/// boundary can take. Their sizes are constant-time readings and are covered.
#[inline]
fn book_intervention(limit: &mut AdditionalLimit, changed: bool) {
    if changed {
        limit.record_inspector_intervention();
    }
}

/// An `O(1)` identity for a byte buffer: where it starts and how long it is.
///
/// The same comparison [`same_buffer`] makes, stored. Neither reads a byte — a buffer's contents
/// at an unchanged address and length are the class with no lane — but replacing the buffer is the
/// only way to change an immutable one, and that is what this catches.
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
/// **Every `O(1)` reading of the interpreter's working set is in this snapshot** — not a list of
/// the readings someone thought of, but the readings themselves, enumerated field by field against
/// revm's `Interpreter` and the traits each field is reachable through.
///
/// Stated over readings rather than over fields because that is the shape of what a boundary can
/// do: an inspector reaches the whole interpreter, and a snapshot taken twice per opcode can only
/// compare what it can read back in constant time. So the line is drawn at the cost of the
/// reading, and everything on the cheap side of it is taken. An enumeration is only as complete as
/// whoever wrote it, and the four-reading version this replaced left out `bytecode` — which let an
/// inspector step the program counter past an instruction, deleting it from the frame, with every
/// lane reading zero.
///
/// # What is here, by the field it is read from
///
/// - `bytecode` — the program counter, the code's identity, and revm's `continue_execution` flag,
///   which the inspected loop breaks on and which is a separate object from the pending action.
/// - `stack` — its length.
/// - `return_data` — the buffer's identity, which `RETURNDATASIZE` and `RETURNDATACOPY` read.
/// - `memory` — its size, and the offset of the frame's window into the shared buffer.
/// - `gas` — the memory memo's two halves. The budget half moves on the gas lanes instead.
/// - `input` — the four addresses and values a frame's identity is made of, and its calldata's
///   identity. `target_address` is what every storage instruction resolves against.
/// - `runtime_flag` — the static flag and the spec id.
///
/// `extend` is the one field with no reading, by construction: `InterpreterTypes::Extend` carries
/// no trait bound, so a shim generic over the interpreter has nothing to call on it.
///
/// The *contents* of the stack, memory, return buffer, calldata and code are deliberately absent:
/// walking unbounded state is the one thing a per-opcode boundary cannot do.
///
/// The pair that made the snapshot necessary is the memory and its memo, moved together. Raising
/// the memo alone desynchronises the two and the EVM reads out of bounds; growing the memory alone
/// is charged twice. Moving both leaves every interpreter invariant intact, having paid nothing,
/// and makes every later expansion inside the new bound free — which no gas lane can see, because
/// what it changes is what the EVM charges afterwards.
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
    /// snapshot: written that way it materialises two hundred bytes onto the stack for the length
    /// of one `==`, twice per opcode, and the optimiser does not reliably take them away again.
    ///
    /// Every reading in [`of`](Self::of) is compared here and neither list may be shortened
    /// without the other. The unit tests below hold them together: a reading in the snapshot and
    /// missing here is one the shim takes and never compares.
    ///
    /// `inline` rather than `inline(always)` on purpose — forced into all four callbacks it is
    /// slower beside the real tracer the inspected path carries.
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
/// The result is compared on its own, by [`result_rewritten`]; this is the rest, and it is not
/// bookkeeping. `memory_offset` is where the caller copies the callee's output to, so moving it
/// feeds the caller a word the callee never wrote. `charged_new_account_state_gas` tells the
/// caller whether to refund an EIP-8037 upfront charge, and the two precompile fields decide which
/// logs an inspector is shown next.
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
/// Rewriting it reports a contract at an address holding no code while the deployed code stays
/// where it was — a split no classification expresses and no gas lane sees.
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
/// By address and length, not content. `Bytes` is immutable, so a callback can only change an
/// output by putting a different buffer there, and the caller holds its snapshot across the
/// comparison — which keeps the original alive, so its address cannot be reused underneath.
#[inline]
fn same_buffer(before: &Bytes, after: &Bytes) -> bool {
    before.as_ptr() == after.as_ptr() && before.len() == after.len()
}

/// Whether a callback rewrote what a finished frame *did*: the classification its caller will see,
/// or the output it will read.
///
/// The classification carries the most — whether the caller sees a success, whether the frame's
/// state is committed, and whether its remainder is handed back or destroyed — and none of it
/// moves gas, so none of it leaves a trace in any gas lane.
#[inline]
fn result_rewritten(before: (InstructionResult, &Bytes), after: &InterpreterResult) -> bool {
    before.0 != after.result || !same_buffer(before.1, &after.output)
}

/// Whether a callback edited a call frame's inputs anywhere but in their gas limit.
///
/// Everything else a call input carries — who is called, with what value, under which scheme, with
/// what calldata, in a static context or not — describes what the frame will do.
///
/// Taken as the derived equality rather than field by field, which a call's inputs can afford
/// because every field of `CallInputs` is one of those descriptions: there is no memo among them,
/// so a field upstream adds joins the comparison by itself. That is a claim about upstream's
/// struct and it is pinned as one, in `tests/rex7/gas_surface.rs`, which classifies every field of
/// both input types as semantic or memo and fails if a call's inputs ever grow one of the latter.
#[inline]
fn call_inputs_rewritten(mut before: CallInputs, after: &CallInputs) -> bool {
    before.gas_limit = after.gas_limit;
    before != *after
}

/// Whether a callback edited a creation's inputs anywhere but in their gas limit.
///
/// Compared field by field, which the derived equality cannot stand in for here: `CreateInputs`
/// carries two `OnceCell` memos, of the address the creation will occupy and of the init code's
/// hash, and both are filled on demand through a shared reference. Filling one is a derived value
/// being computed, not an input being changed — the frame the EVM builds afterwards is built from
/// the same six numbers either way — and it is what `created_address` does, which every tracer
/// that records a deployment calls. Comparing them would report the most ordinary thing an
/// observation-only tracer does as a rewrite.
///
/// What is compared is the whole of what the frame is built from: who creates, under which scheme,
/// with what value, from what init code, and out of what state-gas pool. The gas limit is left out
/// for the reason a call's is — it travels on the envelope lane, and comparing it here would
/// report one edit twice.
///
/// What the exclusion costs is one shape, and it is content-class rather than free: the address
/// memo is derived from a nonce its caller supplies, and revm reads the memo when it builds the
/// frame, so filling it with a nonce other than the one the EVM would have used redirects the
/// deployment. Telling that apart needs the caller's pre-bump nonce and, under `CREATE2`, the
/// keccak of the init code that the memo exists to avoid computing — neither of which a boundary
/// crossed twice per creation can take. It joins the readings a boundary cannot make at all, and
/// rests on the declaration a block's admission rests on.
#[inline]
fn create_inputs_rewritten(before: &CreateInputs, after: &CreateInputs) -> bool {
    before.caller() != after.caller() ||
        before.scheme() != after.scheme() ||
        before.value() != after.value() ||
        before.init_code() != after.init_code() ||
        before.reservoir() != after.reservoir()
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
            create_inputs_rewritten(&before, after)
        }
        (FrameInput::Empty, FrameInput::Empty) => false,
        _ => true,
    }
}

/// What an inspector can ask `MegaETH` about the frame result it is holding.
///
/// One question: is this a result a frame *ran* to produce, or one frame init produced without
/// ever building a frame? The two arrive at the same callback holding the same type, and the
/// difference decides what a classification rewrite does — a running frame's journal decision is
/// still outstanding and follows the rewrite, an init-produced result's was taken before the
/// callback existed and is refused.
///
/// A tool that only observes never needs this. One that rewrites classifications does, because the
/// only other way to find out is to have its transaction refused.
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

/// Which of the three things a frame's result says — the granularity the refusals are stated over.
///
/// A result's gas and its returned output move freely; the lanes measure those. What cannot move
/// is which of these three the caller is handed, because that is what the journal decision
/// answers.
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

/// One shape of rewrite the shim refuses.
///
/// Both are detection only: nothing here compensates the journal. The classification is restored,
/// the ledger counts the refusal, and the error slot carries the reason so the transaction fails
/// rather than producing a receipt built on the rewrite. What differs is how loud a debug build
/// is, which is [`Self::note_in_debug`].
///
/// Both are gated to REX7+: on a frozen spec a rewrite reaches no accounting lane it can make
/// unsound, and those specs' behaviour is closed.
#[derive(Clone, Copy, Debug)]
enum Forbidden {
    /// A result frame init produced, moved across the success / revert / halt boundary.
    ///
    /// Every other classification rewrite is supported because REX7 withholds the journal decision
    /// until the result is final, so a frame rewritten into a revert has its state rolled back
    /// with it. A result out of frame *init* has no such window and cannot be given one from
    /// here: upstream decides inside `make_call_frame` — an empty-code call commits its
    /// transfer and returns `Stop`, a failing precompile reverts and returns its own failure —
    /// and `MegaETH`'s interceptors decide before they return, the `KeylessDeploy` one by
    /// merging a whole sandbox's state. Honouring a rewrite would hand the caller an answer
    /// the state behind it contradicts.
    ///
    /// A result an inspector answered the frame with itself is deliberately outside the refusal:
    /// nothing in the EVM decided anything for it, so its classification is the inspector's to
    /// state. What separates the two is which callback site in `inspect_frame_init` ran, which is
    /// where the window is opened.
    FrameInitRewrite,
    /// A non-successful contract creation turned into a successful one.
    ///
    /// Forbidden rather than supported because there is no state behind it: by the time
    /// `create_end` runs, revm has reverted the frame's checkpoint and declined to deposit the
    /// code — the size limit, the `0xEF` prefix rule and the code-deposit charge are all evaluated
    /// before the callback. Honouring it would report a deployment at an address holding no code.
    CreateRevival,
}

impl Forbidden {
    /// The reason the error slot carries, which is also what a test matches on.
    const fn message(self) -> &'static str {
        match self {
            Self::FrameInitRewrite => FORBIDDEN_FRAME_INIT_REWRITE,
            Self::CreateRevival => FORBIDDEN_CREATE_REVIVAL,
        }
    }

    /// Whether this rewrite is the one that happened.
    #[inline]
    fn applies<DB: Database, ExtEnvs: ExternalEnvTypes>(
        self,
        context: &MegaContext<DB, ExtEnvs>,
        before: InstructionResult,
        after: InstructionResult,
    ) -> bool {
        match self {
            Self::FrameInitRewrite => {
                ResultClass::of(before) != ResultClass::of(after) &&
                    context.additional_limit.borrow().is_settling_frame_init_result()
            }
            Self::CreateRevival => !before.is_ok() && after.is_ok(),
        }
    }

    /// What a debug build does about it, once the refusal has been carried out.
    ///
    /// A frame-init rewrite is the most ordinary rewrite a tool makes — failing a call — landing
    /// on the one frame kind it cannot be applied to, so a corpus should be able to report it
    /// rather than die on it; all that is asserted is that the refusal did restore the
    /// classification. A revived creation has no reading behind it at all, so a corpus that
    /// produces one should stop.
    #[inline]
    fn note_in_debug(self, before: InstructionResult, after: InstructionResult) {
        match self {
            Self::FrameInitRewrite => debug_assert_eq!(
                ResultClass::of(after),
                ResultClass::of(before),
                "{}: the refusal must leave the caller holding the classification the EVM \
                 produced",
                self.message(),
            ),
            Self::CreateRevival => debug_assert!(
                false,
                "{}: {before:?} was rewritten to a success, which no journal entry and no \
                 deposited code stands behind",
                self.message(),
            ),
        }
    }
}

/// Restores the classification a forbidden rewrite moved, and fails the transaction over it.
#[inline]
fn reject_forbidden_rewrite<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &mut MegaContext<DB, ExtEnvs>,
    what: Forbidden,
    before: InstructionResult,
    result: &mut InterpreterResult,
) {
    if !context.spec.is_enabled(MegaSpecId::REX7) || !what.applies(context, before, result.result) {
        return;
    }
    result.result = before;
    context.additional_limit.borrow_mut().record_inspector_rejected_rewrite();
    let slot = context.error();
    if slot.is_ok() {
        *slot = Err(ContextError::Custom(String::from(what.message())));
    }
    what.note_in_debug(before, result.result);
}

/// What the shim reads off a live interpreter on the way into a callback, and settles on the way
/// out.
///
/// The four callbacks handed a live interpreter run the same measurement, written once here:
/// [`enter`](Self::enter) takes the way-in readings, the user's inspector runs, and
/// [`leave`](Self::leave) takes them again and books the differences.
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

        let mut limit = context.additional_limit.borrow_mut();
        book_intervention(&mut limit, moved);
        book_pending_action(&mut limit, change);
        book_refund(&mut limit, self.refund, refund);
        if gas != self.gas {
            limit.record_inspector_gas_adjustment::<IN_OPEN_SEGMENT>(
                &mut interp.gas,
                self.gas,
                lane.counter_reaches_envelope(),
            );
        }
    }
}

/// The measuring bodies of the four live-interpreter callbacks, kept out of line.
///
/// Each callback is a branch on the declaration and, when taken, the measurement. Only the branch
/// belongs in revm's instruction loop, and `inline(never)` is what puts it there alone: inlined,
/// the measurement's two hundred bytes of readings are laid down inside the loop for a declared
/// observer that never executes them, and the loop pays for them in registers and instruction
/// cache regardless.
///
/// The trade was measured both ways. Beside the real tracer the inspected path carries in
/// production, outlining makes the undeclared path faster too; beside an empty inspector it is
/// slower, because there the call is the whole of the work — and an empty inspector is an
/// instrument for isolating this shim's cost, not a workload.
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
fn book_frame_entry(
    limit: &mut AdditionalLimit,
    before: Option<u64>,
    after: Option<u64>,
    intercepted_refund: Option<i64>,
    rewritten: bool,
) {
    let intercepted = intercepted_refund.is_some();
    book_env_adjustment(limit, before, after, intercepted);
    if let Some(refund) = intercepted_refund {
        // A synthetic outcome has no "before" to difference against, so the whole of its refund is
        // the inspector's; the baseline is zero because a frame that never ran refunded nothing.
        book_refund(limit, 0, refund);
    }
    book_intervention(limit, intercepted || rewritten);
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
        {
            let mut limit = context.additional_limit.borrow_mut();
            book_refund(&mut limit, self.refund, result.gas.refunded());
            book_intervention(&mut limit, result_rewritten((self.result, &self.output), result));
            book_intervention(&mut limit, metadata != self.metadata);
        }
        reject_forbidden_rewrite(context, Forbidden::FrameInitRewrite, self.result, result);
        if is_create {
            reject_forbidden_rewrite(context, Forbidden::CreateRevival, self.result, result);
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
            &mut context.additional_limit.borrow_mut(),
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
            &mut context.additional_limit.borrow_mut(),
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
            &mut context.additional_limit.borrow_mut(),
            Some(before.gas_limit()),
            Some(inputs.gas_limit()),
            outcome.as_ref().map(|outcome| outcome.result.gas.refunded()),
            create_inputs_rewritten(&before, inputs),
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
            CreateScheme, InputsImpl, InterpreterAction, SharedMemory,
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

    /// One case: the name of a field, and a rewrite that moves it.
    type OutcomeCase = (&'static str, fn(&mut CallOutcome));

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
    const OUTCOME_CASES: [OutcomeCase; 4] = [
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

    /// One case: the name of a field a creation's inputs are built from, and a rewrite that moves
    /// it.
    type CreateCase = (&'static str, fn(&mut CreateInputs));

    /// The creation every case below rewrites one field of.
    fn create_inputs() -> CreateInputs {
        CreateInputs::new(
            Address::ZERO,
            CreateScheme::Create,
            U256::ZERO,
            Bytes::from_static(&[0x60, 0x00]),
            1_000_000,
            0,
        )
    }

    /// One rewrite per field the comparison is written over, each moving the field it is named
    /// for.
    ///
    /// Every one is something an inspector can do to a creation through the setters upstream
    /// gives it, and each changes what the frame does: who is recorded as the creator, which
    /// address the contract lands at, what it is funded with, what code runs, and what state-gas
    /// pool it draws from.
    const CREATE_CASES: [CreateCase; 5] = [
        ("caller", |inputs| inputs.set_call(OTHER)),
        ("scheme", |inputs| {
            inputs.set_scheme(CreateScheme::Create2 { salt: U256::from(0x5A17) });
        }),
        ("value", |inputs| inputs.set_value(U256::from(1))),
        ("init_code", |inputs| inputs.set_init_code(Bytes::from_static(&[0x00]))),
        ("reservoir", |inputs| inputs.set_reservoir(1)),
    ];

    /// ★ Every field a creation's frame is built from is one an edit to is booked.
    ///
    /// `CreateInputs` is compared field by field rather than by the derived equality a call's
    /// inputs use, which trades a comparison that grows by itself for one someone has to keep
    /// complete — so each field gets a case that moves it and asserts the shim sees it move. The
    /// other half of the trade, that the list is still upstream's whole field set, is not a
    /// question this module can ask; `tests/rex7/gas_surface.rs` asks it against the struct's own
    /// `Debug` rendering.
    #[test]
    fn test_every_semantic_field_of_a_creation_is_compared() {
        assert!(
            !create_inputs_rewritten(&create_inputs(), &create_inputs()),
            "two identical creations must not read as a rewrite",
        );

        let mut names: Vec<&str> = CREATE_CASES.iter().map(|(name, _)| *name).collect();
        let declared = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), declared, "no field may be listed twice");

        for (name, rewrite) in CREATE_CASES {
            let before = create_inputs();
            let mut after = create_inputs();
            rewrite(&mut after);
            assert!(
                create_inputs_rewritten(&before, &after),
                "a rewritten {name} must be visible to the shim",
            );
        }
    }

    /// ★ Filling a creation's memo cells is not a rewrite, in either direction.
    ///
    /// This is the whole reason the comparison is written out: `created_address` and
    /// `init_code_hash` fill an `OnceCell` through a shared reference, so the object a callback
    /// was handed comes back structurally different having had a derived value computed off it.
    /// Every tracer that records a deployment calls the first of those, so the derived equality
    /// booked an intervention for the most ordinary thing an observation-only inspector does.
    ///
    /// The other direction is the setters', which clear the cells: an inspector that writes back
    /// the init code a creation already had has changed nothing and emptied both memos.
    #[test]
    fn test_filling_or_clearing_a_creations_memo_is_not_a_rewrite() {
        let before = create_inputs();
        let after = create_inputs();
        after.created_address(0);
        after.init_code_hash();
        assert!(
            !create_inputs_rewritten(&before, &after),
            "computing the created address and the init code hash changes no input",
        );
        assert!(
            !create_inputs_rewritten(&after, &before),
            "and neither does a setter clearing the memos back down",
        );

        let mut moved = create_inputs();
        moved.set_value(U256::from(1));
        moved.created_address(0);
        assert!(
            create_inputs_rewritten(&before, &moved),
            "a memo filled beside a real edit must not hide the edit",
        );
    }

    /// ★ A creation's gas limit is not part of the comparison.
    ///
    /// It travels on the envelope lane, which books the amount rather than the fact, so counting
    /// it here would report one edit twice. A call's inputs are excluded from their own comparison
    /// the same way.
    #[test]
    fn test_a_creations_gas_limit_is_left_to_the_envelope_lane() {
        let before = create_inputs();
        let mut after = create_inputs();
        after.set_gas_limit(before.gas_limit() + 1);
        assert!(
            !create_inputs_rewritten(&before, &after),
            "the gas limit is booked as an amount, not as an intervention",
        );
    }
}
