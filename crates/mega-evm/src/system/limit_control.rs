//! The `MegaLimitControl` system contract.
//!
//! It answers what the running call has left of the resources `MegaETH` meters; today that is
//! `remainingComputeGas()`. Its methods are intercepted: the deployed bytecode only carries the
//! ABI and reverts with `NotIntercepted()`. Its Solidity source is
//! `crates/system-contracts/contracts/MegaLimitControl.sol`.

use alloy_primitives::{address, Address};

/// The address of the `MegaLimitControl` system contract.
pub const LIMIT_CONTROL_ADDRESS: Address = address!("0x6342000000000000000000000000000000000005");

/// The code of the `MegaLimitControl` contract.
pub use mega_system_contracts::limit_control::LATEST_CODE as LIMIT_CONTROL_CODE;

/// The code hash of the `MegaLimitControl` contract.
pub use mega_system_contracts::limit_control::LATEST_CODE_HASH as LIMIT_CONTROL_CODE_HASH;

pub use mega_system_contracts::limit_control::IMegaLimitControl;
