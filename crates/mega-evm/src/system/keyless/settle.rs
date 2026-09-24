//! The settlement of a keyless deployment: the creation's result returned into the
//! `keylessDeploy` call that started it, and the call's answer in the `IKeylessDeploy` ABI.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use revm::{
    context::{
        result::{FromStringError, HaltReason, OutOfGasError},
        ContextTr, JournalTr,
    },
    handler::FrameResult,
    interpreter::{CallOutcome, Gas, InstructionResult, InterpreterResult, SuccessOrHalt},
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
/// started it, and replaces `result` with the call's own. Nothing to do for a transaction that
/// started no keyless deployment.
///
/// The call's gas takes the creation's back exactly as a `CREATE`'s frame does
/// ([`settle_frame_result`]): the unspent forward, the reservoir, the creation's state and history
/// gas when it succeeded, and the created account's charge when it did not — priced through the
/// same hook it was charged through. The history of the records the creation did not keep comes
/// back too. The creation's lane was popped when it returned, or is popped here when it never
/// ran.
///
/// Then the call answers, as a frame that resumes after its child returned would:
///
/// - with the stop a limit left it, when there is one: the latched one, or its own budget's, which
///   the creator's nonce record a failed creation leaves on its lane can cross. The call reverts
///   with it, and its journal checkpoint goes with the revert, the signer's nonce included;
/// - otherwise with `keylessDeploy`'s return: the deployed address when the creation left code at
///   it, or the zero address and the error the creation failed with — `ExecutionReverted`,
///   `ExecutionHalted`, or `EmptyCodeDeployed` for a creation that left no code, having deployed
///   none or destroyed itself. The call succeeds, and keeps what the creation's start wrote: a
///   signer whose deployment failed has spent its nonce all the same.
///
/// `gasUsed` is what the creation spent: its regular gas and the state and history gas it kept,
/// from whichever pool paid it. It is the same whether the transaction has a state-gas reservoir
/// or not, and it counts nothing the call paid for the creation's start.
pub(crate) fn settle<DB, ExtEnvs, ERROR>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    result: &mut FrameResult,
) -> Result<(), ERROR>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    ERROR: From<DB::Error> + FromStringError,
{
    let Some(mut call) = ctx.keyless_call.take() else { return Ok(()) };
    // The call's lane and the creation's: the creation returned through the frame lifecycle when
    // it ran, and never reached it when it was answered at its start.
    if ctx.additional_limit.frame_depth() > 1 {
        let refund = ctx.additional_limit.on_frame_return(result);
        call.returned_history = call.returned_history.saturating_add(refund);
    }
    settle_frame_result::<_, ERROR>(ctx, call.gas.tracker_mut(), result)?;
    call.gas.refill_history(call.returned_history);

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
        memory_offset: call.memory_offset,
        was_precompile_called: false,
        precompile_call_logs: Vec::new(),
        charged_new_account_state_gas: call.charged_new_account_state_gas,
        charged_state_gas_address: KEYLESS_DEPLOY_ADDRESS,
    });
    Ok(())
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
        let reason = match SuccessOrHalt::<MegaHaltReason>::from(instruction_result) {
            SuccessOrHalt::Halt(reason) => reason,
            _ => MegaHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Basic)),
        };
        return failed(KeylessDeployError::ExecutionHalted { gas_used, reason }, gas_used);
    }
    let FrameResult::Create(outcome) = result else {
        return refused(KeylessDeployError::NoContractCreated);
    };
    let Some(address) = outcome.address else {
        return refused(KeylessDeployError::NoContractCreated);
    };
    if address != call.deploy_address {
        return refused(KeylessDeployError::AddressMismatch);
    }
    // The code the journal holds, not the code the constructor returned: a constructor that
    // destroyed its own account (EIP-6780) returned code the account does not keep.
    let deployed = ctx.journal_ref().state.get(&address).is_some_and(|account| {
        account.info.code_hash != KECCAK_EMPTY && !account.is_selfdestructed()
    });
    if !deployed {
        return failed(KeylessDeployError::EmptyCodeDeployed { gas_used }, gas_used);
    }
    returned(gas_used, address, Bytes::new())
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

/// A revert of the call with `error`.
fn refused(error: KeylessDeployError) -> (InstructionResult, Bytes) {
    (InstructionResult::Revert, encode_error_result(error))
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
