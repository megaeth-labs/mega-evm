//! Per-chain activation of the `MegaETH` hardforks of the Satin engine.
//!
//! This is the single place that records when each [`MegaHardfork`] activates on the known
//! `MegaETH` chains. The node chainspecs decide the schedule; this table mirrors it for tools
//! that replay a real chain, and [`hardfork_schedule`] turns a known chain's ID into the schedule
//! block execution reads. A chain this table does not know has no schedule here: Satin runs on a
//! chain that ran the legacy forks, and whoever runs another names its schedule.

use alloy_hardforks::ForkCondition;
use alloy_primitives::BlockTimestamp;

use crate::{MegaHardfork, MegaHardforkConfig};

/// `MegaETH` mainnet chain ID.
pub const MAINNET_CHAIN_ID: u64 = 4326;

/// `MegaETH` testnet v2 chain ID.
pub const TESTNET_CHAIN_ID: u64 = 6343;

/// Activation timestamps of the Satin-engine hardforks on one chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainActivation {
    /// The chain the timestamps belong to.
    pub chain_id: u64,
    /// When [`MegaHardfork::Satin`] activates; `None` while it is not scheduled.
    pub satin: Option<BlockTimestamp>,
}

impl ChainActivation {
    /// The activation condition of `fork` on this chain.
    pub const fn condition(&self, fork: MegaHardfork) -> ForkCondition {
        let timestamp = match fork {
            MegaHardfork::Satin => self.satin,
        };
        match timestamp {
            Some(timestamp) => ForkCondition::Timestamp(timestamp),
            None => ForkCondition::Never,
        }
    }

    /// The hardfork schedule of this chain.
    pub fn hardforks(&self) -> MegaHardforkConfig {
        let mut config = MegaHardforkConfig::new();
        for fork in MegaHardfork::VARIANTS {
            config.insert(*fork, self.condition(*fork));
        }
        config
    }
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

/// The hardfork schedule of `MegaETH` mainnet.
pub fn mainnet_hardforks() -> MegaHardforkConfig {
    chain_activation(MAINNET_CHAIN_ID).expect("mainnet is in the activation table").hardforks()
}

/// The hardfork schedule of `MegaETH` testnet.
pub fn testnet_hardforks() -> MegaHardforkConfig {
    chain_activation(TESTNET_CHAIN_ID).expect("testnet is in the activation table").hardforks()
}

/// The hardfork schedule of `chain_id`, a known chain's table; `None` for any other chain.
pub fn hardfork_schedule(chain_id: u64) -> Option<MegaHardforkConfig> {
    chain_activation(chain_id).map(|activation| activation.hardforks())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MegaHardforks, MegaSpecId};

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

    #[test]
    fn test_mainnet_schedule_resolves_specs_by_timestamp() {
        let hf = mainnet_hardforks();
        assert_eq!(hf.mega_fork_activation(MegaHardfork::Satin), ForkCondition::Never);
        assert_eq!(hf.spec_id(0), None, "Satin is not scheduled on mainnet yet");
        assert_eq!(hf.spec_id(u64::MAX), None);
        assert_eq!(hf.validate_schedule(), Ok(()));
    }

    #[test]
    fn test_testnet_schedule_resolves_specs_by_timestamp() {
        let hf = testnet_hardforks();
        assert_eq!(hf.mega_fork_activation(MegaHardfork::Satin), ForkCondition::Never);
        assert_eq!(hf.spec_id(0), None, "Satin is not scheduled on testnet yet");
        assert_eq!(hf.spec_id(u64::MAX), None);
        assert_eq!(hf.validate_schedule(), Ok(()));
    }

    /// A scheduled chain resolves the spec its table gives, at the timestamp the table gives.
    #[test]
    fn test_a_scheduled_activation_resolves_at_its_timestamp() {
        let hf =
            ChainActivation { chain_id: MAINNET_CHAIN_ID, satin: Some(1_800_000_000) }.hardforks();

        assert_eq!(hf.spec_id(1_799_999_999), None);
        assert_eq!(hf.spec_id(1_800_000_000), Some(MegaSpecId::SATIN));
        assert_eq!(hf.validate_schedule(), Ok(()));
    }

    /// A known chain's schedule is its table's; another chain has none: the engine names no
    /// schedule for a chain it does not know.
    #[test]
    fn test_schedule_dispatch_by_chain_id() {
        for activation in CHAIN_ACTIVATIONS {
            let schedule = hardfork_schedule(activation.chain_id).expect("a known chain");
            for fork in MegaHardfork::VARIANTS {
                assert_eq!(schedule.get(*fork), Some(&activation.condition(*fork)));
            }
        }
        for chain_id in [0, 1, 6342, 999_999] {
            assert!(hardfork_schedule(chain_id).is_none(), "chain {chain_id} is unknown");
        }
    }
}
