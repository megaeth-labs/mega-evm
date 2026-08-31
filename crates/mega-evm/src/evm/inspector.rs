//! The measurement shim every inspector handed to `MegaETH` is wrapped in.
//!
//! # Why a shim
//!
//! An inspector is not a passive observer. Every callback that receives a live interpreter can
//! write to its gas counter, and every callback that receives a frame's inputs can change the gas
//! limit the frame is about to be built with. `MegaETH` meters compute gas by watching those exact
//! counters, and derives what a transaction destroyed from the envelope it spent, so an unmeasured
//! edit shows up as the EVM having done less work than it did, or as a transaction having spent
//! less gas than it did.
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
//! Nothing here changes what an inspector is allowed to do to the EVM, and nothing here runs on
//! the uninspected path — revm's plain interpreter loop never calls an inspector at all.

use alloy_evm::Database;
use alloy_primitives::{Address, Log, U256};
use revm::{
    handler::FrameResult,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Interpreter,
        InterpreterTypes,
    },
    Inspector,
};

use crate::{ExternalEnvTypes, MegaContext};

/// Wraps a user inspector so that what it does to gas accounting can be measured and booked.
///
/// `MegaETH` applies this itself — [`MegaEvm::with_inspector`](crate::MegaEvm::with_inspector) and
/// [`InspectEvm::set_inspector`](revm::InspectEvm::set_inspector) take the user's inspector by
/// value and store it wrapped, and the accessors hand back the unwrapped inspector — so the
/// wrapper is not something a caller opts into or can opt out of.
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

impl<DB, ExtEnvs, INTR, I> Inspector<MegaContext<DB, ExtEnvs>, INTR> for MeasuredInspector<I>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    INTR: InterpreterTypes,
    I: Inspector<MegaContext<DB, ExtEnvs>, INTR>,
{
    #[inline]
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
    ) {
        self.inner.initialize_interp(interp, context);
    }

    #[inline]
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut MegaContext<DB, ExtEnvs>) {
        self.inner.step(interp, context);
    }

    #[inline]
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut MegaContext<DB, ExtEnvs>) {
        self.inner.step_end(interp, context);
    }

    #[inline]
    fn log(&mut self, context: &mut MegaContext<DB, ExtEnvs>, log: Log) {
        self.inner.log(context, log);
    }

    /// Forwards to the wrapped inspector's `log_full`, not to its `log`: the default `log_full`
    /// already falls through to `log`, and short-circuiting here would silently drop an override.
    #[inline]
    fn log_full(
        &mut self,
        interpreter: &mut Interpreter<INTR>,
        context: &mut MegaContext<DB, ExtEnvs>,
        log: Log,
    ) {
        self.inner.log_full(interpreter, context, log);
    }

    #[inline]
    fn frame_start(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        self.inner.frame_start(context, frame_input)
    }

    #[inline]
    fn frame_end(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        frame_input: &FrameInput,
        frame_result: &mut FrameResult,
    ) {
        self.inner.frame_end(context, frame_input, frame_result);
    }

    #[inline]
    fn call(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.inner.call(context, inputs)
    }

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
        self.inner.create(context, inputs)
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.inner.create_end(context, inputs, outcome);
    }

    /// Everything this callback receives is passed by value, so it cannot change execution state.
    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.inner.selfdestruct(contract, target, value);
    }
}
