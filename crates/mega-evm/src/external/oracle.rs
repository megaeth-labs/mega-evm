//! Oracle environment trait and implementations.

use core::fmt::Debug;

use alloy_primitives::{Address, Bytes, B256, U256};
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

    /// Receives hints emitted on-chain by the oracle contract via logs. A hint is a signal sent
    /// from on-chain to the oracle service backend (on the sequencer).
    ///
    /// Hint logs have exactly three topics:
    /// - `topic[0]`: event signature hash (used by the oracle contract)
    /// - `topic[1]`: the sender address who called `sendHint` (passed to this method as `from`)
    /// - `topic[2]`: user-defined hint topic (passed to this method as `topic`)
    ///
    /// The `from` address is useful for off-chain access control, as the `msg.sender` cannot be
    /// faked. On-chain access control can be enforced in a periphery contract which directly
    /// calls `sendHint`.
    ///
    /// The order of hinting ([`Self::on_hint`]) and oracle reading ([`Self::get_oracle_storage`])
    /// is guaranteed preserved, i.e., if the on-chain transaction emits a hint log first and then
    /// tries to read oracle data, `on_hint` is guaranteed to be called before `get_oracle_storage`.
    ///
    /// One example application is telling the off-chain oracle service which data needs to be
    /// fetched before it provides any oracle data. Handling hints is completely optional for the
    /// oracle service backend.
    fn on_hint(&self, _from: Address, _topic: B256, _data: Bytes) {}
}

impl OracleEnv for EmptyExternalEnv {
    fn get_oracle_storage(&self, _slot: U256) -> Option<U256> {
        None
    }
}
