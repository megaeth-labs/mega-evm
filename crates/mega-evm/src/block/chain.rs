//! Per-chain activation of the `MegaETH` hardforks of the Satin engine.
//!
//! This is the single place that records when each [`MegaHardfork`] activates on the known
//! `MegaETH` chains. The node chainspecs decide the schedule; this table mirrors it for tools
//! that replay a real chain, and [`hardfork_schedule`] turns a known chain's ID into the schedule
//! block execution reads. A chain this table does not know has no schedule here: Satin runs on a
//! chain that ran the legacy forks, and whoever runs another names its schedule.

use alloy_hardforks::ForkCondition;
use alloy_primitives::{address, BlockTimestamp};

use crate::{system::SequencerRegistryBootstrap, MegaHardfork, MegaHardforkConfig};

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
    /// The `SequencerRegistry`'s bootstrap values the chain's configuration carries: on a live
    /// network, the roles Rex5 seeded its registry with and the minimum rotation delay Rex6 gave
    /// it; `None` for a configuration that carries none.
    pub sequencer_registry_bootstrap: Option<SequencerRegistryBootstrap>,
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

    /// The hardfork schedule of this chain, carrying its bootstrap values.
    pub fn hardforks(&self) -> MegaHardforkConfig {
        let mut config = MegaHardforkConfig::new();
        if let Some(bootstrap) = self.sequencer_registry_bootstrap {
            config = config.with_sequencer_registry_bootstrap(bootstrap);
        }
        for fork in MegaHardfork::VARIANTS {
            config.insert(*fork, self.condition(*fork));
        }
        config
    }
}

/// The minimum rotation delay, in blocks, both live networks' configurations carry and Rex6 seeded
/// their registries with.
const MIN_ROTATION_DELAY: u64 = 21_600;

/// Activation table of the known `MegaETH` chains.
pub const CHAIN_ACTIVATIONS: [ChainActivation; 2] = [
    ChainActivation {
        chain_id: MAINNET_CHAIN_ID,
        satin: None,
        sequencer_registry_bootstrap: Some(SequencerRegistryBootstrap::new(
            address!("0x7a49197dd1ebb8d38c67e4eb7626af6ade432445"),
            address!("0x92e0e0b15e3e99b32c9ed9ad284f939553c7b7d6"),
            MIN_ROTATION_DELAY,
        )),
    },
    ChainActivation {
        chain_id: TESTNET_CHAIN_ID,
        satin: None,
        sequencer_registry_bootstrap: Some(SequencerRegistryBootstrap::new(
            address!("0xB8DB54eBA7Ae650d14F362de461516a4FF1551FC"),
            address!("0x1d9BD232C44B39341e670B735c7F423c40426b34"),
            MIN_ROTATION_DELAY,
        )),
    },
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
        let mainnet = chain_activation(MAINNET_CHAIN_ID).expect("a known chain");
        let hf = ChainActivation { satin: Some(1_800_000_000), ..mainnet }.hardforks();

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
            let bootstrap = activation.sequencer_registry_bootstrap.expect("a live network's");
            assert_eq!(
                schedule.sequencer_registry_bootstrap(),
                Some(bootstrap),
                "the schedule carries the chain's bootstrap values"
            );
            assert!(bootstrap.is_usable());
        }
        for chain_id in [0, 1, 6342, 999_999] {
            assert!(hardfork_schedule(chain_id).is_none(), "chain {chain_id} is unknown");
        }
    }
}
