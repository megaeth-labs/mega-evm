//! The `MegaLimitControl` system contract.
//!
//! It answers what the running call has left of the resources `MegaETH` meters; today that is
//! `remainingComputeGas()`. Its methods are intercepted: the deployed bytecode only carries the
//! ABI and reverts with `NotIntercepted()`. Its Solidity source is
//! `crates/system-contracts/contracts/MegaLimitControl.sol`.

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
/// # What `remainingComputeGas` answers today
///
/// The compute ledger belongs to compute gas, which is what will make this the compute gas the
/// transaction has left. Until then the interceptor answers what the common execution layer
/// knows: the regular gas the intercepted call was forwarded, which is the regular gas its frame
/// could still spend. Regular gas is capped at the execution cap, so the answer never counts the
/// state-gas reservoir.
///
/// The method reads and takes no value.
pub(crate) fn intercept<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    inputs: &CallInputs,
) -> Option<FrameResult> {
    let selector = peek_selector(&inputs.input, ctx)?;
    // Selector-only admission: the four bytes decide, whatever follows them.
    if selector != IMegaLimitControl::remainingComputeGasCall::SELECTOR {
        return None;
    }
    if let Some(rejected) = reject_non_zero_transfer(inputs) {
        return Some(rejected);
    }
    let output = IMegaLimitControl::remainingComputeGasCall::abi_encode_returns(&inputs.gas_limit);
    Some(synthetic_call_result(inputs, InstructionResult::Return, Bytes::from(output)))
}
