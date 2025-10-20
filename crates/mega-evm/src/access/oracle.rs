//! This module implements the EVM logic for `MegaETH`'s sequencer oracle service.
//!
//! The main feature is that, whenever oracle data is read during EVM execution, the EVM will
//! limit the remaining gas in parent (nested) calls to a small amount of gas, forcing the
//! transaction to finish execution soon. Similarly, block environment access (TIMESTAMP, NUMBER,
//! etc.) immediately limits gas. These restrictions are necessary to prevent `DoS` attacks on the
//! EVM.

use alloy_primitives::Address;

use crate::ORACLE_CONTRACT_ADDRESS;

/// The address of the `MegaETH`'s oracle contract.
pub const MEGA_ORACLE_CONTRACT_ADDRESS: Address = ORACLE_CONTRACT_ADDRESS;

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
    pub fn check_and_mark_oracle_access(&mut self, address: &Address) -> bool {
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
