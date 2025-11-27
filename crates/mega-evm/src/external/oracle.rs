//! Oracle environment trait and implementations.

use core::fmt::Debug;

use alloy_primitives::U256;
use auto_impl::auto_impl;

use crate::EmptyExternalEnv;

/// An oracle service that provides external information to the EVM. This trait provides a mechanism
/// for the EVM to query storage slots from the `MegaETH` oracle contract.
///
/// Typically, one implementation of this trait can be a reader of the underlying blockchain
/// database of `MegaETH` to provide deterministic oracle data during EVM execution.
#[auto_impl(&, Box, Arc)]
pub trait OracleEnv: Debug + Unpin {
    /// Gets the storage value at a specific slot of the `MegaETH` oracle contract.
    ///
    /// # Arguments
    ///
    /// * `slot` - The storage slot to query
    ///
    /// # Returns
    ///
    /// The storage value at the given slot of the oracle contract. If the oracle does not provide a
    /// value, the result will be `None`.
    fn get_oracle_storage(&self, slot: U256) -> Option<U256>;
}

impl OracleEnv for EmptyExternalEnv {
    fn get_oracle_storage(&self, _slot: U256) -> Option<U256> {
        None
    }
}
