//! What an inspector may do to a Satin transaction, and what its rewrites do not change.
//!
//! An inspector is not a passive observer. A callback holding a live interpreter can write to its
//! gas and its pending action, a callback holding a frame's inputs can change them or answer the
//! frame itself, and every `*_end` callback can rewrite a result's classification, gas and output.
//! Satin keeps no ledger beside revm's `Gas` (compute is the regular gas spent), so an inspector
//! that changes the gas changes what the transaction owes, and a rewriting inspector gets what it
//! asked for, within the bounds below. Rewriting is a tool feature — Foundry's cheatcodes are built
//! on it — supported and unmeasured.
//!
//! # Where an inspector runs
//!
//! - **A user transaction** runs every callback: `initialize_interp`, `step` and `step_end`,
//!   `log_full`, `log` for what a frame journals without running an instruction (an EIP-7708
//!   transfer log, a precompile's logs), `frame_start` and `frame_end`, `call` and `call_end`,
//!   `create` and `create_end`, and `selfdestruct`. A keyless deployment is seen as the frames it
//!   is made of: its `keylessDeploy` call, which runs no instruction and so gets no
//!   `initialize_interp` and no step, and the creation below it. The inspector may rewrite anything
//!   outside block execution, within the bounds below.
//! - **A system transaction** — a system-address transaction, promoted to a deposit before its
//!   first callback — runs through the same handler and the same callbacks, and may be rewritten
//!   the same way. It is the protocol's own work, held to no per-transaction limit, so no limit
//!   stops any of its frames.
//! - **A pre-block system call** — the EIP-2935 and EIP-4788 calls, and the `SequencerRegistry`'s
//!   `applyPendingChanges()` — runs on the handler's plain system-call path, as alloy-evm's
//!   `transact_system_call` runs one: no callback, whatever inspector the EVM holds, so a block's
//!   tracer sees the block's transactions and nothing of its preparation. A tool that wants to
//!   watch a system call runs it through revm's `InspectSystemCallEvm`, which does hand it to the
//!   inspector, with every exemption a system call has.
//! - **The end of a block** runs no code: the post-block balance increments are database writes. No
//!   callback runs, and the EVM handed back carries the inspector with what it collected.
//!
//! # Block execution
//!
//! Block execution admits any inspector: the journal's ledger follows the journal, so a rewrite
//! cannot put the ledger out of step with state, and which inspectors may run on block production
//! is a node's decision, not the engine's.
//!
//! # What a rewrite does not change
//!
//! - **A frame that kept nothing.** A frame the inspector answered in place of running never
//!   started, and a frame that failed had its journal checkpoint reverted before the inspector saw
//!   its result. Whatever the inspector hands the caller for either — Foundry's `expectRevert`
//!   turns a failed call into a success — the caller sees the success, the output and the regular
//!   gas the inspector chose, and nothing else of the frame: no state or history gas, no refund and
//!   no write record for writes the journal does not hold, and the upfront state gas its caller's
//!   opcode was charged for an account the frame did not add comes back, at the price of the
//!   account it was charged for: a call whose target the inspector's `call` hook rewrote gives the
//!   charge back as recorded before the hook ran. A creation revived this way hands its creator the
//!   address the inspector gave, with nothing deployed there.
//! - **A success rewritten into a failure keeps nothing either.** A frame the inspector turns from
//!   a success into a failure — Foundry's `expectRevert` on a call that did not revert — has its
//!   journal taken back to its checkpoint, as revm takes back a frame that fails: the frame's own,
//!   or, for a start revm answers without building a frame (a call to a precompile or to an account
//!   with no code), where the start began. So the state follows the failure its ledgers settle as:
//!   no account, slot, code, value moved or log of the frame's is kept, and a creation keeps its
//!   creator's nonce bump, which revm makes before the creation's checkpoint, as one that fails
//!   does. Everything journaled since the checkpoint goes, what the inspector itself wrote to the
//!   journal during the frame or in the callback that rewrote its result included; where revm fails
//!   a frame the checkpoint is gone before `call_end`, so a write made there stays. The journal's
//!   depth is left as it was.
//!
//! A limit stop is the revert of the frame that crossed the limit, and a rewrite of it is a
//! rewrite of a failure like any other: it settles as above, and the transaction reports no stop
//! once the result of its own frame is no longer the stop's revert. Every other rewrite is the
//! tool's business.
//!
//! # What an inspector's writes to gas cannot do
//!
//! - **Gas changed at a frame's end has no effect.** The instruction that ends a frame copies the
//!   frame's gas into the result it returns, so what `step_end` writes to the interpreter's gas
//!   after it reaches nobody. A tool that changes what a frame returns changes its result, in
//!   `call_end`, `create_end` or `frame_end`: that gas is what the caller, or the transaction's
//!   settlement, takes in.
//! - **An inspector cannot overdraw a frame.** A charge the frame's spendable gas cannot pay is
//!   refused and takes nothing, and the frame is not halted: the interpreter does not ask the
//!   callback what it did. A tool that wants a frame out of gas says so through its result. Under
//!   gas detention the refusal of an opcode's charge the withheld part would have paid is the
//!   crossing, and an opcode's failed charge records what the frame had, which detention reads as
//!   what its halt burned; the refusal of an inspector's charge is neither, and classifies nothing
//!   ([`StepGuard`]).
//! - **Gas an inspector puts back carries the split it was taken with.** The part gas detention
//!   withholds is in the frame's `Gas`, and detention sets it where it can have moved: at a read of
//!   volatile data, when the frame starts or resumes, and after an `SSTORE`. An inspector that
//!   overwrites the interpreter's gas — Foundry's `pauseGasMetering` puts a snapshot back before
//!   every instruction — overwrites the split with it, and the frame runs on the snapshot's split
//!   until detention next sets it. Only the two records of a refused charge are kept from a
//!   callback's write.
//! - **An answer carries only the regular gas the inspector chose.** A frame the inspector answered
//!   never ran, so it drew nothing from the reservoir: whatever `Gas` the answer is built on —
//!   `Gas::new(limit)`, as Foundry answers a cheatcode, carries no reservoir — its caller merges
//!   back the reservoir it forwarded, and none of the state gas, history gas or refund the answer
//!   carries. The gas of a result a frame ran to produce is the inspector's to rewrite, reservoir
//!   included.

use revm::{
    context::ContextTr,
    context_interface::{cfg::StateGasCharge, Host},
    handler::FrameResult,
    inspector::handler::frame_end,
    interpreter::{
        gas::WithheldCrossing, FrameInput, Gas, InstructionResult, Interpreter, InterpreterTypes,
    },
    primitives::{Address, Bytes, Log, U256},
    Database, Inspector,
};

use crate::{limit::Mark, synthetic_frame_result, ExternalEnvTypes, MegaContext};

/// Hands a running frame's instruction-loop callbacks to the inspector inside, and keeps what they
/// do to the interpreter's gas from classifying the frame.
///
/// A regular charge the frame's spendable gas cannot pay is refused, and leaves two records: the
/// regular gas the frame had before it, which gas detention reads as what a halting frame burned,
/// and, when the part detention withholds would have paid it, the crossing, which stops the
/// transaction at the compute limit when the frame ends. An opcode's refused charge halts the
/// frame at once, so its records are the frame's end. An inspector's does not: the interpreter
/// never sees it fail and the frame runs on, and a halt that no failed charge records — a cold
/// access the frame cannot pay — keeps a record already there. So both records a callback leaves
/// are put back to what they were before the callback, and the frame ends on the charges it made
/// itself; a record an opcode's own failure left before the callback stays.
///
/// It stands in for the inspector in revm's instruction loop (`inspect_instructions`), which makes
/// four of the callbacks forwarded here and no other, and for the one callback a frame's start
/// hands the interpreter to before the loop, `initialize_interp`; the rest of frame starts and
/// ends reach the inspector itself. The other callbacks are left to the trait's no-op defaults
/// rather than forwarded, because the loop never makes them and a forward nothing reaches is one
/// nothing tests. So a callback the loop gains in a later revision of the fork reaches no inspector
/// until it is forwarded here: a fork upgrade checks the loop's callbacks against this list.
pub(crate) struct StepGuard<'a, I>(pub(crate) &'a mut I);

impl<I, CTX, INTR> Inspector<CTX, INTR> for StepGuard<'_, I>
where
    I: Inspector<CTX, INTR>,
    INTR: InterpreterTypes,
{
    #[inline]
    fn initialize_interp(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let records = gas_records(&interp.gas);
        self.0.initialize_interp(interp, context);
        restore_gas_records(&mut interp.gas, records);
    }

    #[inline]
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let records = gas_records(&interp.gas);
        self.0.step(interp, context);
        restore_gas_records(&mut interp.gas, records);
    }

    #[inline]
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        let records = gas_records(&interp.gas);
        self.0.step_end(interp, context);
        restore_gas_records(&mut interp.gas, records);
    }

    #[inline]
    fn log_full(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX, log: Log) {
        let records = gas_records(&interp.gas);
        self.0.log_full(interp, context, log);
        restore_gas_records(&mut interp.gas, records);
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.0.selfdestruct(contract, target, value);
    }
}

/// A frame's two records of a refused regular charge: the crossing of its withheld gas and the
/// regular gas it had before the charge.
type GasRecords = (Option<WithheldCrossing>, Option<u64>);

/// The records of a refused charge `gas` holds before a callback runs.
#[inline]
const fn gas_records(gas: &Gas) -> GasRecords {
    (gas.withheld_crossing(), gas.failed_charge_remaining())
}

/// Puts the records [`gas_records`] read back on `gas`, over whatever the callback left.
#[inline]
const fn restore_gas_records(gas: &mut Gas, (crossing, failed): GasRecords) {
    gas.set_withheld_crossing(crossing);
    gas.set_failed_charge_remaining(failed);
}

/// What the calling opcode charged upfront for a call, as it stood before the inspector could see
/// the call: whether it charged the new-account state gas, and for which address.
///
/// An inspector's `call` hook holds the call's inputs mutably, and a rewritten target would
/// otherwise be the address the charge is given back at, priced in another SALT bucket than the one
/// it was made in. The record is taken before `frame_start` and written into the frame's result
/// when the frame ends, whatever produced the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UpfrontRecord {
    charged_new_account_state_gas: bool,
    charged_state_gas_address: Address,
}

impl UpfrontRecord {
    /// The record of the call `input` starts; none for any other frame, whose creation inputs keep
    /// what they charged and where in fields an inspector's hook cannot write.
    pub(crate) fn of(input: &FrameInput) -> Option<Self> {
        match input {
            FrameInput::Call(inputs) => Some(Self {
                charged_new_account_state_gas: inputs.charged_new_account_state_gas,
                charged_state_gas_address: inputs.target_address,
            }),
            _ => None,
        }
    }

    /// `input` with the call's upfront charge as the record has it.
    fn restore(self, input: &FrameInput) -> FrameInput {
        let mut input = input.clone();
        if let FrameInput::Call(inputs) = &mut input {
            inputs.charged_new_account_state_gas = self.charged_new_account_state_gas;
            inputs.target_address = self.charged_state_gas_address;
        }
        input
    }
}

/// Where the result [`frame_end_checked`] hands the inspector came from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ResultSource {
    /// The inspector answered the frame in place of running it: nothing of the frame is in the
    /// journal.
    Inspector,
    /// The engine produced it — a frame that ran, or a start revm or the engine answered without
    /// building a frame — and what the frame journaled begins at the mark: the frame's own
    /// checkpoint, or where the journal stood when the start began, with the ledger as it stood
    /// there.
    Engine(Mark),
}

/// Hands a frame's end to the inspector, then settles what its rewrite does not change.
///
/// - **A frame that kept nothing.** A frame the inspector answered in place of running, and a frame
///   that failed before the inspector saw it, made no write the journal kept. Whatever the
///   inspector left the result saying, it settles into its caller as such a frame does
///   ([`settle_kept_nothing`]).
/// - **A success rewritten into a failure.** The journal follows the result: what the frame
///   journaled is taken back to its checkpoint, as revm takes back a frame that fails, so the state
///   holds none of the writes the failure's settlement bills nothing for. A frame the inspector
///   answered journaled nothing, so nothing is taken back for it.
///
/// Every place a frame result reaches the inspector goes through here, so both cover a result
/// a frame ran to produce and one answered without running.
#[inline]
pub(crate) fn frame_end_checked<DB, ExtEnvs, INTR, INSP>(
    context: &mut MegaContext<DB, ExtEnvs>,
    inspector: &mut INSP,
    frame_input: &FrameInput,
    frame_result: &mut FrameResult,
    source: ResultSource,
    upfront: Option<UpfrontRecord>,
) where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    INTR: InterpreterTypes,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, INTR, FrameInput, FrameResult>,
{
    let before = frame_result.instruction_result();
    frame_end(context, inspector, frame_input, frame_result);
    // What the opcode charged is what was recorded before the inspector saw the call, not what its
    // hooks left in the inputs.
    let restored = upfront.and_then(|record| {
        (UpfrontRecord::of(frame_input) != Some(record)).then(|| record.restore(frame_input))
    });
    let frame_input = restored.as_ref().unwrap_or(frame_input);
    if upfront.is_some() {
        carry_upfront_flags(frame_input, frame_result);
    }
    let answered = match source {
        ResultSource::Inspector => true,
        ResultSource::Engine(mark) => {
            if before.is_ok() && !frame_result.instruction_result().is_ok() {
                context.journal_mut().revert_to(&mark);
            }
            false
        }
    };
    if answered || !before.is_ok() {
        settle_kept_nothing(context, frame_input, frame_result, answered);
    }
}

/// Settles the result of a frame that kept nothing in the journal — one an inspector
/// answered in place of running, or one whose checkpoint was reverted before the inspector saw its
/// result — as such a frame settles, whatever the inspector made of the result.
///
/// The calling opcode's upfront state-gas charge — the account a value call adds, the account a
/// creation adds, or EIP-2780's for the transaction's own frame — is given back: the frame added
/// no account. The result carries the frame's own upfront-charge flags again, so a result that
/// fails gets it back from revm's settlement, as revm's own do; one an inspector answered carries
/// none of them otherwise.
///
/// A frame the inspector answered (`answered`) never ran, so it drew nothing from its pools: its
/// answer's gas carries nothing but the regular gas the inspector chose. Whatever `Gas` the answer
/// was built on — `Gas::new(limit)`, which carries no reservoir, or one carrying charges — its
/// state and history charges are rolled back, its refund is dropped, and its reservoir is the one
/// the frame inherited, so its caller merges back the reservoir it forwarded. The rollback comes
/// first: it unwinds the charges against the answer's own reservoir, which the inherited one then
/// replaces, so a charge the answer carries is counted neither in the reservoir nor on a ledger.
///
/// A result that says success is not one revm's settlement treats as keeping nothing, so it is made
/// to settle like the failure it stands for, save what the inspector chose — the success its caller
/// sees, the output, the regular gas:
///
/// - its state and history charges are rolled back and its refunds dropped, as a failure's are,
///   before its caller merges them;
/// - the upfront charge comes back through it: the result holds it as state its frame gave back, as
///   a frame that restored a slot its caller filled does, and its caller's merge nets it out.
///
/// What the frame counted is the journal's ledger's, which the frame's checkpoint took back with
/// the frame's writes, or which never counted the start of a frame the inspector answered: its
/// caller gets back the history it paid for the start's records once the frame returns, and the
/// transaction the history of its own frame's record at the settlement, as for any frame that kept
/// nothing.
fn settle_kept_nothing<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: &mut MegaContext<DB, ExtEnvs>,
    input: &FrameInput,
    result: &mut FrameResult,
    answered: bool,
) {
    carry_upfront_flags(input, result);
    let success = result.instruction_result().is_ok();
    let gas = result.gas_mut();
    if answered || success {
        gas.rollback_state_gas();
        gas.set_refunded(0);
    }
    if answered {
        gas.set_reservoir(inherited_reservoir(input));
    }
    if !success {
        return;
    }
    // A creation answered with a success and no address is one revm's settlement gives the
    // charge back for itself.
    let upfront = upfront_charge(input).filter(|_| result.refundable_state_gas_charge().is_none());
    // A failed lookup records its cause, which fails the transaction before the result settles.
    let upfront = upfront.map_or(Some(0), |charge| context.state_gas_charge(charge));
    result.gas_mut().refill_reservoir(upfront.unwrap_or_default());
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

/// The reservoir the frame `input` starts inherits from its caller.
const fn inherited_reservoir(input: &FrameInput) -> u64 {
    match input {
        FrameInput::Call(inputs) => inputs.reservoir,
        FrameInput::Create(inputs) => inputs.reservoir(),
        FrameInput::Empty => 0,
    }
}

/// The upfront state-gas charge the calling opcode made for the frame `input` starts: the charge
/// revm's settlement gives back when that frame fails.
fn upfront_charge(input: &FrameInput) -> Option<StateGasCharge> {
    synthetic_frame_result(input, InstructionResult::Revert, Bytes::new())
        .refundable_state_gas_charge()
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::interpreter::interpreter::EthInterpreter;

    /// Charges `amount` of regular gas from every callback that holds the interpreter, and
    /// ignores whether the frame could pay it.
    struct Charges(u64);

    impl Inspector<(), EthInterpreter> for Charges {
        fn initialize_interp(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut ()) {
            let _ = interp.gas.record_regular_cost(self.0);
        }

        fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut ()) {
            let _ = interp.gas.record_regular_cost(self.0);
        }

        fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut ()) {
            let _ = interp.gas.record_regular_cost(self.0);
        }

        fn log_full(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut (), _: Log) {
            let _ = interp.gas.record_regular_cost(self.0);
        }
    }

    /// Runs `callback` of [`StepGuard`] around `inspector` on `interp`.
    fn guarded(callback: &str, inspector: &mut Charges, interp: &mut Interpreter<EthInterpreter>) {
        let mut guard = StepGuard(inspector);
        match callback {
            "initialize_interp" => guard.initialize_interp(interp, &mut ()),
            "step" => guard.step(interp, &mut ()),
            "step_end" => guard.step_end(interp, &mut ()),
            "log_full" => guard.log_full(interp, &mut (), Log::default()),
            _ => unreachable!(),
        }
    }

    /// A charge a callback cannot pay is refused and leaves no failed-charge record behind: the
    /// frame did not fail, and a record left there would be read as what the frame had when a
    /// later halt that records nothing of its own ended it. A record an opcode's own failed charge
    /// left before the callback stays as it was, whatever the callback's charge did.
    #[test]
    fn test_a_callback_leaves_the_failed_charge_record_as_it_was() {
        for callback in ["initialize_interp", "step", "step_end", "log_full"] {
            let mut interp = Interpreter { gas: Gas::new(1_000), ..Default::default() };
            guarded(callback, &mut Charges(1_001), &mut interp);
            assert_eq!(interp.gas.remaining(), 1_000, "{callback}: the charge took nothing");
            assert_eq!(interp.gas.failed_charge_remaining(), None, "{callback}: no record");

            // An opcode's charge failed with 1,000 left, and its halt zeroed the frame's gas: a
            // callback after it fails its own charge on nothing left.
            assert!(!interp.gas.record_regular_cost(1_500));
            interp.gas.spend_all();
            guarded(callback, &mut Charges(1), &mut interp);
            assert_eq!(
                interp.gas.failed_charge_remaining(),
                Some(1_000),
                "{callback}: the opcode's record"
            );
        }
    }
}
