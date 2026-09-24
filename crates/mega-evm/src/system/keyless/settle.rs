//! The settlement of a keyless deployment: the creation's result returned into the
//! `keylessDeploy` call that started it, and the call's answer in the `IKeylessDeploy` ABI.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{boxed::Box, vec::Vec};

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use revm::{
    context::{
        result::{FromStringError, HaltReason, OutOfGasError},
        ContextTr, JournalTr,
    },
    context_interface::journaled_state::account::JournaledAccountTr,
    handler::FrameResult,
    interpreter::{
        CallInputs, CallOutcome, Gas, InstructionResult, InterpreterResult, SuccessOrHalt,
    },
    primitives::KECCAK_EMPTY,
};

use crate::{
    settle_frame_result,
    system::keyless::{
        encode_error_result, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
    },
    ExternalEnvTypes, MegaContext, MegaHaltReason,
};

use super::dispatch::KeylessCall;

/// Keeps `refund`, the history gas the creation's lane gave back when the creation returned, for
/// the `keylessDeploy` call that paid it: the creation is the outermost frame revm runs, so no
/// frame of revm's receives it.
pub(crate) fn give_back_history<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    refund: u64,
) {
    if let Some(call) = &mut ctx.keyless_call {
        call.returned_history = call.returned_history.saturating_add(refund);
    }
}

/// Settles the result of a keyless deployment's creation into the `keylessDeploy` call that
/// started it, and replaces `result` with the call's own; returns the call's inputs, which an
/// inspector is told the call ended from. Nothing to do for a transaction that started no keyless
/// deployment.
///
/// The call's gas takes the creation's back exactly as a `CREATE`'s frame does
/// ([`settle_frame_result`]): the unspent forward, the reservoir, the creation's state and history
/// gas when it succeeded, and the created account's charge when it did not — priced through the
/// same hook it was charged through. The history of the records the creation did not keep comes
/// back too. The creation's lane was popped when it returned, or is popped here when it never
/// ran.
///
/// A deployment that failed keeps the signer's nonce bump only when it took the signer from 0 to
/// 1: a failed deployment from nonce 1 leaves it at 1 ([`keep_no_second_bump`]). The call is
/// permissionless and the signed transaction public, so a nonce every failure spent would let
/// anybody make a signer's address undeployable with two failing calls.
///
/// Then the call answers, as a frame that resumes after its child returned would:
///
/// - with the stop a limit left it, when there is one: the latched one, or its own budget's, which
///   the nonce record a failed creation leaves a signer at nonce 0 on the call's lane can cross.
///   The call reverts with it, and its journal checkpoint goes with the revert, the signer's nonce
///   included;
/// - otherwise with `keylessDeploy`'s return: the deployed address when the creation left code at
///   it, or the zero address and the error the creation failed with — `ExecutionReverted`,
///   `ExecutionHalted`, or `EmptyCodeDeployed` for a creation that left no code, having deployed
///   none or destroyed itself. The call succeeds, and keeps what the creation's start wrote: a
///   signer at nonce 0 whose deployment failed has spent its nonce all the same.
///
/// `gasUsed` is what the creation spent: its regular gas and the state and history gas it kept,
/// from whichever pool paid it. It is the same whether the transaction has a state-gas reservoir
/// or not, and it counts nothing the call paid for the creation's start.
pub(crate) fn settle<DB, ExtEnvs, ERROR>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    result: &mut FrameResult,
) -> Result<Option<Box<CallInputs>>, ERROR>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    ERROR: From<DB::Error> + FromStringError,
{
    let Some(mut call) = ctx.keyless_call.take() else { return Ok(None) };
    // The call's lane and the creation's: the creation returned through the frame lifecycle when
    // it ran, and never reached it when it was answered at its start.
    if ctx.additional_limit.frame_depth() > 1 {
        let refund = ctx.additional_limit.on_frame_return(result);
        call.returned_history = call.returned_history.saturating_add(refund);
    }
    settle_frame_result::<_, ERROR>(ctx, call.gas.tracker_mut(), result)?;
    call.gas.refill_history(call.returned_history);
    // The creation's start is what creates an empty signer's account. revm bumps the nonce of
    // every creation it is asked to build, and a creation the limits stop at its start is bumped
    // all the same; one an inspector answered in its place never started, and adds no account.
    // A signer that had an account was charged nothing, and gets nothing back.
    let bumped = account_nonce(ctx, call.signer) != call.signer_nonce;
    if !bumped {
        call.gas.refill_reservoir(call.signer_account_charge);
    }
    // A deployment that failed from nonce 1 keeps no bump: the address stays deployable.
    if bumped && call.signer_nonce > 0 && !holds_code(ctx, call.deploy_address) {
        keep_no_second_bump(ctx, &mut call, result)?;
    }

    let (status, output) = match ctx.additional_limit.stop_before_run() {
        Some(stop) => (InstructionResult::Revert, stop.revert_data()),
        None => answer(ctx, &call, result),
    };
    if status.is_ok() {
        ctx.journal_mut().checkpoint_commit();
    } else {
        ctx.journal_mut().checkpoint_revert(call.checkpoint);
    }
    *result = FrameResult::Call(CallOutcome {
        result: InterpreterResult::new(status, output, call.gas),
        memory_offset: call.inputs.return_memory_offset.clone(),
        was_precompile_called: false,
        precompile_call_logs: Vec::new(),
        charged_new_account_state_gas: call.inputs.charged_new_account_state_gas,
        charged_state_gas_address: KEYLESS_DEPLOY_ADDRESS,
    });
    Ok(Some(call.inputs))
}

/// The `keylessDeploy` answer of a call whose creation settled into `result`.
///
/// A creation revm reports as a success at another address than the pinned one, or at none, is a
/// broken engine rather than a failed deployment: the call reverts with `AddressMismatch()` or
/// `NoContractCreated()`, which no deployment reaches.
fn answer<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    call: &KeylessCall,
    result: &FrameResult,
) -> (InstructionResult, Bytes) {
    let gas_used = creation_gas_used(result.gas());
    let instruction_result = result.instruction_result();
    if instruction_result.is_revert() {
        let output = result.interpreter_result().output.clone();
        return failed(KeylessDeployError::ExecutionReverted { gas_used, output }, gas_used);
    }
    if !instruction_result.is_ok() {
        let reason = halt_reason(instruction_result);
        return failed(KeylessDeployError::ExecutionHalted { gas_used, reason }, gas_used);
    }
    let created = match result {
        FrameResult::Create(outcome) => outcome.address,
        FrameResult::Call(_) => None,
    };
    if created != Some(call.deploy_address) {
        let error = if created.is_some() {
            KeylessDeployError::AddressMismatch
        } else {
            KeylessDeployError::NoContractCreated
        };
        return (InstructionResult::Revert, encode_error_result(error));
    }
    if !holds_code(ctx, call.deploy_address) {
        return failed(KeylessDeployError::EmptyCodeDeployed { gas_used }, gas_used);
    }
    returned(gas_used, call.deploy_address, Bytes::new())
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

/// Takes back the nonce bump of a failed deployment by a signer whose nonce was already 1, with
/// its write record and that record's history, so what stands is what would stand had the
/// creation's start not bumped the nonce: the signer stays at 1, however many deployments fail.
///
/// The record goes unless the signer's account keeps another write it stands for: the value a
/// creation that succeeded moved out of it, having deployed no code. A creation that failed took
/// its value transfer back with the rest of its writes.
fn keep_no_second_bump<DB, ExtEnvs, ERROR>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    call: &mut KeylessCall,
    result: &FrameResult,
) -> Result<(), ERROR>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    ERROR: From<DB::Error>,
{
    ctx.journal_mut().load_account_mut(call.signer)?.data.set_nonce(call.signer_nonce);
    let moved_value = call.moves_value && result.instruction_result().is_ok();
    if !moved_value && ctx.additional_limit.take_back_creator_record() {
        call.gas.refill_history(call.signer_record_charge);
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
    returned(gas_used, Address::ZERO, encode_error_result(error))
}

/// `keylessDeploy`'s return: a success, whatever the deployment did.
fn returned(
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
