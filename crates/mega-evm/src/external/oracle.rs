//! Oracle environment trait and implementations.

use core::fmt::Debug;

use alloy_primitives::U256;
use auto_impl::auto_impl;

use super::DefaultExternalEnvs;

/// An oracle service that provides external information to the EVM. This trait provides a mechanism
/// for the EVM to query storage slots from the `MegaETH` oracle contract.
///
/// Typically, one implementation of this trait can be a reader of the underlying blockchain
/// database of `MegaETH` to provide deterministic oracle data during EVM execution.
#[auto_impl(&, Box, Arc)]
pub trait OracleEnv: Debug + Send + Sync + Unpin {
    /// The error type for the oracle.
    type Error;

    /// Gets the storage value at a specific slot of the `MegaETH` oracle contract.
    ///
    /// # Arguments
    ///
    /// * `slot` - The storage slot to query
    ///
    /// # Returns
    ///
    /// The storage value at the given slot of the oracle contract.
    fn get_oracle_storage(&self, slot: U256) -> Result<U256, Self::Error>;
}

impl<Error: Unpin + Send + Sync + Clone + 'static> OracleEnv for DefaultExternalEnvs<Error> {
    type Error = Error;

    fn get_oracle_storage(&self, slot: U256) -> Result<U256, Self::Error> {
        // Return the value from storage, or zero if not set
        Ok(self.oracle_storage.read().expect("RwLock poisoned").get(&slot).copied().unwrap_or(U256::ZERO))
    }
}

impl<Error: Unpin + Send + Sync + Clone + 'static> DefaultExternalEnvs<Error> {
    /// Sets an oracle storage slot to a specific value for testing purposes.
    ///
    /// # Arguments
    ///
    /// * `slot` - The storage slot to set
    /// * `value` - The value to set at the given slot
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_oracle_storage(self, slot: U256, value: U256) -> Self {
        self.oracle_storage.write().expect("RwLock poisoned").insert(slot, value);
        self
    }

    /// Clears all oracle storage values.
    pub fn clear_oracle_storage(&self) {
        self.oracle_storage.write().expect("RwLock poisoned").clear();
    }
}
