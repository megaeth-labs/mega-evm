//! The oracle system contract for the `MegaETH` EVM.

use alloy_primitives::{address, Address};

/// The address of the oracle system contract, which delegates to the implementation contract at
/// `ORACLE_CONTRACT_IMPLEMENTATION_ADDRESS`.
pub const ORACLE_CONTRACT_ADDRESS: Address = address!("0x6342000000000000000000000000000000000001");

/// The address of the oracle system contract implementation.
pub const ORACLE_CONTRACT_IMPLEMENTATION_ADDRESS: Address =
    address!("0x6342000019670000000000000000000000000002");
