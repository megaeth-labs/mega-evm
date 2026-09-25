//! The settlement of a keyless deployment: the creation's result returned into the
//! `keylessDeploy` call that started it, and the call's answer in the `IKeylessDeploy` ABI.

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use revm::{
    context::{
        result::{HaltReason, OutOfGasError},
        ContextError, ContextTr, FrameStack, JournalTr,
    },
    context_interface::{cfg::gas::GasTracker, journaled_state::account::JournaledAccountTr},
    handler::{handle_reservoir_remaining_gas, EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, Gas, InstructionResult, InterpreterAction, SuccessOrHalt,
    },
    primitives::KECCAK_EMPTY,
};

use crate::{
    system::keyless::{encode_error_result, IKeylessDeploy, KeylessDeployError},
    ExternalEnvTypes, MegaContext, MegaHaltReason,
};

use super::dispatch::{KeylessFrame, Started};

/// What a `keylessDeploy` call answers from, once its creation returned into it.
#[derive(Debug)]
pub(crate) struct Returned {
    started: Started,
    creation: Creation,
}

/// What the creation a `keylessDeploy` call started returned.
#[derive(Debug)]
pub(crate) struct Creation {
    result: InstructionResult,
    output: Bytes,
    /// What the creation spent ([`creation_gas_used`]).
    gas_used: u64,
    /// The address revm reports the creation at.
    created: Option<Address>,
}

/// The creation a `keylessDeploy` call started, when `result`, about to be returned to the frame
/// that waits for it, is that creation's, returning into the call; `None` for every other result.
///
/// It is read here, before revm merges it into the call, which keeps nothing of it but its gas and
/// whether it succeeded. The frame stack says whether the result lands in the call
/// ([`returns_into_the_call`]).
pub(crate) fn returning<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    stack: &mut FrameStack<EthFrame<EthInterpreter>>,
    result: &FrameResult,
) -> Option<Creation> {
    if !matches!(ctx.keyless_frame, Some(KeylessFrame::Deploying(_))) {
        return None;
    }
    if !returns_into_the_call(stack) {
        return None;
    }
    let result_code = result.instruction_result();
    // The creation's gas as revm's settlement leaves it before the merge: a creation that failed
    // rolls its state and history gas back, and one that halted spends its regular gas.
    let mut gas = *result.gas();
    handle_reservoir_remaining_gas(result_code, &mut GasTracker::default(), gas.tracker_mut());
    Some(Creation {
        result: result_code,
        output: result.interpreter_result().output.clone(),
        gas_used: creation_gas_used(&gas),
        created: match result {
            FrameResult::Create(outcome) => outcome.address,
            FrameResult::Call(_) => None,
        },
    })
}

/// Whether the result about to be returned lands in the frame at the bottom of the stack, which in
/// a keyless deployment is the `keylessDeploy` call.
///
/// It does in two cases: the top of the stack is the creation, at index 1 and finished, which revm
/// pops before it hands the result to the frame below; or the top is the call itself, at index 0
/// and not finished, which a creation answered without a frame of its own returns into directly.
/// A frame the creation started returning (index 2, finished) or answered while the creation runs
/// (index 1, not finished) lands in the creation instead.
fn returns_into_the_call(stack: &mut FrameStack<EthFrame<EthInterpreter>>) -> bool {
    let Some(top) = stack.index() else { return false };
    top == usize::from(stack.get().is_finished())
}

/// Settles `creation`, which revm has just merged into the `keylessDeploy` call whose gas is
/// `gas`, and keeps what the call answers from on its resume ([`answer`]).
///
/// The merge is revm's own, as for a `CREATE`'s frame: the unspent forward, the reservoir, the
/// creation's state and history gas when it succeeded, and the created account's charge when it
/// did not — priced through the same hook it was charged through — with the history of the records
/// the creation did not keep given back after it.
///
/// What is left is what a `CREATE` has no counterpart for. The signer's account comes back when
/// the creation's start never created it. And a deployment from nonce 0 keeps the creation's nonce
/// bump, while from nonce 1 the bump is taken back, whether the deployment succeeded or failed,
/// only when it is the last nonce change the deployment made ([`take_back_bump`]), leaving the
/// nonce at 1, as the legacy engine did; its write record and that record's history go with it,
/// unless the creation succeeded and moved value out of the signer. The call is permissionless
/// and the signed transaction public, so a nonce every failure spent would let anybody make a
/// signer's address undeployable with two failing calls; and a success that spent it would answer
/// a resubmission with `SignerNonceTooHigh` rather than `ContractAlreadyExists`. When the signer's
/// own code spent a nonce in the constructor that survived it — on a default configuration, a
/// delegated signer's `CREATE` or `CREATE2`, successful or not — nothing is taken back: a later
/// bump may stand for an account, so the nonce is never moved back under one. That signer ends
/// above 1, and a resubmission is refused `SignerNonceTooHigh`.
///
/// It runs before the call resumes, because the record it takes back may be what put the call
/// over its budget: the call is held to its limits again without it, and the stop it returns on
/// its resume, if any, is the one that stands then.
pub(crate) fn settle<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    gas: &mut Gas,
    creation: Creation,
) -> Result<(), ContextError<DB::Error>> {
    let Some(KeylessFrame::Deploying(started)) = ctx.keyless_frame.take() else {
        unreachable!("a creation returns into a keylessDeploy call that started it")
    };
    // The creation's start is what creates an empty signer's account. revm bumps the nonce of
    // every creation it is asked to build, and a creation the limits stop at its start is bumped
    // all the same; one an inspector answered in its place never started, and adds no account.
    // A signer that had an account was charged nothing, and gets nothing back.
    let nonce = account_nonce(ctx, started.signer);
    if nonce == started.signer_nonce {
        gas.refill_reservoir(started.signer_account_charge);
    }
    // A deployment from nonce 1 keeps no bump of its own: a failure leaves the address
    // deployable, a success leaves it occupied. The creation's bump is the one above the nonce
    // the call started from, and it is taken back only when it is the last one: a later bump the
    // signer's own code made may stand for an account, so the nonce is never moved back under it.
    if started.signer_nonce > 0 && nonce == started.signer_nonce + 1 {
        take_back_bump(ctx, gas, &started, creation.result)?;
    }
    ctx.keyless_frame = Some(KeylessFrame::Returned(Returned { started, creation }));
    Ok(())
}

/// The action of a `keylessDeploy` call resuming on `gas` once its creation `returned` into it, a
/// stop aside: `keylessDeploy`'s return — the deployed address when the creation left code at it,
/// or the zero address and the error the creation failed with, `ExecutionReverted`,
/// `ExecutionHalted`, or `EmptyCodeDeployed` for a creation that left no code, having deployed none
/// or destroyed itself. The call succeeds, and keeps what the creation's start wrote: a signer at
/// nonce 0 whose deployment failed has spent its nonce all the same.
///
/// `gasUsed` is what the creation spent: its regular gas and the state and history gas it kept,
/// from whichever pool paid it. It is the same whether the transaction has a state-gas reservoir
/// or not, and it counts nothing the call paid for the creation's start.
///
/// A creation revm reports as a success at another address than the pinned one, or at none, is a
/// broken engine rather than a failed deployment: the call reverts with `AddressMismatch()` or
/// `NoContractCreated()`, which no deployment reaches.
pub(crate) fn answer<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    returned: &Returned,
    gas: Gas,
) -> InterpreterAction {
    let (result, output) = answer_of(ctx, returned);
    InterpreterAction::new_return(result, output, gas)
}

/// The result and output of [`answer`].
fn answer_of<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    returned: &Returned,
) -> (InstructionResult, Bytes) {
    let Returned { started, creation } = returned;
    let gas_used = creation.gas_used;
    if creation.result.is_revert() {
        let output = creation.output.clone();
        return failed(KeylessDeployError::ExecutionReverted { gas_used, output }, gas_used);
    }
    if !creation.result.is_ok() {
        let reason = halt_reason(creation.result);
        return failed(KeylessDeployError::ExecutionHalted { gas_used, reason }, gas_used);
    }
    if creation.created != Some(started.deploy_address) {
        let error = if creation.created.is_some() {
            KeylessDeployError::AddressMismatch
        } else {
            KeylessDeployError::NoContractCreated
        };
        return (InstructionResult::Revert, encode_error_result(error));
    }
    if !holds_code(ctx, started.deploy_address) {
        return failed(KeylessDeployError::EmptyCodeDeployed { gas_used }, gas_used);
    }
    returned_value(gas_used, started.deploy_address, Bytes::new())
}

/// Whether the deployment left code at `address`. The code the journal holds, not the code the
/// constructor returned: a constructor that destroyed its own account (EIP-6780) returned code
/// the account does not keep.
fn holds_code<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    address: Address,
) -> bool {
    ctx.journal_ref().state.get(&address).is_some_and(|account| {
        account.info.code_hash != KECCAK_EMPTY && !account.is_selfdestructed()
    })
}

/// Takes back the creation's nonce bump of a signer whose nonce was already 1, with its write
/// record and that record's history, so what stands is what would stand had the creation's start
/// not bumped the nonce: the signer stays at 1, however many deployments it makes.
///
/// The record goes unless the signer's account keeps another write it stands for: the value a
/// creation that succeeded moved out of it. A creation that failed took its value transfer back
/// with the rest of its writes.
fn take_back_bump<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    gas: &mut Gas,
    started: &Started,
    result: InstructionResult,
) -> Result<(), ContextError<DB::Error>> {
    ctx.journal_mut().load_account_mut(started.signer)?.data.set_nonce(started.signer_nonce);
    let moved_value = started.moves_value && result.is_ok();
    if !moved_value && ctx.additional_limit.take_back_creator_record() {
        gas.refill_history(started.signer_record_charge);
    }
    Ok(())
}

/// The nonce of an account the journal holds; zero for one it does not.
fn account_nonce<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    address: Address,
) -> u64 {
    ctx.journal_ref().state.get(&address).map_or(0, |account| account.info.nonce)
}

/// `keylessDeploy`'s return for a deployment that failed with `error`.
fn failed(error: KeylessDeployError, gas_used: u64) -> (InstructionResult, Bytes) {
    returned_value(gas_used, Address::ZERO, encode_error_result(error))
}

/// `keylessDeploy`'s return: a success, whatever the deployment did.
fn returned_value(
    gas_used: u64,
    deployed_address: Address,
    error_data: Bytes,
) -> (InstructionResult, Bytes) {
    let output = IKeylessDeploy::keylessDeployCall::abi_encode_returns(
        &IKeylessDeploy::keylessDeployReturn {
            gasUsed: gas_used,
            deployedAddress: deployed_address,
            errorData: error_data,
        },
    );
    (InstructionResult::Return, output.into())
}

/// Why a creation that ended with `result` halted. The ABI's `ExecutionHalted` carries no reason,
/// so this is what the error value, and whoever handles it off-chain, is told.
fn halt_reason(result: InstructionResult) -> MegaHaltReason {
    match SuccessOrHalt::<MegaHaltReason>::from(result) {
        SuccessOrHalt::Halt(reason) => reason,
        _ => MegaHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Basic)),
    }
}

/// What a creation whose gas settled into `gas` spent, from either pool: its regular gas, and the
/// state and history gas it kept that the reservoir paid.
///
/// Regular gas spent includes the state and history charges that spilled onto it once the
/// reservoir was empty, and nothing the reservoir paid. Adding back what the reservoir paid makes
/// the figure the same whether the transaction was funded above the execution cap or not. A
/// creation that failed kept no state or history gas: its settlement rolled both back, and a
/// halt spent its regular gas.
fn creation_gas_used(gas: &Gas) -> u64 {
    let kept = gas.state_gas_spent().saturating_add(gas.history_gas_spent());
    let from_reservoir = kept.saturating_sub_unsigned(gas.state_gas_spilled());
    gas.total_gas_spent().saturating_add(u64::try_from(from_reservoir).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A halt is reported with its own reason.
    #[test]
    fn test_a_halt_keeps_its_reason() {
        assert_eq!(
            halt_reason(InstructionResult::InvalidFEOpcode),
            MegaHaltReason::Base(HaltReason::InvalidFEOpcode),
        );
        assert_eq!(
            halt_reason(InstructionResult::CreateContractStartingWithEF),
            MegaHaltReason::Base(HaltReason::CreateContractStartingWithEF),
        );
        assert_eq!(
            halt_reason(InstructionResult::Return),
            MegaHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Basic)),
            "what is not a halt is reported as running out of gas",
        );
    }

    /// The creation's spend is the same figure whichever pool paid its state and history gas.
    #[test]
    fn test_the_creation_gas_used_does_not_depend_on_the_pool() {
        // No reservoir: every state and history charge spills onto regular gas.
        let mut narrow = Gas::new_with_regular_gas_and_reservoir(1_000_000, 0);
        assert!(narrow.record_regular_cost(30_000));
        assert!(narrow.record_state_cost(200_000));
        assert!(narrow.record_history_cost(4_000));

        // A reservoir that pays for all of it.
        let mut wide = Gas::new_with_regular_gas_and_reservoir(1_000_000, 10_000_000);
        assert!(wide.record_regular_cost(30_000));
        assert!(wide.record_state_cost(200_000));
        assert!(wide.record_history_cost(4_000));

        // Half and half: the reservoir runs dry in the middle of the state charge.
        let mut split = Gas::new_with_regular_gas_and_reservoir(1_000_000, 100_000);
        assert!(split.record_regular_cost(30_000));
        assert!(split.record_state_cost(200_000));
        assert!(split.record_history_cost(4_000));

        for gas in [narrow, wide, split] {
            assert_eq!(creation_gas_used(&gas), 234_000, "{gas:?}");
        }
    }

    /// A creation that failed kept nothing but the regular gas it spent: a revert gives back the
    /// spill with the rollback, a halt spends the whole forward.
    #[test]
    fn test_a_failed_creation_spent_its_regular_gas_alone() {
        for reservoir in [0, 10_000_000] {
            let mut gas = Gas::new_with_regular_gas_and_reservoir(1_000_000, reservoir);
            assert!(gas.record_regular_cost(30_000));
            assert!(gas.record_state_cost(200_000));
            gas.rollback_state_gas();
            assert_eq!(creation_gas_used(&gas), 30_000, "a revert, reservoir {reservoir}");
            gas.spend_all();
            assert_eq!(creation_gas_used(&gas), 1_000_000, "a halt, reservoir {reservoir}");
        }
    }

    /// A creation that refilled more than it charged — a slot written back that its caller had
    /// filled — does not report less than the regular gas it spent.
    #[test]
    fn test_a_net_refill_does_not_lower_the_regular_spend() {
        let mut gas = Gas::new_with_regular_gas_and_reservoir(1_000_000, 0);
        assert!(gas.record_regular_cost(30_000));
        gas.refill_reservoir(50_000);
        assert_eq!(creation_gas_used(&gas), 30_000);
    }
}
