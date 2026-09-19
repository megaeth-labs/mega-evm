//! Per-chain activation of the `MegaETH` hardforks of the Satin engine.
//!
//! This is the single place that records when each [`MegaHardfork`](crate::MegaHardfork)
//! activates on the known `MegaETH` chains. The node chainspecs decide the schedule; this table
//! mirrors it for tools that replay a real chain.

use alloy_primitives::BlockTimestamp;

/// `MegaETH` mainnet chain ID.
pub const MAINNET_CHAIN_ID: u64 = 4326;

/// `MegaETH` testnet v2 chain ID.
pub const TESTNET_CHAIN_ID: u64 = 6343;

/// Activation timestamps of the Satin-engine hardforks on one chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainActivation {
    /// The chain the timestamps belong to.
    pub chain_id: u64,
    /// When [`MegaHardfork::Satin`](crate::MegaHardfork::Satin) activates; `None` while it is
    /// not scheduled.
    pub satin: Option<BlockTimestamp>,
}

/// Activation table of the known `MegaETH` chains.
pub const CHAIN_ACTIVATIONS: [ChainActivation; 2] = [
    ChainActivation { chain_id: MAINNET_CHAIN_ID, satin: None },
    ChainActivation { chain_id: TESTNET_CHAIN_ID, satin: None },
];

/// The activation table of a known chain, or `None` for any other chain.
pub fn chain_activation(chain_id: u64) -> Option<ChainActivation> {
    CHAIN_ACTIVATIONS.iter().find(|activation| activation.chain_id == chain_id).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_chains_have_satin_unscheduled() {
        assert_eq!(MAINNET_CHAIN_ID, 4326);
        assert_eq!(TESTNET_CHAIN_ID, 6343);
        for chain_id in [MAINNET_CHAIN_ID, TESTNET_CHAIN_ID] {
            let activation = chain_activation(chain_id).expect("known chain");
            assert_eq!(activation.chain_id, chain_id);
            assert_eq!(activation.satin, None, "Satin has no activation timestamp yet");
        }
        assert_eq!(chain_activation(1), None);
    }
}
