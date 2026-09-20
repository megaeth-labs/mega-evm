//! The Oracle system contract.
//!
//! The Oracle holds the protocol's key-value storage (`getSlot` / `setSlot`) and is the surface
//! through which a contract reaches the node's oracle service: `sendHint` carries a message to
//! the service backend. Its Solidity source is
//! `crates/system-contracts/contracts/Oracle.sol`.

use alloy_primitives::{address, Address};

/// The address of the Oracle system contract.
pub const ORACLE_CONTRACT_ADDRESS: Address = address!("0x6342000000000000000000000000000000000001");

/// The code of the Oracle contract.
pub use mega_system_contracts::oracle::LATEST_CODE as ORACLE_CONTRACT_CODE;

/// The code hash of the Oracle contract.
pub use mega_system_contracts::oracle::LATEST_CODE_HASH as ORACLE_CONTRACT_CODE_HASH;

pub use mega_system_contracts::oracle::IOracle;
