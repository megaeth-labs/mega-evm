//! An observer that proves, in debug builds, that the inspector it wraps writes nothing back.

use revm::{
    context::{ContextTr, JournalTr},
    handler::FrameResult,
    inspector::JournalExt,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Interpreter,
        InterpreterTypes,
    },
    primitives::{Address, Log, U256},
    Inspector,
};

/// Wraps an inspector that is meant to write nothing back, and proves it in debug builds.
///
/// It forwards every callback to the inspector inside and adds nothing. A debug build checks
/// around every callback and panics at the one that breaks the claim: the interpreter's gas,
/// pending action, stack depth and memory size, the frame's inputs, the frame result, and the
/// journal's entries and logs must be as they were, and no frame may be answered. Writes the check
/// cannot see (a stack word replaced in place, a context field outside the journal) are not caught;
/// the check is a tripwire, not a proof.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadOnlyObserver<I>(pub I);

impl<I> ReadOnlyObserver<I> {
    /// Declares `inspector` a read-only observer.
    pub const fn new(inspector: I) -> Self {
        Self(inspector)
    }

    /// The inspector inside.
    pub fn into_inner(self) -> I {
        self.0
    }
}

impl<I, CTX, INTR> Inspector<CTX, INTR> for ReadOnlyObserver<I>
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
        debug_assert!(answer.is_none(), "a read-only observer answered a frame in `frame_start`");
        if let Some((input, journal)) = before {
            debug_assert!(&input == frame_input, "a read-only observer rewrote a frame's inputs");
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
        debug_assert!(answer.is_none(), "a read-only observer answered a call");
        if let Some(before) = before {
            debug_assert!(&before == inputs, "a read-only observer rewrote a call's inputs");
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
            debug_assert!(&before == outcome, "a read-only observer rewrote a call result");
        }
        assert_journal_unwritten(journal, context, "call_end");
    }

    #[inline]
    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let before = cfg!(debug_assertions).then(|| inputs.clone());
        let journal = journal_snapshot(context);
        let answer = self.0.create(context, inputs);
        debug_assert!(answer.is_none(), "a read-only observer answered a creation");
        if let Some(before) = before {
            debug_assert!(&before == inputs, "a read-only observer rewrote a creation's inputs");
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
            debug_assert!(&before == outcome, "a read-only observer rewrote a creation result");
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
            "a read-only observer wrote to the journal in `{callback}`"
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
            "a read-only observer wrote to the interpreter in `{callback}`"
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
            "a read-only observer rewrote a frame result in `{callback}`"
        );
    }
}
