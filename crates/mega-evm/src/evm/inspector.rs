//! What an inspector may do to a Satin transaction, and the one thing it may not.
//!
//! An inspector is not a passive observer. A callback holding a live interpreter can write to its
//! gas and its pending action, a callback holding a frame's inputs can change them, and every
//! `*_end` callback can rewrite a result's classification, gas and output. Satin keeps no ledger
//! beside revm's `Gas` (compute is the regular gas spent), so an inspector that changes the gas
//! changes what the transaction owes, and a rewriting inspector gets exactly what it asked for.
//! Rewriting is a tool feature, supported and unmeasured.
//!
//! Two things bound it:
//!
//! - **The admission gate.** [`TrustedObserver`] is a declaration, made in source about one
//!   inspector type, that none of its callbacks writes anything back. A [`MegaEvm`](crate::MegaEvm)
//!   built with [`with_trusted_inspector`](crate::MegaEvm::with_trusted_inspector) carries the
//!   declaration, and [`has_rewriting_inspector`](crate::MegaEvm::has_rewriting_inspector) is what
//!   block execution refuses a transaction on: a rewriting inspector has no route to a block.
//!   [`DeclaredObserver`] carries the declaration for a tracer whose type cannot, and in debug
//!   builds checks it at every callback.
//! - **The refusal.** A failed contract creation rewritten into a successful one is refused: the
//!   transaction fails with [`FORBIDDEN_CREATE_REVIVAL`] as an `EVMError::Custom`. By the time
//!   `create_end` runs, revm has reverted the frame and deposited no code, so the rewrite would
//!   push an address for code that does not exist and merge the frame's state gas for state that
//!   was rolled back. Every other rewrite is the tool's business.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{format, string::String};

use revm::{
    context::{ContextError, ContextTr},
    handler::FrameResult,
    inspector::{handler::frame_end, NoOpInspector},
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, InstructionResult,
        Interpreter, InterpreterTypes,
    },
    primitives::{Address, Log, U256},
    Inspector,
};

/// The message of the `EVMError::Custom` a refused creation revival fails the transaction with.
///
/// Public so a tool driving rewriting inspectors can tell the refusal from an execution failure.
pub const FORBIDDEN_CREATE_REVIVAL: &str =
    "inspector rewrote a failed contract creation into a successful one";

/// A declaration, made in source about one inspector type, that none of its callbacks writes
/// anything back to the EVM: nothing to an interpreter's gas or pending action, nothing to a
/// frame's inputs, nothing to a result's classification, gas or output, and no frame answered
/// itself. It may read anything and write its own state.
///
/// The declaration is what block execution admits an inspected transaction on.
///
/// - Implement it for one concrete type at a time, never as a blanket implementation, so each
///   declaration is a line someone wrote about a type they had read.
/// - Do not implement it for an inspector that answers a frame, edits inputs or rewrites a result,
///   however rarely.
/// - A foreign inspector type (a `revm-inspectors` tracer) is wrapped in [`DeclaredObserver`],
///   which is local here and carries the declaration.
/// - The test utilities' inspectors are not declared: they are not part of the gate.
pub trait TrustedObserver {}

/// The inspector an EVM runs with when none was given observes nothing.
impl TrustedObserver for NoOpInspector {}

/// A declared observer lent by reference stays declared: `&mut T` is declared exactly when `T`
/// is.
impl<T: TrustedObserver + ?Sized> TrustedObserver for &mut T {}

/// Carries a [`TrustedObserver`] declaration for an inspector whose type cannot carry one.
///
/// It forwards every callback to the inspector inside and adds nothing. `DeclaredObserver(tracer)`
/// says "this tracer writes nothing back", moved from a type definition to the line that wraps
/// the value. Wrapping an inspector that writes is a false declaration, and a debug build panics
/// at the callback that breaks it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeclaredObserver<I>(pub I);

impl<I> DeclaredObserver<I> {
    /// Declares `inspector` a read-only observer.
    pub const fn new(inspector: I) -> Self {
        Self(inspector)
    }

    /// The inspector inside.
    pub fn into_inner(self) -> I {
        self.0
    }
}

impl<I> TrustedObserver for DeclaredObserver<I> {}

impl<I, CTX, INTR> Inspector<CTX, INTR> for DeclaredObserver<I>
where
    I: Inspector<CTX, INTR>,
    INTR: InterpreterTypes,
{
    #[inline]
    fn initialize_interp(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let before = gas_snapshot(interp);
        self.0.initialize_interp(interp, context);
        assert_gas_unwritten(before, interp, "initialize_interp");
    }

    #[inline]
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let before = gas_snapshot(interp);
        self.0.step(interp, context);
        assert_gas_unwritten(before, interp, "step");
    }

    #[inline]
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let before = gas_snapshot(interp);
        self.0.step_end(interp, context);
        assert_gas_unwritten(before, interp, "step_end");
    }

    #[inline]
    fn log(&mut self, context: &mut CTX, log: Log) {
        self.0.log(context, log);
    }

    #[inline]
    fn log_full(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX, log: Log) {
        let before = gas_snapshot(interp);
        self.0.log_full(interp, context, log);
        assert_gas_unwritten(before, interp, "log_full");
    }

    #[inline]
    fn frame_start(
        &mut self,
        context: &mut CTX,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        let answer = self.0.frame_start(context, frame_input);
        debug_assert!(answer.is_none(), "a declared observer answered a frame in `frame_start`");
        answer
    }

    #[inline]
    fn frame_end(
        &mut self,
        context: &mut CTX,
        frame_input: &FrameInput,
        frame_result: &mut FrameResult,
    ) {
        let before = result_snapshot(frame_result);
        self.0.frame_end(context, frame_input, frame_result);
        assert_result_unwritten(before, frame_result, "frame_end");
    }

    #[inline]
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let before = cfg!(debug_assertions).then(|| inputs.clone());
        let answer = self.0.call(context, inputs);
        debug_assert!(answer.is_none(), "a declared observer answered a call");
        if let Some(before) = before {
            debug_assert!(&before == inputs, "a declared observer rewrote a call's inputs");
        }
        answer
    }

    #[inline]
    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let before = cfg!(debug_assertions).then(|| outcome.clone());
        self.0.call_end(context, inputs, outcome);
        if let Some(before) = before {
            debug_assert!(&before == outcome, "a declared observer rewrote a call result");
        }
    }

    #[inline]
    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let before = cfg!(debug_assertions).then(|| inputs.clone());
        let answer = self.0.create(context, inputs);
        debug_assert!(answer.is_none(), "a declared observer answered a creation");
        if let Some(before) = before {
            debug_assert!(&before == inputs, "a declared observer rewrote a creation's inputs");
        }
        answer
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        let before = cfg!(debug_assertions).then(|| outcome.clone());
        self.0.create_end(context, inputs, outcome);
        if let Some(before) = before {
            debug_assert!(&before == outcome, "a declared observer rewrote a creation result");
        }
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.0.selfdestruct(contract, target, value);
    }
}

/// An interpreter's gas and pending action, for the debug-build proof. `None` in release builds.
type GasSnapshot = Option<(revm::interpreter::Gas, bool)>;

#[inline]
fn gas_snapshot<INTR: InterpreterTypes>(interp: &Interpreter<INTR>) -> GasSnapshot {
    use revm::interpreter::interpreter_types::LoopControl;
    cfg!(debug_assertions).then(|| (interp.gas, interp.bytecode.is_end()))
}

#[inline]
fn assert_gas_unwritten<INTR: InterpreterTypes>(
    before: GasSnapshot,
    interp: &Interpreter<INTR>,
    callback: &str,
) {
    if let Some(before) = before {
        debug_assert_eq!(
            before,
            gas_snapshot(interp).expect("debug build"),
            "a declared observer wrote to the interpreter in `{callback}`"
        );
    }
}

/// A frame result's classification, gas and output, and a creation's address, for the
/// debug-build proof.
type ResultSnapshot = Option<(revm::interpreter::InterpreterResult, Option<Address>)>;

#[inline]
fn result_snapshot(result: &FrameResult) -> ResultSnapshot {
    cfg!(debug_assertions).then(|| {
        let address = match result {
            FrameResult::Call(_) => None,
            FrameResult::Create(outcome) => outcome.address,
        };
        (result.interpreter_result().clone(), address)
    })
}

#[inline]
fn assert_result_unwritten(before: ResultSnapshot, result: &FrameResult, callback: &str) {
    if let Some(before) = before {
        debug_assert!(
            before == result_snapshot(result).expect("debug build"),
            "a declared observer rewrote a frame result in `{callback}`"
        );
    }
}

/// Hands a frame's end to the inspector, then applies the one refusal: a creation that failed,
/// rewritten into a success, is put back and fails the transaction with
/// [`FORBIDDEN_CREATE_REVIVAL`].
///
/// Every place a frame result reaches the inspector goes through here, so the refusal covers a
/// result a frame ran to produce and one answered without running. A frame the inspector answered
/// with a success itself is no revival: nothing failed.
#[inline]
pub(crate) fn frame_end_checked<CTX, INTR, INSP>(
    context: &mut CTX,
    inspector: &mut INSP,
    frame_input: &FrameInput,
    frame_result: &mut FrameResult,
) where
    CTX: ContextTr,
    INTR: InterpreterTypes,
    INSP: Inspector<CTX, INTR, FrameInput, FrameResult>,
{
    let before = frame_result.instruction_result();
    frame_end(context, inspector, frame_input, frame_result);
    refuse_create_revival(context, before, frame_result);
}

/// Puts a revived creation back to `before` and records the refusal as the context's error.
fn refuse_create_revival<CTX: ContextTr>(
    context: &mut CTX,
    before: InstructionResult,
    result: &mut FrameResult,
) {
    let FrameResult::Create(outcome) = result else { return };
    if before.is_ok() || !outcome.result.result.is_ok() {
        return;
    }
    outcome.result.result = before;
    if context.error().is_ok() {
        let message: String = format!("{FORBIDDEN_CREATE_REVIVAL}: {before:?}");
        *context.error() = Err(ContextError::Custom(message));
    }
}
