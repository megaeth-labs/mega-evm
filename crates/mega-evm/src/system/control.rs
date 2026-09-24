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
/// and the deployed bytecode runs. `depth` is the depth of the frame the call would start.
///
/// # The switch
///
/// The three methods steer and read gas detention's switch ([`Detention`](crate::Detention)),
/// for the frame that made the call — the caller, one level above the frame the call would start:
///
/// - `disableVolatileDataAccess()` switches volatile-data access off for the caller and every frame
///   below it. A switch already off from a frame above stays as it is.
/// - `enableVolatileDataAccess()` switches it back on, and reverts with `DisabledByParent()` when a
///   frame above the caller switched it off: a frame cannot lift a restriction its caller placed.
///   Switching on what is not off succeeds and changes nothing.
/// - `isVolatileDataAccessDisabled()` answers whether the switch is off for the caller.
///
/// While the switch is off, the Host refuses every volatile read of the caller's subtree and the
/// reading frame reverts with `VolatileDataAccessDisabled`. The caller reads the switch as it
/// resumes after the call, and it turns back on when the frame that switched it off returns,
/// whatever it returns with, so a sibling called afterwards is not restricted.
///
/// A transaction that calls the contract directly has no frame of its own above the one the call
/// starts: nothing runs after the answer, so disabling changes nothing, enabling succeeds and the
/// query answers `false`.
///
/// All three take no value: they read or steer execution, and the contract holds no balance. A
/// `STATICCALL` reaches them too, because the switch is not state: it lives for the transaction
/// and leaves nothing behind.
pub(crate) fn intercept<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inputs: &CallInputs,
    depth: usize,
) -> Option<FrameResult> {
    let selector = peek_selector(&inputs.input, ctx)?;
    // Selector-only admission: the four bytes decide, whatever follows them.
    let method = if selector == IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR {
        Method::Disable
    } else if selector == IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR {
        Method::Enable
    } else if selector == IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR {
        Method::IsDisabled
    } else {
        return None;
    };
    if let Some(rejected) = reject_non_zero_transfer(inputs) {
        return Some(rejected);
    }
    let caller = depth.checked_sub(1);
    let detention = &mut ctx.detention;
    let (result, output) = match method {
        Method::Disable => {
            if let Some(caller) = caller {
                detention.disable_access(caller);
            }
            (InstructionResult::Return, Bytes::new())
        }
        Method::Enable => {
            if caller.is_none_or(|caller| detention.enable_access(caller)) {
                (InstructionResult::Return, Bytes::new())
            } else {
                (InstructionResult::Revert, Bytes::from_static(&DISABLED_BY_PARENT_REVERT_DATA))
            }
        }
        Method::IsDisabled => {
            let disabled = caller.is_some_and(|caller| detention.is_access_disabled(caller));
            let output =
                IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(&disabled);
            (InstructionResult::Return, Bytes::from(output))
        }
    };
    Some(synthetic_call_result(inputs, result, output))
}

/// The three methods `MegaAccessControl` intercepts.
#[derive(Clone, Copy, Debug)]
enum Method {
    /// `disableVolatileDataAccess()`.
    Disable,
    /// `enableVolatileDataAccess()`.
    Enable,
    /// `isVolatileDataAccessDisabled()`.
    IsDisabled,
}
