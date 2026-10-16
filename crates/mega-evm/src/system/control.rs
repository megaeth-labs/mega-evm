//! The `MegaAccessControl` system contract.
//!
//! It switches volatile-data access off and on for the calling frame and the frames below it,
//! and reports whether it is off. Its methods are intercepted: the deployed bytecode only
//! carries the ABI and reverts with `NotIntercepted()`. Its Solidity source is
//! `crates/system-contracts/contracts/MegaAccessControl.sol`.

use alloy_evm::Database;
use alloy_primitives::{address, Address, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::{
    handler::FrameResult,
    interpreter::{CallInputs, InstructionResult},
};

use crate::{
    synthetic_call_result,
    system::intercept::{peek_selector, reject_non_zero_transfer},
    ExternalEnvTypes, MegaContext,
};

/// The address of the `MegaAccessControl` system contract.
pub const ACCESS_CONTROL_ADDRESS: Address = address!("0x6342000000000000000000000000000000000004");

/// The code of the `MegaAccessControl` contract.
pub use mega_system_contracts::access_control::LATEST_CODE as ACCESS_CONTROL_CODE;

/// The code hash of the `MegaAccessControl` contract.
pub use mega_system_contracts::access_control::LATEST_CODE_HASH as ACCESS_CONTROL_CODE_HASH;

pub use mega_system_contracts::access_control::IMegaAccessControl;
pub use IMegaAccessControl::VolatileDataAccessType;

/// The revert data of `DisabledByParent()`: the answer to a frame that tries to switch
/// volatile-data access back on after a frame above it switched it off.
pub const DISABLED_BY_PARENT_REVERT_DATA: [u8; 4] = IMegaAccessControl::DisabledByParent::SELECTOR;

/// The selector of `VolatileDataAccessDisabled(uint8)`, the error a volatile read reverts with
/// while access is off.
pub const VOLATILE_DATA_ACCESS_DISABLED_SELECTOR: [u8; 4] =
    IMegaAccessControl::VolatileDataAccessDisabled::SELECTOR;

/// Answers a call to `MegaAccessControl`, or `None` when the selector is not one of its three
/// and the deployed bytecode runs.
///
/// # What the three methods do today
///
/// The switch itself belongs to detention, which brings the tracker that remembers a frame's
/// answer and the volatile reads that consult it. Until then the interceptor is the dispatch
/// and the value policy: `disableVolatileDataAccess` and `enableVolatileDataAccess` succeed and
/// change nothing, and `isVolatileDataAccessDisabled` answers `false`, which is what the common
/// execution layer knows — no frame has ever switched access off. `DisabledByParent()`, the
/// answer to a frame that re-enables what a frame above it disabled, is the tracker's to give.
///
/// All three take no value: they read or steer execution, and the contract holds no balance.
pub(crate) fn intercept<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    inputs: &CallInputs,
) -> Option<FrameResult> {
    let selector = peek_selector(&inputs.input, ctx)?;
    // Selector-only admission: the four bytes decide, whatever follows them.
    let output = if selector == IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR ||
        selector == IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR
    {
        Bytes::new()
    } else if selector == IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR {
        Bytes::from(IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(
            &false,
        ))
    } else {
        return None;
    };
    if let Some(rejected) = reject_non_zero_transfer(inputs) {
        return Some(rejected);
    }
    Some(synthetic_call_result(inputs, InstructionResult::Return, output))
}
