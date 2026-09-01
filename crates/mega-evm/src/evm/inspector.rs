//! The measurement shim every inspector handed to `MegaETH` is wrapped in.
//!
//! # Why a shim
//!
//! An inspector is not a passive observer. Every callback that receives a live interpreter can
//! write to its gas counter, and every callback that receives a frame's inputs can change the gas
//! limit the frame is about to be built with. `MegaETH` meters compute gas by watching those exact
//! counters, and derives what a transaction destroyed from the envelope it spent, so an
//! unmeasured edit shows up as the EVM having done less work than it did, or as a transaction
//! having spent less gas than it did.
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
//! - Frame-envelope edits go to [`AdditionalLimit::record_inspector_env_adjustment`].
//! - One rewrite shape is refused outright: see [`MeasuredInspector::create_end`].
//!
//! Nothing here changes what the inspector is allowed to do to the EVM, and nothing here runs on
//! the uninspected path — revm's plain interpreter loop never calls an inspector at all.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::string::String;

use alloy_evm::Database;
use alloy_primitives::{Address, Log, U256};
use revm::{
    context::{ContextError, ContextTr},
    handler::FrameResult,
    interpreter::{
        interpreter_types::LoopControl, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        FrameInput, InstructionResult, Interpreter, InterpreterAction, InterpreterResult,
        InterpreterTypes,
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

/// Whether an edit to this interpreter's gas counter can still reach the transaction's envelope.
///
/// It cannot exactly when the interpreter is holding a `Return` action. revm's inspected loop runs
/// the terminating instruction first and the callback after it, and that instruction has already
/// copied the counter into the action it set — the action is what becomes the frame's result and
/// what the caller reclaims from. Whatever the callback writes into the counter afterwards is
/// written into an object nobody will read again.
///
/// The two neighbouring shapes are live and must stay booked. With no action pending the frame
/// carries straight on; with a `NewFrame` action pending it suspends into a child and then resumes
/// on this very counter. "The loop is about to break" is therefore not the question — revm breaks
/// out of the instruction loop in both the suspending and the terminating case — and a rule phrased
/// that way would stop booking edits that really do move a frame's budget.
///
/// Read *after* the callback returns, because the question is about the counter the callback left
/// behind: an inspector that sets or clears an action has changed what the EVM does next, and the
/// answer has to follow it.
///
/// # Why this decides booking and not measurement
///
/// Only the ledger is gated on it. `MegaETH`'s own tail settlement measures a frame's work as a
/// drop in this same counter and does read it after the action is set, so the settlement baseline
/// has to shift for a dead-window edit exactly as it does for a live one — otherwise gas the
/// inspector wrote in would read as work the frame performed, which is the opposite error.
///
/// # What it does not cover
///
/// The pending action itself. A callback holding a live interpreter can reach the action through
/// `LoopControl` and edit the gas inside it, which does move the envelope. Measuring that at the
/// callback boundary would be unsound rather than merely incomplete: whether such an edit moves
/// anything depends on the frame's *final* classification, which no callback here knows — a
/// returning frame hands its remainder back and a halting one does not — so it belongs to the
/// frame's settlement point, which books edits made at the one callback it can see
/// ([`InspectorLedger::result`](crate::InspectorLedger::result)). Extending that lane to cover
/// this one is a per-frame snapshot this shim deliberately does not carry.
#[inline]
fn counter_reaches_envelope<INTR: InterpreterTypes>(interp: &mut Interpreter<INTR>) -> bool {
    !matches!(interp.bytecode.action(), Some(InterpreterAction::Return(_)))
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

/// Books what a callback did to a frame's envelope, if the edited inputs will actually reach a
/// frame.
///
/// `intercepted` is true when the callback returned a synthetic outcome: the frame is skipped
/// entirely and the EVM never reads the inputs it edited, so the edit by itself moves nothing. Gas
/// the inspector then puts into that synthetic outcome travels through the result lane, which this
/// lane deliberately does not cover — see [`InspectorLedger::env`](crate::InspectorLedger::env).
#[inline]
fn book_env_adjustment<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &MegaContext<DB, ExtEnvs>,
    before: Option<u64>,
    after: Option<u64>,
    intercepted: bool,
) {
    if intercepted {
        return;
    }
    let (Some(before), Some(after)) = (before, after) else {
        return;
    };
    if before == after {
        return;
    }
    context
        .additional_limit
        .borrow_mut()
        .record_inspector_env_adjustment(i128::from(after) - i128::from(before));
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
        let before = interp.gas.remaining();
        self.inner.initialize_interp(interp, context);
        let reaches_envelope = counter_reaches_envelope(interp);
        context.additional_limit.borrow_mut().record_inspector_gas_adjustment::<false>(
            &mut interp.gas,
            before,
            reaches_envelope,
        );
    }

    #[inline]
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut MegaContext<DB, ExtEnvs>) {
        let before = interp.gas.remaining();
        self.inner.step(interp, context);
        let reaches_envelope = counter_reaches_envelope(interp);
        context.additional_limit.borrow_mut().record_inspector_gas_adjustment::<true>(
            &mut interp.gas,
            before,
            reaches_envelope,
        );
    }

    /// The callback that sits in the dead window: revm runs it after the instruction that set the
    /// frame's action, so on a terminating opcode the counter it hands out has already been copied
    /// into the result the caller will be given — see [`counter_reaches_envelope`].
    #[inline]
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut MegaContext<DB, ExtEnvs>) {
        let before = interp.gas.remaining();
        self.inner.step_end(interp, context);
        let reaches_envelope = counter_reaches_envelope(interp);
        context.additional_limit.borrow_mut().record_inspector_gas_adjustment::<true>(
            &mut interp.gas,
            before,
            reaches_envelope,
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
        let before = interpreter.gas.remaining();
        self.inner.log_full(interpreter, context, log);
        let reaches_envelope = counter_reaches_envelope(interpreter);
        context.additional_limit.borrow_mut().record_inspector_gas_adjustment::<true>(
            &mut interpreter.gas,
            before,
            reaches_envelope,
        );
    }

    #[inline]
    fn frame_start(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        let before = frame_input_gas_limit(frame_input);
        let outcome = self.inner.frame_start(context, frame_input);
        book_env_adjustment(context, before, frame_input_gas_limit(frame_input), outcome.is_some());
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
        self.inner.frame_end(context, frame_input, frame_result);
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
        let before = inputs.gas_limit;
        let outcome = self.inner.call(context, inputs);
        book_env_adjustment(context, Some(before), Some(inputs.gas_limit), outcome.is_some());
        outcome
    }

    /// `CallInputs` is immutable here and the frame's result gas is deliberately not booked — see
    /// [`InspectorLedger::env`](crate::InspectorLedger::env) — so this is a plain forward.
    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.inner.call_end(context, inputs, outcome);
    }

    #[inline]
    fn create(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        let before = inputs.gas_limit();
        let outcome = self.inner.create(context, inputs);
        book_env_adjustment(context, Some(before), Some(inputs.gas_limit()), outcome.is_some());
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
        let before = outcome.result.result;
        self.inner.create_end(context, inputs, outcome);
        reject_forbidden_create_rewrite(context, before, &mut outcome.result);
    }

    /// Everything this callback receives is passed by value, so it cannot change execution state.
    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.inner.selfdestruct(contract, target, value);
    }
}
