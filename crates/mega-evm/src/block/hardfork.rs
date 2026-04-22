#[cfg(not(feature = "std"))]
use alloc as std;

use alloy_hardforks::{hardfork, EthereumHardfork, EthereumHardforks, ForkCondition, Hardfork};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use alloy_primitives::{BlockNumber, BlockTimestamp, U256};
use auto_impl::auto_impl;
use std::{boxed::Box, vec::Vec};

use crate::{system::SequencerRegistryConfig, MegaSpecId};

hardfork! {
    /// The name of MegaETH hardforks. It is expected to mix with [`EthereumHardfork`] and
    /// [`OpHardfork`].
    #[derive(serde::Serialize, serde::Deserialize)]
    MegaHardfork {
        /// The first hardfork.
        MiniRex,
        /// The first patch hardfork to MiniRex.
        MiniRex1,
        /// The second patch hardfork to MiniRex.
        MiniRex2,
        /// The fourth hardfork.
        Rex,
        /// The fifth hardfork (first patch to Rex).
        Rex1,
        /// The sixth hardfork (second patch to Rex).
        Rex2,
        /// The seventh hardfork (third patch to Rex).
        Rex3,
        /// The eighth hardfork (fourth patch to Rex).
        Rex4,
        /// The ninth hardfork (fifth patch to Rex).
        Rex5,
    }
}

impl MegaHardfork {
    /// Gets the `MegaSpecId` associated with this hardfork.
    #[allow(clippy::match_same_arms)]
    pub fn spec_id(&self) -> MegaSpecId {
        // Note: MiniRex1 and MiniRex2 are patch hardforks that intentionally reverted to
        // previously released specs rather than introducing new EVM semantics.
        match self {
            Self::MiniRex => MegaSpecId::MINI_REX,
            Self::MiniRex1 => MegaSpecId::EQUIVALENCE,
            Self::MiniRex2 => MegaSpecId::MINI_REX,
            Self::Rex => MegaSpecId::REX,
            Self::Rex1 => MegaSpecId::REX1,
            Self::Rex2 => MegaSpecId::REX2,
            Self::Rex3 => MegaSpecId::REX3,
            Self::Rex4 => MegaSpecId::REX4,
            Self::Rex5 => MegaSpecId::REX5,
        }
    }
}

/// Extends [`OpHardforks`] with `MegaETH` helper methods.
#[auto_impl(&, Box, Arc)]
pub trait MegaHardforks: OpHardforks {
    /// Retrieves [`ForkCondition`] by a [`MegaHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition;

    /// Returns the bootstrap configuration for `SequencerRegistry`.
    fn sequencer_registry_config(&self) -> SequencerRegistryConfig {
        SequencerRegistryConfig::default()
    }

    /// Returns `true` if the given [`MegaHardfork`] is the hardfork to be activated at the
    /// given timestamp. One special case is that if the current block is the first block of the
    /// chain and it activates the hardfork, we should return `true`.
    ///
    /// If the block is the first block of the hardfork, some hardfork
    /// initialization logic should be applied. This helper method is used for this purpose.
    fn first_hardfork_block(
        &self,
        fork: MegaHardfork,
        parent_timestamp: BlockTimestamp,
        current_number_and_timestamp: (BlockNumber, BlockTimestamp),
    ) -> bool {
        let (current_number, current_timestamp) = current_number_and_timestamp;
        self.mega_fork_activation(fork).active_at_timestamp(current_timestamp) &&
            (current_number == 1 ||
                !self.mega_fork_activation(fork).active_at_timestamp(parent_timestamp))
    }

    /// Gets the latest `MegaHardfork` that is active at the given timestamp. If no `MegaHardfork`
    /// is active at the given timestamp, returns `None`.
    fn hardfork(&self, timestamp: u64) -> Option<MegaHardfork> {
        if self.is_rex_5_active_at_timestamp(timestamp) {
            Some(MegaHardfork::Rex5)
        } else if self.is_rex_4_active_at_timestamp(timestamp) {
            Some(MegaHardfork::Rex4)
        } else if self.is_rex_3_active_at_timestamp(timestamp) {
            Some(MegaHardfork::Rex3)
        } else if self.is_rex_2_active_at_timestamp(timestamp) {
            Some(MegaHardfork::Rex2)
        } else if self.is_rex_1_active_at_timestamp(timestamp) {
            Some(MegaHardfork::Rex1)
        } else if self.is_rex_active_at_timestamp(timestamp) {
            Some(MegaHardfork::Rex)
        } else if self.is_mini_rex_2_active_at_timestamp(timestamp) {
            Some(MegaHardfork::MiniRex2)
        } else if self.is_mini_rex_1_active_at_timestamp(timestamp) {
            Some(MegaHardfork::MiniRex1)
        } else if self.is_mini_rex_active_at_timestamp(timestamp) {
            Some(MegaHardfork::MiniRex)
        } else {
            None
        }
    }

    /// Gets the expected `MegaSpecId` for a block with the given timestamp.
    fn spec_id(&self, timestamp: BlockTimestamp) -> MegaSpecId {
        // Newer hardforks should be checked first
        if self.is_rex_5_active_at_timestamp(timestamp) {
            MegaSpecId::REX5
        } else if self.is_rex_4_active_at_timestamp(timestamp) {
            MegaSpecId::REX4
        } else if self.is_rex_3_active_at_timestamp(timestamp) {
            MegaSpecId::REX3
        } else if self.is_rex_2_active_at_timestamp(timestamp) {
            MegaSpecId::REX2
        } else if self.is_rex_1_active_at_timestamp(timestamp) {
            MegaSpecId::REX1
        } else if self.is_rex_active_at_timestamp(timestamp) {
            MegaSpecId::REX
        } else if self.is_mini_rex_2_active_at_timestamp(timestamp) {
            MegaSpecId::MINI_REX
        } else if self.is_mini_rex_1_active_at_timestamp(timestamp) {
            MegaSpecId::EQUIVALENCE
        } else if self.is_mini_rex_active_at_timestamp(timestamp) {
            MegaSpecId::MINI_REX
        } else {
            MegaSpecId::EQUIVALENCE
        }
    }

    /// Returns `true` if [`MegaHardfork::MiniRex`] is active at given block timestamp.
    fn is_mini_rex_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::MiniRex).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaHardfork::MiniRex1`] is active at given block timestamp.
    fn is_mini_rex_1_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::MiniRex1).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaHardfork::MiniRex2`] is active at given block timestamp.
    fn is_mini_rex_2_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::MiniRex2).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaHardfork::Rex`] is active at given block timestamp.
    fn is_rex_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::Rex).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaHardfork::Rex1`] is active at given block timestamp.
    fn is_rex_1_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::Rex1).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaHardfork::Rex2`] is active at given block timestamp.
    fn is_rex_2_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::Rex2).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaHardfork::Rex3`] is active at given block timestamp.
    fn is_rex_3_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::Rex3).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaHardfork::Rex4`] is active at given block timestamp.
    fn is_rex_4_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::Rex4).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaHardfork::Rex5`] is active at given block timestamp.
    fn is_rex_5_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::Rex5).active_at_timestamp(timestamp)
    }
}

/// Configuration of the hardforks for `MegaETH`. It by default includes no `MegaHardfork` but
/// includes all hardforks before and including Optimism Isthmus. Optimism Isthmus is the hardfork
/// where `MegaETH` is established.
#[derive(Debug, Clone)]
pub struct MegaHardforkConfig {
    hardforks: Vec<(Box<dyn Hardfork>, ForkCondition)>,
    sequencer_registry_config: SequencerRegistryConfig,
}

impl Default for MegaHardforkConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, H> From<I> for MegaHardforkConfig
where
    I: Iterator<Item = (H, ForkCondition)>,
    H: Hardfork + 'static,
{
    fn from(iter: I) -> Self {
        Self {
            hardforks: iter.map(|(h, c)| (Box::new(h) as Box<dyn Hardfork>, c)).collect(),
            sequencer_registry_config: SequencerRegistryConfig::default(),
        }
    }
}

impl MegaHardforkConfig {
    /// Creates a new hardfork configuration with the default hardforks, i.e., all hardforks before
    /// and including Optimism Isthmus are enabled. Optimism Isthmus is the hardfork where
    /// `MegaETH` is established.
    pub fn new() -> Self {
        Self {
            hardforks: vec![
                (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Dao.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
                (
                    EthereumHardfork::Paris.boxed(),
                    ForkCondition::TTD {
                        activation_block_number: 0,
                        fork_block: None,
                        total_difficulty: U256::ZERO,
                    },
                ),
                (OpHardfork::Bedrock.boxed(), ForkCondition::Block(0)),
                (OpHardfork::Regolith.boxed(), ForkCondition::Timestamp(0)),
                (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Canyon.boxed(), ForkCondition::Timestamp(0)),
                (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Ecotone.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Fjord.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Granite.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Holocene.boxed(), ForkCondition::Timestamp(0)),
                (EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Isthmus.boxed(), ForkCondition::Timestamp(0)),
            ],
            sequencer_registry_config: SequencerRegistryConfig::default(),
        }
    }

    /// Sets all `MegaHardfork` to be activated at timestamp 0.
    pub fn with_all_activated(mut self) -> Self {
        self.insert(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
        self.insert(MegaHardfork::MiniRex1, ForkCondition::Timestamp(0));
        self.insert(MegaHardfork::MiniRex2, ForkCondition::Timestamp(0));
        self.insert(MegaHardfork::Rex, ForkCondition::Timestamp(0));
        self.insert(MegaHardfork::Rex1, ForkCondition::Timestamp(0));
        self.insert(MegaHardfork::Rex2, ForkCondition::Timestamp(0));
        self.insert(MegaHardfork::Rex3, ForkCondition::Timestamp(0));
        self.insert(MegaHardfork::Rex4, ForkCondition::Timestamp(0));
        self.insert(MegaHardfork::Rex5, ForkCondition::Timestamp(0));
        self
    }

    /// Sets the bootstrap configuration for `SequencerRegistry`.
    pub fn with_sequencer_registry_config(
        mut self,
        sequencer_registry_config: SequencerRegistryConfig,
    ) -> Self {
        self.sequencer_registry_config = sequencer_registry_config;
        self
    }

    /// Removes a `MegaHardfork` from the configuration, i.e., equivalent to setting the fork
    /// condition to [`ForkCondition::Never`].
    pub fn without(mut self, hardfork: MegaHardfork) -> Self {
        self.hardforks.retain(|(h, _)| h.name() != hardfork.name());
        self
    }

    /// Creates a new hardfork configuration with the given hardfork and condition.
    pub fn with(mut self, hardfork: impl Hardfork, condition: ForkCondition) -> Self {
        self.insert(hardfork, condition);
        self
    }

    /// Inserts a new hardfork into the configuration. If the hardfork is already present, it will
    /// be overwritten.
    pub fn insert(&mut self, hardfork: impl Hardfork, condition: ForkCondition) {
        let index = self.hardforks.iter().position(|(h, _)| h.name() == hardfork.name());
        if let Some(index) = index {
            self.hardforks[index] = (Box::new(hardfork), condition);
        } else {
            self.hardforks.push((Box::new(hardfork), condition));
        }
    }

    /// Gets `ForkCondition` by a [`Hardfork`]. If the hardfork is not present, returns `None`.
    pub fn get(&self, hardfork: impl Hardfork) -> Option<&ForkCondition> {
        self.hardforks
            .iter()
            .find(|(h, _)| h.name() == hardfork.name())
            .map(|(_, condition)| condition)
    }
}

impl EthereumHardforks for MegaHardforkConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }
}

impl OpHardforks for MegaHardforkConfig {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }
}

impl MegaHardforks for MegaHardforkConfig {
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }

    fn sequencer_registry_config(&self) -> SequencerRegistryConfig {
        self.sequencer_registry_config.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mega_hardfork_spec_ids_match_expected_specs() {
        // Note: MiniRex1 and MiniRex2 are patch hardforks that reverted to earlier specs.
        let cases = [
            (MegaHardfork::MiniRex, MegaSpecId::MINI_REX),
            (MegaHardfork::MiniRex1, MegaSpecId::EQUIVALENCE),
            (MegaHardfork::MiniRex2, MegaSpecId::MINI_REX),
            (MegaHardfork::Rex, MegaSpecId::REX),
            (MegaHardfork::Rex1, MegaSpecId::REX1),
            (MegaHardfork::Rex2, MegaSpecId::REX2),
            (MegaHardfork::Rex3, MegaSpecId::REX3),
            (MegaHardfork::Rex4, MegaSpecId::REX4),
            (MegaHardfork::Rex5, MegaSpecId::REX5),
        ];

        for (hardfork, expected_spec) in cases {
            assert_eq!(hardfork.spec_id(), expected_spec);
        }
    }

    #[test]
    fn test_default_config_contains_upstream_forks_and_no_mega_forks() {
        let config = MegaHardforkConfig::default();

        assert_eq!(
            config.ethereum_fork_activation(EthereumHardfork::Frontier),
            ForkCondition::Block(0)
        );
        assert_eq!(
            config.ethereum_fork_activation(EthereumHardfork::Prague),
            ForkCondition::Timestamp(0)
        );
        assert_eq!(config.op_fork_activation(OpHardfork::Isthmus), ForkCondition::Timestamp(0));
        assert_eq!(config.mega_fork_activation(MegaHardfork::MiniRex), ForkCondition::Never);
        assert_eq!(config.sequencer_registry_config(), SequencerRegistryConfig::default());
    }

    #[test]
    fn test_config_builder_helpers_override_and_remove_hardforks() {
        let mut config = MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(10))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(80));

        assert_eq!(config.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(10)));
        assert_eq!(config.get(MegaHardfork::Rex4), Some(&ForkCondition::Timestamp(80)));

        config.insert(MegaHardfork::MiniRex, ForkCondition::Timestamp(20));
        assert_eq!(config.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(20)));

        let config = config.without(MegaHardfork::MiniRex);
        assert_eq!(config.get(MegaHardfork::MiniRex), None);

        let from_iter = MegaHardforkConfig::from(
            [
                (MegaHardfork::MiniRex, ForkCondition::Timestamp(1)),
                (MegaHardfork::Rex2, ForkCondition::Timestamp(2)),
            ]
            .into_iter(),
        );
        assert_eq!(from_iter.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(1)));
        assert_eq!(from_iter.get(MegaHardfork::Rex2), Some(&ForkCondition::Timestamp(2)));
    }

    #[test]
    fn test_with_all_activated_enables_all_mega_hardforks() {
        let config = MegaHardforkConfig::default().with_all_activated();

        for hardfork in [
            MegaHardfork::MiniRex,
            MegaHardfork::MiniRex1,
            MegaHardfork::MiniRex2,
            MegaHardfork::Rex,
            MegaHardfork::Rex1,
            MegaHardfork::Rex2,
            MegaHardfork::Rex3,
            MegaHardfork::Rex4,
            MegaHardfork::Rex5,
        ] {
            assert_eq!(config.mega_fork_activation(hardfork), ForkCondition::Timestamp(0));
        }
    }

    #[test]
    fn test_with_sequencer_registry_config_overrides_registry_bootstrap_config() {
        let config_override = SequencerRegistryConfig {
            initial_system_address: alloy_primitives::address!(
                "0x1111111111111111111111111111111111111111"
            ),
            initial_sequencer: alloy_primitives::address!(
                "0x2222222222222222222222222222222222222222"
            ),
            initial_admin: alloy_primitives::address!("0x3333333333333333333333333333333333333333"),
        };

        let config =
            MegaHardforkConfig::default().with_sequencer_registry_config(config_override.clone());

        assert_eq!(config.sequencer_registry_config(), config_override);
    }

    #[test]
    fn test_hardfork_and_spec_id_follow_latest_active_timestamp() {
        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(10))
            .with(MegaHardfork::MiniRex1, ForkCondition::Timestamp(20))
            .with(MegaHardfork::MiniRex2, ForkCondition::Timestamp(30))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(40))
            .with(MegaHardfork::Rex1, ForkCondition::Timestamp(50))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(60))
            .with(MegaHardfork::Rex3, ForkCondition::Timestamp(70))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(80))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(90));

        let expected = [
            (0, None, MegaSpecId::EQUIVALENCE),
            (15, Some(MegaHardfork::MiniRex), MegaSpecId::MINI_REX),
            (25, Some(MegaHardfork::MiniRex1), MegaSpecId::EQUIVALENCE),
            (35, Some(MegaHardfork::MiniRex2), MegaSpecId::MINI_REX),
            (45, Some(MegaHardfork::Rex), MegaSpecId::REX),
            (55, Some(MegaHardfork::Rex1), MegaSpecId::REX1),
            (65, Some(MegaHardfork::Rex2), MegaSpecId::REX2),
            (75, Some(MegaHardfork::Rex3), MegaSpecId::REX3),
            (85, Some(MegaHardfork::Rex4), MegaSpecId::REX4),
            (95, Some(MegaHardfork::Rex5), MegaSpecId::REX5),
        ];

        for (timestamp, expected_hardfork, expected_spec) in expected {
            assert_eq!(config.hardfork(timestamp), expected_hardfork);
            assert_eq!(config.spec_id(timestamp), expected_spec);
        }
    }

    #[test]
    fn test_first_hardfork_block_handles_genesis_and_parent_activation_boundaries() {
        let config =
            MegaHardforkConfig::default().with(MegaHardfork::Rex2, ForkCondition::Timestamp(100));

        assert!(config.first_hardfork_block(MegaHardfork::Rex2, 99, (1, 100)));
        assert!(config.first_hardfork_block(MegaHardfork::Rex2, 99, (2, 100)));
        assert!(!config.first_hardfork_block(MegaHardfork::Rex2, 100, (3, 101)));
        assert!(!config.first_hardfork_block(MegaHardfork::Rex2, 99, (2, 99)));
    }

    #[test]
    fn test_spec_id_with_gaps_in_hardfork_configuration() {
        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(10))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(20))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(30));

        assert_eq!(config.spec_id(5), MegaSpecId::EQUIVALENCE);
        assert_eq!(config.spec_id(15), MegaSpecId::MINI_REX);
        assert_eq!(config.spec_id(25), MegaSpecId::REX4);
        assert_eq!(config.spec_id(35), MegaSpecId::REX5);
        assert_eq!(config.hardfork(15), Some(MegaHardfork::MiniRex));
        assert_eq!(config.hardfork(25), Some(MegaHardfork::Rex4));
        assert_eq!(config.hardfork(35), Some(MegaHardfork::Rex5));
    }

    #[test]
    fn test_latest_hardfork_wins_when_multiple_activate_at_same_timestamp() {
        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(10))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(10))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(10))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(10));

        assert_eq!(config.hardfork(9), None);
        assert_eq!(config.hardfork(10), Some(MegaHardfork::Rex5));
        assert_eq!(config.spec_id(10), MegaSpecId::REX5);
    }
}
