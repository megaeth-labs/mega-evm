//! The `MegaAccessControl` system contract.
//!
//! It switches volatile-data access off and on for the calling frame and the frames below it,
//! and reports whether it is off. Its methods are intercepted: the deployed bytecode only
//! carries the ABI and reverts with `NotIntercepted()`. Its Solidity source is
//! `crates/system-contracts/contracts/MegaAccessControl.sol`.

use alloy_primitives::{address, Address};
use alloy_sol_types::SolError;

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
