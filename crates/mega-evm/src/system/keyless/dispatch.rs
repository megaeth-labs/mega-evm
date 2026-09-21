//! The dispatch of `KeylessDeploy` calls.
//!
//! A `keylessDeploy(bytes,uint256)` call is recognised here, charged the fixed overhead, and
//! handed to the keyless rewrite hook, which turns it into the creation it stands for. The
//! rewrite is native keyless deployment's; until it lands the recognised call runs the deployed
//! bytecode, which reverts with `NotIntercepted()`, and the overhead is charged all the same.

use alloy_evm::Database;
use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolError};
use revm::{
    handler::FrameResult,
    interpreter::{CallInputs, InstructionResult},
};

use crate::{
    synthetic_call_result,
    system::{
        intercept::peek_selector,
        keyless::{IKeylessDeploy, KEYLESS_DEPLOY_OVERHEAD_GAS},
    },
    ExternalEnvTypes, MegaContext,
};

/// Recognises a `keylessDeploy` call and charges it, or answers it when it cannot go on.
///
/// # What is dispatched
///
/// Only a transaction reaches the deployment: `depth` is the depth of the frame the call would
/// start, and a call a contract makes (`depth > 0`) is not dispatched, so the deployment's
/// answer is never something an inner caller can read off its own return data. A call whose
/// selector is not `keylessDeploy` is not dispatched either. Both run the deployed bytecode,
/// which answers them differently: a `keylessDeploy` call reaches the method body and its
/// `NotIntercepted()`, while a selector the contract does not declare finds no function and no
/// fallback, so it reverts with empty data.
///
/// # What it charges
///
/// A dispatched call pays [`KEYLESS_DEPLOY_OVERHEAD_GAS`] of regular gas before anything else:
/// the frame is handed that much less gas, so the caller pays it whatever the deployment does.
/// A call that was not forwarded enough gas to pay it is answered out of gas, as a frame that
/// ran out is.
///
/// # What it refuses
///
/// A `keylessDeploy` call carries no value: the deployment is made by the transaction's signer,
/// not by the caller, and the contract holds no balance. A call that carries one is answered
/// with the ABI's own `NoEtherTransfer()`, after the overhead was charged.
pub(crate) fn intercept<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    inputs: &mut CallInputs,
    depth: usize,
) -> Option<FrameResult> {
    if depth != 0 {
        return None;
    }
    let selector = peek_selector(&inputs.input, ctx)?;
    if selector != IKeylessDeploy::keylessDeployCall::SELECTOR {
        return None;
    }
    let Some(charged) = inputs.gas_limit.checked_sub(KEYLESS_DEPLOY_OVERHEAD_GAS) else {
        let mut result = synthetic_call_result(inputs, InstructionResult::OutOfGas, Bytes::new());
        result.gas_mut().spend_all();
        return Some(result);
    };
    inputs.gas_limit = charged;
    if !inputs.call_value().is_zero() {
        return Some(synthetic_call_result(
            inputs,
            InstructionResult::Revert,
            Bytes::from_static(&IKeylessDeploy::NoEtherTransfer::SELECTOR),
        ));
    }
    None
}
