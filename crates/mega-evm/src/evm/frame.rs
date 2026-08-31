//! Turning a frame's final action into a frame result, with the journal decision withheld.
//!
//! # Why `MegaETH` owns this
//!
//! revm assembles a frame's result, decides whether the frame's journal checkpoint commits or
//! reverts, and — for a contract creation — runs the deposit predicates and writes the code, all
//! inside one function, and it runs the inspector's last mutating callback *after* that function
//! returns. So the classification an inspector is handed is already carved into state.
//!
//! `MegaETH` needs the opposite order. Its resource accounting settles a frame once, on the
//! frame's final classification, and an inspector is allowed to rewrite that classification; a
//! settlement taken before the rewrite would book a result that never reaches the caller, and a
//! journal committed before the rewrite would leave state behind that the reported result denies.
//!
//! This module therefore splits the upstream function in two. [`classify_frame_action`] does
//! everything that decides *what the frame's result is* — the create-return predicates, the
//! code-deposit charge, assembling the outcome — and records what the journal will have to be told
//! as a [`FrameJournalVerdict`]. [`commit_frame_journal`] carries that verdict out, once the
//! result is final. Between the two sit the inspector's last callback and `MegaETH`'s single
//! frame settlement point.
//!
//! # Upstream lockstep
//!
//! REVIEW ON UPSTREAM BUMP: [`classify_frame_action`] and [`commit_frame_journal`] together must
//! stay a faithful re-ordering of `revm_handler::EthFrame::process_next_action` and
//! `revm_handler::frame::return_create`. A revm bump that changes what those do — a new predicate,
//! a different charge, a changed journal decision — has to be mirrored here, because nothing in
//! the type system ties the two together. The debug assertion in [`classify_create_return`]
//! catches one specific class of drift (the deposit predicate `MegaETH` weighs against) and
//! nothing else.

use alloy_primitives::{Address, Bytes};
use revm::{
    context::{Cfg, ContextTr, JournalTr},
    context_interface::journaled_state::JournalCheckpoint,
    handler::{EthFrame, FrameData, FrameResult, ItemOrResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallOutcome, CreateOutcome,
        FrameInput, InstructionResult, InterpreterAction, InterpreterResult,
    },
    primitives::hardfork::SpecId,
    state::Bytecode,
};

/// What the journal has to be told about a frame, once that frame's result is final.
///
/// The variants carry the decision the *classification* reached, not the decision that will be
/// carried out: a creation whose predicates all passed still reverts if the final result is no
/// longer successful, and the code it would have deposited is dropped.
#[derive(Clone, Debug)]
pub(crate) enum FrameJournalVerdict {
    /// A call frame: commit if the final result is successful, revert otherwise.
    Call,
    /// A contract creation the deposit predicates turned away — an oversized runtime code, an
    /// `0xEF` prefix, or a code-deposit charge the frame could not afford. Reverts
    /// unconditionally: the classification already failed the frame, and nothing a later
    /// rewrite says brings the rejected code back.
    CreateRejected,
    /// A contract creation that passed every deposit predicate and whose code is ready to be
    /// written, if the final result is still successful.
    ///
    /// Holding the code here rather than re-reading the result's output at commit time is what
    /// makes the deposit structurally unable to follow a rewrite: the bytes written are the bytes
    /// the predicates approved, and they are written only on the branch this verdict allows.
    CreateAccepted { address: Address, code: Bytes },
}

/// A frame's result, with the journal not yet told what to do with it.
#[derive(Debug)]
pub(crate) struct PendingFrame {
    /// The frame's result as classified.
    result: FrameResult,
    /// The journal decision the classification reached but did not carry out.
    journal: PendingJournal,
}

impl PendingFrame {
    /// Hands out the result and the journal decision still owed on it, so that the caller can put
    /// its own work between the two — or not.
    pub(crate) fn split(self) -> (FrameResult, PendingJournal) {
        (self.result, self.journal)
    }
}

/// A journal decision a frame's classification reached, waiting to be carried out.
#[derive(Debug)]
pub(crate) struct PendingJournal {
    verdict: FrameJournalVerdict,
    /// The frame's own journal checkpoint, to revert to.
    checkpoint: JournalCheckpoint,
}

/// The classification half of revm's `process_next_action`: everything that decides a frame's
/// result, and nothing that writes state.
///
/// Returns the child frame to build when the action is a new frame — that path settles nothing,
/// because the frame is suspended rather than finished.
pub(crate) fn classify_frame_action<CTX: ContextTr>(
    ctx: &CTX,
    frame: &mut EthFrame<EthInterpreter>,
    action: InterpreterAction,
) -> ItemOrResult<FrameInit, PendingFrame> {
    let mut interpreter_result = match action {
        InterpreterAction::NewFrame(frame_input) => {
            return ItemOrResult::Item(FrameInit {
                frame_input,
                depth: frame.depth + 1,
                memory: frame.interpreter.memory.new_child_context(),
            })
        }
        InterpreterAction::Return(result) => result,
    };

    let (result, verdict) = match &frame.data {
        FrameData::Call(call_frame) => {
            // Propagate the EIP-8037 new-account state-gas flag from the frame input so the parent
            // can refund the upfront charge if the call ends in revert or halt.
            let charged_new_account_state_gas = match &frame.input {
                FrameInput::Call(inputs) => inputs.charged_new_account_state_gas,
                _ => false,
            };
            let mut outcome =
                CallOutcome::new(interpreter_result, call_frame.return_memory_range.clone());
            outcome.charged_new_account_state_gas = charged_new_account_state_gas;
            (FrameResult::Call(outcome), FrameJournalVerdict::Call)
        }
        FrameData::Create(create_frame) => {
            let address = create_frame.created_address;
            let verdict = classify_create_return(ctx, &mut interpreter_result, address);
            (FrameResult::Create(CreateOutcome::new(interpreter_result, Some(address))), verdict)
        }
    };

    ItemOrResult::Result(PendingFrame {
        result,
        journal: PendingJournal { verdict, checkpoint: frame.checkpoint },
    })
}

/// The state-writing half: the journal decision the classification recorded, carried out against
/// the frame's *final* result.
///
/// Two rewrites are possible between the two halves, and this is where each of them lands:
///
/// - a successful frame rewritten into a failure reverts, and a creation deposits no code — the
///   caller is told the frame failed, and the state agrees;
/// - a failed frame rewritten into a success cannot commit a creation, because the verdict a
///   rejected creation carries has no code and no commit branch. (The measurement shim refuses that
///   rewrite outright and restores the original classification, so this is the second of two
///   independent stops rather than the only one.)
pub(crate) fn commit_frame_journal<CTX: ContextTr>(
    ctx: &mut CTX,
    pending: PendingJournal,
    result: &FrameResult,
) {
    let PendingJournal { verdict, checkpoint } = pending;
    let is_ok = result.instruction_result().is_ok();
    let journal = ctx.journal_mut();
    match verdict {
        FrameJournalVerdict::Call => {
            if is_ok {
                journal.checkpoint_commit();
            } else {
                journal.checkpoint_revert(checkpoint);
            }
        }
        FrameJournalVerdict::CreateRejected => journal.checkpoint_revert(checkpoint),
        FrameJournalVerdict::CreateAccepted { address, code } => {
            if is_ok {
                journal.checkpoint_commit();
                journal.set_code(address, Bytecode::new_legacy(code));
            } else {
                journal.checkpoint_revert(checkpoint);
            }
        }
    }
}

/// The classification half of revm's `return_create`: the deposit predicates and the code-deposit
/// charge, with every journal write deferred to the verdict.
///
/// The predicates run in upstream's order, because they do not commute: the code-size limit is
/// checked before the deposit is charged so that oversized code is not billed for storage it never
/// gets, and the `0xEF` rejection is checked before that same charge for the same reason.
fn classify_create_return<CTX: ContextTr>(
    ctx: &CTX,
    interpreter_result: &mut InterpreterResult,
    address: Address,
) -> FrameJournalVerdict {
    let cfg = ctx.cfg();
    let max_code_size = cfg.max_code_size();
    let is_eip3541_disabled = cfg.is_eip3541_disabled();
    let spec_id: SpecId = cfg.spec().into();
    let is_amsterdam_eip8037 = cfg.is_amsterdam_eip8037_enabled();
    let gas_params = cfg.gas_params();
    let gas_for_code = gas_params.code_deposit_cost(interpreter_result.output.len());

    // What `MegaETH`'s own code-deposit accounting predicted this classification would do, weighed
    // a moment ago against this same result. The two are the same decision read twice, so a bump
    // that changes one and not the other turns into a failing assertion rather than into compute
    // gas recorded for a deposit that never happened, or a deposit charged with nothing recorded.
    #[cfg(debug_assertions)]
    let predicted_charge = will_return_create_charge_code_deposit(
        interpreter_result,
        max_code_size,
        spec_id,
        is_eip3541_disabled,
        gas_for_code,
    );

    let verdict = 'classify: {
        if !interpreter_result.result.is_ok() {
            break 'classify FrameJournalVerdict::CreateRejected;
        }

        // EIP-170 / EIP-7954: runtime code size limit, checked before any deposit charge.
        if spec_id.is_enabled_in(SpecId::SPURIOUS_DRAGON) &&
            interpreter_result.output.len() > max_code_size
        {
            interpreter_result.result = InstructionResult::CreateContractSizeLimit;
            break 'classify FrameJournalVerdict::CreateRejected;
        }

        // EIP-3541: reject new contract code starting with the 0xEF byte.
        if !is_eip3541_disabled &&
            spec_id.is_enabled_in(SpecId::LONDON) &&
            interpreter_result.output.first() == Some(&0xEF)
        {
            interpreter_result.result = InstructionResult::CreateContractStartingWithEF;
            break 'classify FrameJournalVerdict::CreateRejected;
        }

        if !interpreter_result.gas.record_regular_cost(gas_for_code) {
            // EIP-2 point 3: a creation that cannot pay for its own code deposit fails out of gas
            // rather than leaving an empty contract behind. Before Homestead it left one.
            if spec_id.is_enabled_in(SpecId::HOMESTEAD) {
                interpreter_result.result = InstructionResult::OutOfGas;
                break 'classify FrameJournalVerdict::CreateRejected;
            }
            interpreter_result.output = Bytes::new();
        }

        // EIP-8037 splits the deposit into a hash charge and a state-gas charge. Every `MegaEVM`
        // configuration pins the EIP off, so this is mirrored for lockstep rather than for reach.
        if is_amsterdam_eip8037 {
            let hash_cost = gas_params.keccak256_cost(interpreter_result.output.len());
            if !interpreter_result.gas.record_regular_cost(hash_cost) {
                interpreter_result.result = InstructionResult::OutOfGas;
                break 'classify FrameJournalVerdict::CreateRejected;
            }
            let state_gas_for_code =
                gas_params.code_deposit_state_gas(interpreter_result.output.len());
            if state_gas_for_code > 0 &&
                !interpreter_result.gas.record_state_cost(state_gas_for_code)
            {
                interpreter_result.result = InstructionResult::OutOfGas;
                break 'classify FrameJournalVerdict::CreateRejected;
            }
        }

        interpreter_result.result = InstructionResult::Return;
        FrameJournalVerdict::CreateAccepted { address, code: interpreter_result.output.clone() }
    };

    // EIP-8037 adds two more charges past the predicate's last one, so the two only have to
    // agree while it is off — which every `MegaEVM` configuration pins it to be.
    #[cfg(debug_assertions)]
    debug_assert!(
        is_amsterdam_eip8037 ||
            predicted_charge == matches!(verdict, FrameJournalVerdict::CreateAccepted { .. }),
        "the code-deposit predicate and the create classification disagreed"
    );

    verdict
}

/// Whether [`classify_create_return`] will charge `code_deposit_gas` and accept the deposit.
///
/// The charge is conditional — the classification only takes it from a creation that clears every
/// deposit predicate — and `MegaETH` has to record the matching compute gas *before* the charge
/// happens, at the frame's exit settlement, while it can still rewrite the result to stop the
/// charge. So the decision is read twice: once here, ahead of time, and once by the classification
/// itself. [`classify_create_return`] asserts in debug builds that the two agreed.
pub(crate) fn will_return_create_charge_code_deposit(
    interpreter_result: &InterpreterResult,
    max_code_size: usize,
    runtime_spec_id: SpecId,
    is_eip3541_disabled: bool,
    code_deposit_gas: u64,
) -> bool {
    if !interpreter_result.result.is_ok() {
        return false;
    }
    if !is_eip3541_disabled &&
        runtime_spec_id.is_enabled_in(SpecId::LONDON) &&
        interpreter_result.output.first() == Some(&0xEF)
    {
        return false;
    }
    if runtime_spec_id.is_enabled_in(SpecId::SPURIOUS_DRAGON) &&
        interpreter_result.output.len() > max_code_size
    {
        return false;
    }
    interpreter_result.gas.remaining() >= code_deposit_gas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, EmptyExternalEnv, MegaContext, MegaSpecId};
    use alloy_primitives::{address, Address as Addr, U256};
    use revm::{
        context::JournalTr,
        interpreter::{Gas, InstructionResult, InterpreterResult},
    };
    use std::{vec, vec::Vec};

    const DEPLOYED: Addr = address!("00000000000000000000000000000000000c0de0");
    /// Ample: the deposit charge for the runtime codes below is a few hundred gas.
    const FRAME_GAS: u64 = 1_000_000;

    fn context() -> MegaContext<MemoryDatabase, EmptyExternalEnv> {
        MegaContext::new(MemoryDatabase::default(), MegaSpecId::REX7)
    }

    fn returned(output: Vec<u8>, gas: u64) -> InterpreterResult {
        InterpreterResult::new(InstructionResult::Return, Bytes::from(output), Gas::new(gas))
    }

    /// Runs the classification and reports what it decided: the rewritten instruction result, and
    /// whether the verdict allows a deposit.
    fn classify(result: &mut InterpreterResult) -> FrameJournalVerdict {
        classify_create_return(&context(), result, DEPLOYED)
    }

    fn accepts(verdict: &FrameJournalVerdict) -> bool {
        matches!(verdict, FrameJournalVerdict::CreateAccepted { .. })
    }

    /// A creation that clears every deposit predicate is accepted, is charged for its code, and
    /// carries the exact bytes the predicates approved.
    #[test]
    fn test_a_clean_creation_is_accepted_and_charged_for_its_code() {
        let mut result = returned(vec![0x00; 32], FRAME_GAS);
        let verdict = classify(&mut result);

        let FrameJournalVerdict::CreateAccepted { address, code } = verdict else {
            panic!("a clean creation must be accepted, got {verdict:?}")
        };
        assert_eq!(address, DEPLOYED);
        assert_eq!(code, Bytes::from(vec![0x00; 32]), "the approved bytes travel with the verdict");
        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(
            FRAME_GAS - result.gas.remaining(),
            32 * revm::interpreter::gas::CODEDEPOSIT,
            "the deposit is charged during classification, not at the journal decision",
        );
    }

    /// Each rejecting predicate rejects, names itself on the result, and — this is the part that
    /// matters for the deposit — leaves no code on the verdict for anything to write.
    #[test]
    fn test_every_rejecting_predicate_rejects_without_code() {
        // (name, runtime code, gas the frame has left, the classification it must produce)
        let cases: Vec<(&str, Vec<u8>, u64, InstructionResult)> = vec![
            (
                "0xEF prefix",
                vec![0xEF, 0x00],
                FRAME_GAS,
                InstructionResult::CreateContractStartingWithEF,
            ),
            (
                "cannot pay the deposit",
                vec![0x00; 32],
                32 * revm::interpreter::gas::CODEDEPOSIT - 1,
                InstructionResult::OutOfGas,
            ),
        ];

        for (name, code, gas, expected) in cases {
            let mut result = returned(code, gas);
            let verdict = classify(&mut result);

            assert!(!accepts(&verdict), "{name}: must not be accepted, got {verdict:?}");
            assert_eq!(result.result, expected, "{name}: classification");
        }
    }

    /// A creation whose frame never succeeded is rejected untouched: the classification does not
    /// charge it, does not rename its failure, and hands the journal a verdict with no code.
    #[test]
    fn test_a_failed_frame_is_rejected_without_being_charged() {
        let mut result = InterpreterResult::new(
            InstructionResult::Revert,
            Bytes::from_static(b"reason"),
            Gas::new(FRAME_GAS),
        );
        let verdict = classify(&mut result);

        assert!(!accepts(&verdict));
        assert_eq!(result.result, InstructionResult::Revert, "the failure keeps its own name");
        assert_eq!(result.gas.remaining(), FRAME_GAS, "and pays nothing for a deposit");
    }

    /// The journal decision follows the *final* result. A creation the predicates accepted, whose
    /// result is then rewritten into a failure, must not leave its code behind.
    #[test]
    fn test_a_creation_rewritten_into_a_failure_deposits_nothing() {
        let mut ctx = context();
        let checkpoint = ctx.journal_mut().checkpoint();
        let mut result = returned(vec![0x60; 32], FRAME_GAS);
        let verdict = classify_create_return(&ctx, &mut result, DEPLOYED);
        assert!(accepts(&verdict), "the fixture must be a creation the predicates accepted");

        // What a `create_end` rewrite does, after the classification and before the journal.
        result.result = InstructionResult::Revert;
        let frame_result = FrameResult::Create(CreateOutcome::new(result, Some(DEPLOYED)));
        commit_frame_journal(&mut ctx, PendingJournal { verdict, checkpoint }, &frame_result);

        let account = ctx.journal_mut().load_account(DEPLOYED).unwrap();
        assert!(account.info.is_empty_code_hash(), "no code may be deposited for a failed frame");
    }

    /// And the other direction: a creation the predicates *rejected*, whose result is then
    /// rewritten into a success, has no code and no commit branch to reach. The rewrite cannot
    /// deposit code that never passed the predicates, whatever the result says.
    #[test]
    fn test_a_rejected_creation_rewritten_into_a_success_still_deposits_nothing() {
        let mut ctx = context();
        let checkpoint = ctx.journal_mut().checkpoint();
        // Runtime code the frame cannot pay to deposit.
        let mut result = returned(vec![0x60; 32], 32 * revm::interpreter::gas::CODEDEPOSIT - 1);
        let verdict = classify_create_return(&ctx, &mut result, DEPLOYED);
        assert!(!accepts(&verdict), "the fixture must be a creation the predicates rejected");

        result.result = InstructionResult::Return;
        let frame_result = FrameResult::Create(CreateOutcome::new(result, Some(DEPLOYED)));
        commit_frame_journal(&mut ctx, PendingJournal { verdict, checkpoint }, &frame_result);

        let account = ctx.journal_mut().load_account(DEPLOYED).unwrap();
        assert!(
            account.info.is_empty_code_hash(),
            "a rejected creation carries no code, so a rewrite has nothing to deposit",
        );
    }

    /// A call frame's journal decision reads the final result and nothing else.
    #[test]
    fn test_a_call_frame_commits_or_reverts_on_its_final_result() {
        for (label, instruction_result, expect_committed) in [
            ("success", InstructionResult::Stop, true),
            ("revert", InstructionResult::Revert, false),
            ("halt", InstructionResult::OutOfGas, false),
        ] {
            let mut ctx = context();
            ctx.journal_mut().load_account(DEPLOYED).expect("the account must load");
            let checkpoint = ctx.journal_mut().checkpoint();
            ctx.journal_mut()
                .sstore(DEPLOYED, U256::from(1), U256::from(7))
                .expect("sstore must reach the in-memory database");

            let frame_result = FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(instruction_result, Bytes::new(), Gas::new(FRAME_GAS)),
                0..0,
            ));
            commit_frame_journal(
                &mut ctx,
                PendingJournal { verdict: FrameJournalVerdict::Call, checkpoint },
                &frame_result,
            );

            let stored = ctx.journal_mut().sload(DEPLOYED, U256::from(1)).unwrap().data;
            assert_eq!(
                stored == U256::from(7),
                expect_committed,
                "{label}: the frame's write must follow its final result",
            );
        }
    }
}
