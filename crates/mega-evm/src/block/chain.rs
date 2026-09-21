//! Per-chain activation of the `MegaETH` hardforks of the Satin engine.
//!
//! This is the single place that records when each [`MegaHardfork`] activates on the known
//! `MegaETH` chains. The node chainspecs decide the schedule; this table mirrors it for tools
//! that replay a real chain, and [`hardfork_schedule`] turns a chain ID into the schedule block
//! execution reads.

use alloy_hardforks::ForkCondition;
use alloy_primitives::BlockTimestamp;

use crate::{system::SequencerRegistryConfig, MegaHardfork, MegaHardforkConfig, MegaSpecId};

/// `MegaETH` mainnet chain ID.
pub const MAINNET_CHAIN_ID: u64 = 4326;

/// `MegaETH` testnet v2 chain ID.
pub const TESTNET_CHAIN_ID: u64 = 6343;

/// The rung an unknown chain runs, from genesis.
///
/// Pinned, not taken from [`MegaSpecId::default`]: an unknown chain runs this spec from block
/// zero with no fork boundary, so introducing a spec must leave it alone — registering a fork
/// above the pin would rewrite what history such a chain has already produced means. Raising the
/// pin is a decision of its own, made by editing this constant.
pub const FALLBACK_RUNG: MegaSpecId = MegaSpecId::SATIN;

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

/// The schedule an unknown chain runs: every fork up to [`FALLBACK_RUNG`], active at genesis.
///
/// Satin requires a [`SequencerRegistryConfig`]. An unknown chain has no published roles, so
/// every role is seeded with [`MEGA_SYSTEM_ADDRESS`], `_initialFromBlock` is zero, and
/// `_minRotationDelay` is [`crate::system::PLACEHOLDER_MIN_ROTATION_DELAY`]. The placeholder
/// only matters when bootstrapping a fresh registry: on a chain whose registry is already
/// deployed, the live roles are read from storage.
pub fn all_activated_hardforks() -> MegaHardforkConfig {
    let mut config = MegaHardforkConfig::new();
    for fork in MegaHardfork::VARIANTS {
        let condition = if fork.spec_id() <= FALLBACK_RUNG {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        };
        config.insert(*fork, condition);
    }
    config.with_params(SequencerRegistryConfig::placeholder())
}

/// The hardfork schedule of `chain_id`: a known chain's table, or the unknown-chain fallback.
pub fn hardfork_schedule(chain_id: u64) -> MegaHardforkConfig {
    chain_activation(chain_id).map_or_else(all_activated_hardforks, |a| a.hardforks())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        system::{MEGA_SYSTEM_ADDRESS, PLACEHOLDER_MIN_ROTATION_DELAY},
        MegaHardforks,
    };

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
        let hf = ChainActivation { chain_id: MAINNET_CHAIN_ID, satin: Some(1_800_000_000) }
            .hardforks()
            .with_params(SequencerRegistryConfig::placeholder());

        assert_eq!(hf.spec_id(1_799_999_999), None);
        assert_eq!(hf.spec_id(1_800_000_000), Some(MegaSpecId::SATIN));
        assert_eq!(hf.validate_schedule(), Ok(()));
    }

    #[test]
    fn test_schedule_dispatch_by_chain_id() {
        assert_eq!(hardfork_schedule(MAINNET_CHAIN_ID).spec_id(0), None);
        assert_eq!(hardfork_schedule(TESTNET_CHAIN_ID).spec_id(0), None);
        // Unknown chain: every fork up to the pinned rung, active at genesis.
        assert_eq!(hardfork_schedule(1).spec_id(0), Some(FALLBACK_RUNG));
    }

    /// The fallback schedule is one a chain can run: it carries placeholder registry params, so
    /// an unknown chain ID starts rather than failing at its first block.
    #[test]
    fn test_unknown_chain_fallback_is_a_runnable_schedule() {
        let hf = hardfork_schedule(999_999);

        assert_eq!(hf.spec_id(0), Some(FALLBACK_RUNG));
        assert_eq!(hf.validate_schedule(), Ok(()));
        let params = hf
            .fork_params::<SequencerRegistryConfig>()
            .expect("fallback schedule must carry a SequencerRegistryConfig");
        assert_eq!(params.initial_system_address, MEGA_SYSTEM_ADDRESS);
        assert_eq!(params.initial_sequencer, MEGA_SYSTEM_ADDRESS);
        assert_eq!(params.initial_admin, MEGA_SYSTEM_ADDRESS);
        assert_eq!(params.initial_from_block, 0);
        assert_eq!(params.min_rotation_delay, PLACEHOLDER_MIN_ROTATION_DELAY);
        assert_ne!(params.min_rotation_delay, 0);
    }

    /// The fallback rung is pinned, not inherited from [`MegaSpecId::default`].
    ///
    /// Unknown chains run their rung from genesis, so it is their semantics from block zero with
    /// no fork boundary. Introducing a spec must therefore leave them alone. What this pins is
    /// drift: the fallback silently following `MegaSpecId::default` again, and the rung advancing
    /// without [`FALLBACK_RUNG`] being edited in the same change.
    #[test]
    fn test_unknown_chain_fallback_pins_its_rung() {
        let hf = all_activated_hardforks();
        const RUNG: MegaSpecId = MegaSpecId::SATIN;

        assert_eq!(FALLBACK_RUNG, RUNG, "the pin moves only by editing it here too");
        assert_eq!(hf.spec_id(0), Some(RUNG), "the rung applies from genesis");
        assert_eq!(
            hf.spec_id(u64::MAX),
            Some(RUNG),
            "and is terminal — no later fork is registered"
        );

        for fork in MegaHardfork::VARIANTS {
            let registered = hf.mega_fork_activation(*fork) != ForkCondition::Never;
            assert_eq!(
                registered,
                fork.spec_id() <= RUNG,
                "{fork:?} registration must follow the pinned rung, not MegaSpecId::default()"
            );
        }
    }
}
