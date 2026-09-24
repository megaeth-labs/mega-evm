//! The `MegaLimitControl` system contract.
//!
//! It answers what the calling frame has left of the resources `MegaETH` meters; today that is
//! `remainingComputeGas()`, the compute it could still spend. Its methods are intercepted: the
//! deployed bytecode only carries the ABI and reverts with `NotIntercepted()`. Its Solidity source
//! is `crates/system-contracts/contracts/MegaLimitControl.sol`.

use alloy_evm::Database;
use alloy_primitives::{address, Address, Bytes};
use alloy_sol_types::SolCall;
use revm::{
    handler::FrameResult,
    interpreter::{CallInputs, InstructionResult},
};

use crate::{
    synthetic_call_result,
    system::intercept::{peek_selector, reject_non_zero_transfer},
    ExternalEnvTypes, MegaContext,
};

/// The address of the `MegaLimitControl` system contract.
pub const LIMIT_CONTROL_ADDRESS: Address = address!("0x6342000000000000000000000000000000000005");

/// The code of the `MegaLimitControl` contract.
pub use mega_system_contracts::limit_control::LATEST_CODE as LIMIT_CONTROL_CODE;

/// The code hash of the `MegaLimitControl` contract.
pub use mega_system_contracts::limit_control::LATEST_CODE_HASH as LIMIT_CONTROL_CODE_HASH;

pub use mega_system_contracts::limit_control::IMegaLimitControl;

/// Answers a call to `MegaLimitControl`, or `None` when the selector is not `remainingComputeGas`
/// and the deployed bytecode runs.
///
/// `depth` is the depth of the frame the call would start, and `caller_remaining` the regular gas
/// the calling frame has left once the call deducted the gas it forwards, the part gas detention
/// withholds included: [`Gas::remaining`](revm::interpreter::Gas::remaining). A transaction that
/// calls the contract directly has no calling frame, and passes zero.
///
/// # What `remainingComputeGas()` answers
///
/// The compute the caller could still spend: the regular gas a regular charge of the calling frame
/// could draw once the call returns. That is the lesser of two figures, both read when the call
/// reaches the interceptor — after the `CALL` or `STATICCALL` charged its own costs, and with the
/// gas it forwarded counted back:
///
/// - the caller's own regular gas, `caller_remaining` plus what the call forwarded. The answer
///   gives the forward back untouched, so this is what the caller holds when the call returns;
/// - the compute gas detention leaves the transaction, once a read of volatile data set a limit:
///   the limit less the transaction's compute at the call, the forward not counted
///   ([`Detention::allowance`](crate::Detention)). A transaction that read nothing, or that
///   detention does not hold, has no such limit.
///
/// Detention holds a frame's spendable gas at the lesser of the two at all times, so the answer is
/// the caller's spendable part before the forward. For a transaction that calls the contract
/// directly, the caller's own gas is the regular gas its frame was given.
///
/// The figure is taken before the forward, as the legacy engine's was: its compute gas never
/// counted the gas a call forwarded, so its answer did not fall by the forward either. Taken after
/// it, a caller that forwards what `GAS` reports would hear back one sixty-fourth of its gas.
///
/// It is regular gas only, capped at the execution cap: the state-gas reservoir is not compute,
/// and a transaction above the cap hears the cap's share at most. It is not a promise of compute
/// either: state or history gas the caller later pays out of its regular gas comes out of the
/// same figure. A frame a value call started holds its stipend in its own regular gas, so an
/// undetained one hears it counted.
///
/// The method reads and takes no value.
pub(crate) fn intercept<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    inputs: &CallInputs,
    depth: usize,
    caller_remaining: u64,
) -> Option<FrameResult> {
    let selector = peek_selector(&inputs.input, ctx)?;
    // Selector-only admission: the four bytes decide, whatever follows them.
    if selector != IMegaLimitControl::remainingComputeGasCall::SELECTOR {
        return None;
    }
    if let Some(rejected) = reject_non_zero_transfer(inputs) {
        return Some(rejected);
    }
    let own = caller_remaining.saturating_add(inputs.gas_limit);
    let remaining = ctx
        .detention
        .allowance(depth, inputs.gas_limit)
        .map_or(own, |allowance| own.min(allowance));
    let output = IMegaLimitControl::remainingComputeGasCall::abi_encode_returns(&remaining);
    Some(synthetic_call_result(inputs, InstructionResult::Return, Bytes::from(output)))
}
