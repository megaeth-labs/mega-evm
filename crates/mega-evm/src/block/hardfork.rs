#[cfg(not(feature = "std"))]
use alloc as std;

use alloy_hardforks::{hardfork, EthereumHardfork, EthereumHardforks, ForkCondition, Hardfork};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use alloy_primitives::{BlockTimestamp, U256};
use auto_impl::auto_impl;
use core::any::Any;
use std::{boxed::Box, sync::Arc, vec::Vec};

use crate::MegaSpecId;

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

/// Marker trait for per-fork parameters.
///
/// Each params type is pinned to exactly one [`MegaHardfork`] variant via `FORK`.
/// This lets [`MegaHardforks::fork_params`] return a typed reference without
/// requiring the caller to specify the fork separately.
pub trait HardforkParams: Any + core::fmt::Debug + Send + Sync {
    /// The hardfork this params type belongs to.
    const FORK: MegaHardfork;
}

/// Extends [`OpHardforks`] with `MegaETH` helper methods.
#[auto_impl(&, Box, Arc)]
pub trait MegaHardforks: OpHardforks {
    /// Retrieves [`ForkCondition`] by a [`MegaHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition;

    /// Returns a type-erased reference to per-fork parameters, if configured.
    ///
    /// Most forks carry no parameters and the default implementation returns `None`.
    fn fork_params_any(&self, _fork: MegaHardfork) -> Option<&(dyn Any + Send + Sync)> {
        None
    }

    /// Returns a typed reference to per-fork parameters.
    ///
    /// `P::FORK` identifies the fork. Returns `None` if the fork has no params configured.
    fn fork_params<P: HardforkParams>(&self) -> Option<&P> {
        self.fork_params_any(P::FORK)?.downcast_ref::<P>()
    }

    /// Returns the current `MegaHardfork` active at the given timestamp.
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

    /// Returns the current `MegaSpecId` for the given block timestamp.
    fn spec_id(&self, timestamp: BlockTimestamp) -> MegaSpecId {
        self.hardfork(timestamp).map_or(MegaSpecId::EQUIVALENCE, |h| h.spec_id())
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

/// A single fork entry: identity, activation condition, and optional per-fork parameters.
#[derive(Debug)]
struct ForkEntry {
    fork: Box<dyn Hardfork>,
    condition: ForkCondition,
    params: Option<Arc<dyn Any + Send + Sync>>,
}

impl Clone for ForkEntry {
    fn clone(&self) -> Self {
        Self { fork: self.fork.clone(), condition: self.condition, params: self.params.clone() }
    }
}

/// Configuration of the hardforks for `MegaETH`. It by default includes no `MegaHardfork` but
/// includes all hardforks before and including Optimism Isthmus. Optimism Isthmus is the hardfork
/// where `MegaETH` is established.
///
/// Per-fork parameters (e.g., [`SequencerRegistryConfig`](crate::SequencerRegistryConfig)) are
/// embedded in the corresponding fork entry via [`with_params`](Self::with_params).
#[derive(Debug, Clone)]
pub struct MegaHardforkConfig {
    entries: Vec<ForkEntry>,
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
            entries: iter
                .map(|(h, c)| ForkEntry {
                    fork: Box::new(h) as Box<dyn Hardfork>,
                    condition: c,
                    params: None,
                })
                .collect(),
        }
    }
}

impl MegaHardforkConfig {
    /// Creates a new hardfork configuration with the default hardforks, i.e., all hardforks before
    /// and including Optimism Isthmus are enabled. Optimism Isthmus is the hardfork where
    /// `MegaETH` is established.
    pub fn new() -> Self {
        let forks: Vec<(Box<dyn Hardfork>, ForkCondition)> = vec![
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
        ];
        Self {
            entries: forks
                .into_iter()
                .map(|(fork, condition)| ForkEntry { fork, condition, params: None })
                .collect(),
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

    /// Attaches per-fork parameters to the entry identified by `P::FORK`.
    ///
    /// The fork must already exist in the config (via [`with`](Self::with) or default).
    /// Panics if the fork is not found.
    pub fn with_params<P: HardforkParams>(mut self, params: P) -> Self {
        let entry =
            self.entries.iter_mut().find(|e| e.fork.name() == P::FORK.name()).unwrap_or_else(
                || {
                    panic!(
                        "Cannot attach params to fork {:?}: fork not registered in config. \
                     Call .with({:?}, condition) first.",
                        P::FORK,
                        P::FORK,
                    )
                },
            );
        entry.params = Some(Arc::new(params));
        self
    }

    /// Removes a `MegaHardfork` from the configuration, i.e., equivalent to setting the fork
    /// condition to [`ForkCondition::Never`].
    pub fn without(mut self, hardfork: MegaHardfork) -> Self {
        self.entries.retain(|e| e.fork.name() != hardfork.name());
        self
    }

    /// Creates a new hardfork configuration with the given hardfork and condition.
    pub fn with(mut self, hardfork: impl Hardfork, condition: ForkCondition) -> Self {
        self.insert(hardfork, condition);
        self
    }

    /// Inserts a new hardfork into the configuration. If the hardfork is already present, it will
    /// be overwritten (condition updated, params preserved).
    pub fn insert(&mut self, hardfork: impl Hardfork, condition: ForkCondition) {
        let index = self.entries.iter().position(|e| e.fork.name() == hardfork.name());
        if let Some(index) = index {
            self.entries[index].condition = condition;
        } else {
            self.entries.push(ForkEntry { fork: Box::new(hardfork), condition, params: None });
        }
    }

    /// Gets `ForkCondition` by a [`Hardfork`]. If the hardfork is not present, returns `None`.
    pub fn get(&self, hardfork: impl Hardfork) -> Option<&ForkCondition> {
        self.entries.iter().find(|e| e.fork.name() == hardfork.name()).map(|e| &e.condition)
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

    fn fork_params_any(&self, fork: MegaHardfork) -> Option<&(dyn Any + Send + Sync)> {
        self.entries.iter().find(|e| e.fork.name() == fork.name()).and_then(|e| e.params.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SequencerRegistryConfig;

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
        assert!(config.fork_params::<SequencerRegistryConfig>().is_none());
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
    fn test_fork_params_typed_access() {
        let params = SequencerRegistryConfig {
            initial_system_address: alloy_primitives::address!(
                "0x1111111111111111111111111111111111111111"
            ),
            initial_sequencer: alloy_primitives::address!(
                "0x2222222222222222222222222222222222222222"
            ),
            initial_admin: alloy_primitives::address!("0x3333333333333333333333333333333333333333"),
        };

        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
            .with_params(params.clone());

        let retrieved = config
            .fork_params::<SequencerRegistryConfig>()
            .expect("should have SequencerRegistryConfig");
        assert_eq!(retrieved, &params);
    }

    #[test]
    fn test_fork_params_none_when_not_configured() {
        let config =
            MegaHardforkConfig::default().with(MegaHardfork::Rex5, ForkCondition::Timestamp(0));

        assert!(config.fork_params::<SequencerRegistryConfig>().is_none());
    }

    #[test]
    fn test_hardfork_and_spec_id_follow_latest_active_timestamp() {
        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(100))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(200))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(300));

        assert_eq!(config.hardfork(99), None);
        assert_eq!(config.hardfork(100), Some(MegaHardfork::MiniRex));
        assert_eq!(config.hardfork(200), Some(MegaHardfork::Rex4));
        assert_eq!(config.hardfork(300), Some(MegaHardfork::Rex5));
        assert_eq!(config.spec_id(99), MegaSpecId::EQUIVALENCE);
        assert_eq!(config.spec_id(100), MegaSpecId::MINI_REX);
        assert_eq!(config.spec_id(200), MegaSpecId::REX4);
        assert_eq!(config.spec_id(300), MegaSpecId::REX5);
    }
}
