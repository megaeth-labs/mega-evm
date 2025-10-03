//! This module implements the EVM logic for MegaETH's sequencer oracle service.
//!
//! The main feature is that, whenever an oracle data is read during the EVM execution, the EVM will
//! immediately limit the remaining gas in all message calls to a small amount of gas, forcing the
//! transaction to finish execution soon. These restrictions are necessary to prevent DoS attacks on
//! EVM.

use alloy_primitives::{address, Address};

/// The address of the MegaETH's oracle contract.
pub const MEGA_ORACLE_CONTRACT_ADDRESS: Address =
    address!("0x4200000000000000000000000000000000000101");

/// A tracker for oracle data access.
#[derive(Default, Debug, Clone)]
pub struct OracleAccessTracker {
    accessed: bool,
}

impl OracleAccessTracker {
    /// Creates a new oracle access tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks if the given address is the oracle contract address. If so, it will mark that the
    /// oracle has been accessed and returns true.
    pub fn check_oracle_access(&mut self, address: &Address) -> bool {
        if address == &MEGA_ORACLE_CONTRACT_ADDRESS {
            self.accessed = true;
            true
        } else {
            false
        }
    }

    /// Checks if the oracle has been accessed.
    pub fn has_accessed(&self) -> bool {
        self.accessed
    }

    /// Resets the oracle access tracker.
    pub fn reset(&mut self) {
        self.accessed = false;
    }
}
