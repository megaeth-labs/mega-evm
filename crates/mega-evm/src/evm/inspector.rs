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
//!   builds checks it around every callback.
//! - **The refusal.** A failed contract creation rewritten into a successful one is refused: the
//!   transaction fails with [`FORBIDDEN_CREATE_REVIVAL`] as an `EVMError::Custom`. By the time
//!   `create_end` runs, revm has reverted the frame and deposited no code, so the rewrite would
//!   push an address for code that does not exist and merge the frame's state gas for state that
//!   was rolled back. A transaction a limit stopped is the exception: once it is latched, every
//!   frame's result is the stop whatever produced it, so a revived creation is put back and reports
//!   the stop, and the transaction does not fail. A creation stopped by its own frame budget
//!   latches nothing, and its revival is refused. Every other rewrite is the tool's business.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{format, string::String};

use revm::{
    context::{ContextError, ContextTr, JournalTr},
    context_interface::{cfg::StateGasCharge, Host},
    handler::FrameResult,
    inspector::{handler::frame_end, JournalExt, NoOpInspector},
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, InstructionResult,
        Interpreter, InterpreterTypes,
    },
    primitives::{Address, Bytes, Log, U256},
    Database, Inspector,
};

use crate::{synthetic_frame_result, ExternalEnvTypes, MegaContext};

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
/// the value.
///
/// A debug build checks the declaration around every callback and panics at the one that breaks
/// it: the interpreter's gas, pending action, stack depth and memory size, the frame's inputs,
/// the frame result, and the journal's entries and logs must be as they were, and no frame may be
/// answered. Writes the check cannot see (a stack word replaced in place, a context field outside
/// the journal) are still false declarations; the check is a tripwire, not a proof.
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
    CTX: ContextTr<Journal: JournalExt>,
    INTR: InterpreterTypes,
{
    #[inline]
    fn initialize_interp(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let before = snapshot(interp, context);
        self.0.initialize_interp(interp, context);
        assert_unwritten(before, interp, context, "initialize_interp");
    }

    #[inline]
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let before = snapshot(interp, context);
        self.0.step(interp, context);
        assert_unwritten(before, interp, context, "step");
    }

    #[inline]
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let before = snapshot(interp, context);
        self.0.step_end(interp, context);
        assert_unwritten(before, interp, context, "step_end");
    }

    #[inline]
    fn log(&mut self, context: &mut CTX, log: Log) {
        let before = journal_snapshot(context);
        self.0.log(context, log);
        assert_journal_unwritten(before, context, "log");
    }

    #[inline]
    fn log_full(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX, log: Log) {
        let before = snapshot(interp, context);
        self.0.log_full(interp, context, log);
        assert_unwritten(before, interp, context, "log_full");
    }

    #[inline]
    fn frame_start(
        &mut self,
        context: &mut CTX,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        let before =
            cfg!(debug_assertions).then(|| (frame_input.clone(), journal_snapshot(context)));
        let answer = self.0.frame_start(context, frame_input);
        debug_assert!(answer.is_none(), "a declared observer answered a frame in `frame_start`");
        if let Some((input, journal)) = before {
            debug_assert!(&input == frame_input, "a declared observer rewrote a frame's inputs");
            assert_journal_unwritten(journal, context, "frame_start");
        }
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
        let journal = journal_snapshot(context);
        self.0.frame_end(context, frame_input, frame_result);
        assert_result_unwritten(before, frame_result, "frame_end");
        assert_journal_unwritten(journal, context, "frame_end");
    }

    #[inline]
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let before = cfg!(debug_assertions).then(|| inputs.clone());
        let journal = journal_snapshot(context);
        let answer = self.0.call(context, inputs);
        debug_assert!(answer.is_none(), "a declared observer answered a call");
        if let Some(before) = before {
            debug_assert!(&before == inputs, "a declared observer rewrote a call's inputs");
        }
        assert_journal_unwritten(journal, context, "call");
        answer
    }

    #[inline]
    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let before = cfg!(debug_assertions).then(|| outcome.clone());
        let journal = journal_snapshot(context);
        self.0.call_end(context, inputs, outcome);
        if let Some(before) = before {
            debug_assert!(&before == outcome, "a declared observer rewrote a call result");
        }
        assert_journal_unwritten(journal, context, "call_end");
    }

    #[inline]
    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let before = cfg!(debug_assertions).then(|| inputs.clone());
        let journal = journal_snapshot(context);
        let answer = self.0.create(context, inputs);
        debug_assert!(answer.is_none(), "a declared observer answered a creation");
        if let Some(before) = before {
            debug_assert!(&before == inputs, "a declared observer rewrote a creation's inputs");
        }
        assert_journal_unwritten(journal, context, "create");
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
        let journal = journal_snapshot(context);
        self.0.create_end(context, inputs, outcome);
        if let Some(before) = before {
            debug_assert!(&before == outcome, "a declared observer rewrote a creation result");
        }
        assert_journal_unwritten(journal, context, "create_end");
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.0.selfdestruct(contract, target, value);
    }
}

/// The journal's entry and log counts, for the debug-build proof. `None` in release builds.
type JournalSnapshot = Option<(usize, usize)>;

#[inline]
fn journal_snapshot<CTX: ContextTr<Journal: JournalExt>>(context: &CTX) -> JournalSnapshot {
    cfg!(debug_assertions)
        .then(|| (context.journal_ref().journal().len(), context.journal_ref().logs().len()))
}

#[inline]
fn assert_journal_unwritten<CTX: ContextTr<Journal: JournalExt>>(
    before: JournalSnapshot,
    context: &CTX,
    callback: &str,
) {
    if let Some(before) = before {
        debug_assert_eq!(
            before,
            journal_snapshot(context).expect("debug build"),
            "a declared observer wrote to the journal in `{callback}`"
        );
    }
}

/// An interpreter's and the journal's state, for the debug-build proof.
type Snapshot = (GasSnapshot, JournalSnapshot);

#[inline]
fn snapshot<INTR: InterpreterTypes, CTX: ContextTr<Journal: JournalExt>>(
    interp: &Interpreter<INTR>,
    context: &CTX,
) -> Snapshot {
    (gas_snapshot(interp), journal_snapshot(context))
}

#[inline]
fn assert_unwritten<INTR: InterpreterTypes, CTX: ContextTr<Journal: JournalExt>>(
    (gas, journal): Snapshot,
    interp: &Interpreter<INTR>,
    context: &CTX,
    callback: &str,
) {
    assert_gas_unwritten(gas, interp, callback);
    assert_journal_unwritten(journal, context, callback);
}

/// An interpreter's gas, pending action, stack depth and memory size, for the debug-build proof.
/// `None` in release builds.
type GasSnapshot = Option<(revm::interpreter::Gas, bool, usize, usize)>;

#[inline]
fn gas_snapshot<INTR: InterpreterTypes>(interp: &Interpreter<INTR>) -> GasSnapshot {
    use revm::interpreter::interpreter_types::{LoopControl, MemoryTr, StackTr};
    cfg!(debug_assertions)
        .then(|| (interp.gas, interp.bytecode.is_end(), interp.stack.len(), interp.memory.size()))
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

/// Hands a frame's end to the inspector, then settles what its rewrite does not change.
///
/// - **The refusal.** A creation that failed, rewritten into a success, is put back and fails the
///   transaction with [`FORBIDDEN_CREATE_REVIVAL`] — unless the transaction is latched, where the
///   creation is put back and reports the stop, which the latch writes over every result.
/// - **A frame that kept nothing.** A frame the inspector answered in place of running
///   (`answered`), and a frame that failed before the inspector saw it, made no write the journal
///   kept. Whatever the inspector left the result saying, it settles into its caller as such a
///   frame does ([`settle_kept_nothing`]).
///
/// Every place a frame result reaches the inspector goes through here, so both cover a result a
/// frame ran to produce and one answered without running. A frame the inspector answered with a
/// success itself is no revival: nothing failed. The latch is read after the hooks that can set
/// it — the frame's run, detention's classification of its end, the hold on a start's upfront state
/// gas, and the answer before building — have run, so a creation the latch stopped is never
/// refused.
#[inline]
pub(crate) fn frame_end_checked<DB, ExtEnvs, INTR, INSP>(
    context: &mut MegaContext<DB, ExtEnvs>,
    inspector: &mut INSP,
    frame_input: &FrameInput,
    frame_result: &mut FrameResult,
    depth: usize,
    answered: bool,
) where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    INTR: InterpreterTypes,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, INTR, FrameInput, FrameResult>,
{
    let before = frame_result.instruction_result();
    frame_end(context, inspector, frame_input, frame_result);
    refuse_create_revival(context, before, frame_result);
    if answered || !before.is_ok() {
        settle_kept_nothing(context, frame_input, frame_result, depth);
    }
}

/// Settles the result of a frame at `depth` that kept nothing in the journal — one an inspector
/// answered in place of running, or one whose checkpoint was reverted before the inspector saw its
/// result — as such a frame settles, whatever the inspector made of the result.
///
/// The calling opcode's upfront state-gas charge — the account a value call adds, the account a
/// creation adds, or EIP-2780's for the transaction's own frame — is given back: the frame added
/// no account. The result carries the frame's own upfront-charge flags again, so a result that
/// fails gets it back from revm's settlement, as revm's own do; one an inspector answered carries
/// none of them otherwise.
///
/// A result that says success is not one revm's settlement treats as keeping nothing, so it is made
/// to settle like the failure it stands for, save what the inspector chose — the success its caller
/// sees, the output, the regular gas:
///
/// - its state and history charges are rolled back and its refunds dropped, as a failure's are,
///   before its caller merges them;
/// - the upfront charge comes back through it: the result holds it as state its frame gave back, as
///   a frame that restored a slot its caller filled does, and its caller's merge nets it out;
/// - for the transaction's own frame, the history of the write record charged before execution for
///   its start comes back the same way;
/// - its lane is discarded as a failure's is, so nothing it counted reaches its caller, which gets
///   back the history it paid for the records.
///
/// A latched transaction is left to the latch, which writes the stop over every result, and the
/// stop settles as a revert.
fn settle_kept_nothing<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &mut MegaContext<DB, ExtEnvs>,
    input: &FrameInput,
    result: &mut FrameResult,
    depth: usize,
) {
    carry_upfront_flags(input, result);
    if !result.instruction_result().is_ok() || context.additional_limit.latched().is_some() {
        return;
    }
    // A creation answered with a success and no address is one revm's settlement gives the
    // charge back for itself.
    let upfront = upfront_charge(input).filter(|_| result.refundable_state_gas_charge().is_none());
    // A failed lookup records its cause, which fails the transaction before the result settles.
    let upfront = upfront.map_or(Some(0), |charge| context.state_gas_charge(charge));
    let history =
        if depth == 0 { context.additional_limit.top_level_write_record_gas() } else { 0 };
    let gas = result.gas_mut();
    gas.rollback_state_gas();
    gas.set_refunded(0);
    gas.refill_reservoir(upfront.unwrap_or_default());
    gas.refill_history(history);
    context.additional_limit.discard_returning_lane();
}

/// Sets `result`'s upfront-charge flags to those of `input`, the frame it answers: what revm's
/// settlement reads to give the calling opcode's upfront state-gas charge back.
fn carry_upfront_flags(input: &FrameInput, result: &mut FrameResult) {
    match (input, result) {
        (FrameInput::Call(inputs), FrameResult::Call(outcome)) => {
            outcome.charged_new_account_state_gas = inputs.charged_new_account_state_gas;
            outcome.charged_state_gas_address = inputs.target_address;
        }
        (FrameInput::Create(inputs), FrameResult::Create(outcome)) => {
            outcome.charged_create_state_gas = inputs.charged_create_state_gas();
            outcome.charged_state_gas_address = inputs.charged_state_gas_address();
        }
        _ => {}
    }
}

/// The upfront state-gas charge the calling opcode made for the frame `input` starts: the charge
/// revm's settlement gives back when that frame fails.
fn upfront_charge(input: &FrameInput) -> Option<StateGasCharge> {
    synthetic_frame_result(input, InstructionResult::Revert, Bytes::new())
        .refundable_state_gas_charge()
}

/// Puts a revived creation back to `before` and, unless the transaction is latched, records the
/// refusal as the context's error.
fn refuse_create_revival<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &mut MegaContext<DB, ExtEnvs>,
    before: InstructionResult,
    result: &mut FrameResult,
) {
    let FrameResult::Create(outcome) = result else { return };
    if before.is_ok() || !outcome.result.result.is_ok() {
        return;
    }
    outcome.result.result = before;
    if context.additional_limit.latched().is_none() && context.error().is_ok() {
        let message: String = format!("{FORBIDDEN_CREATE_REVIVAL}: {before:?}");
        *context.error() = Err(ContextError::Custom(message));
    }
}
