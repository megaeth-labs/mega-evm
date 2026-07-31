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
        /// The tenth hardfork (sixth patch to Rex).
        Rex6,
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
            Self::Rex6 => MegaSpecId::REX6,
        }
    }
}

/// Validation error returned by [`HardforkParams::validate`].
#[derive(Debug, Clone, PartialEq, Eq, derive_more::Display, derive_more::Error)]
#[display("{message}")]
pub struct HardforkParamsError {
    /// Human-readable description of the invalid field or invariant.
    pub message: std::string::String,
}

/// Marker trait for per-fork parameters.
///
/// Each params type is pinned to exactly one [`MegaHardfork`] variant via `FORK`.
/// This lets [`MegaHardforks::fork_params`] return a typed reference without
/// requiring the caller to specify the fork separately.
pub trait HardforkParams: Any + core::fmt::Debug + Send + Sync {
    /// The hardfork this params type belongs to.
    const FORK: MegaHardfork;

    /// Validates construction-time invariants (no cross-fork context required).
    ///
    /// Called by [`MegaHardforkConfig::with_params`] so misconfiguration is caught
    /// at chain-config load time rather than at the first block where the fork activates.
    /// The default implementation accepts any value.
    fn validate(&self) -> Result<(), HardforkParamsError> {
        Ok(())
    }
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
        if self.is_rex_6_active_at_timestamp(timestamp) {
            Some(MegaHardfork::Rex6)
        } else if self.is_rex_5_active_at_timestamp(timestamp) {
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

    /// Returns the highest [`MegaSpecId`] among all [`MegaHardfork`]s activated at or before
    /// `timestamp`.
    ///
    /// This differs from [`spec_id`](Self::spec_id) only when a patch hardfork maps to an earlier
    /// spec, as `MiniRex1` does (it rolls back to `EQUIVALENCE`). The two answer different
    /// questions:
    ///
    /// - `spec_id` — *which EVM semantics execute in this block*. Reversible: a rollback hardfork
    ///   moves it back down, and it must stay the gate for execution behavior and block limits.
    /// - This method — *which chain-setup features have ever been activated*. Monotone: a spec
    ///   rollback does not un-deploy a predeploy or retract a pre-block system call, so one-way
    ///   setup must be gated on this instead.
    ///
    /// Deriving setup gates from this single value keeps them additive by construction: a config
    /// that schedules only a late fork still gets every earlier fork's setup, matching the ordinal
    /// inclusion the EVM layer already relies on.
    ///
    /// The equivalence with the per-fork `is_*_active_at_timestamp` predicates holds for
    /// *spec-introducing* forks — those whose spec is strictly higher than every earlier fork's.
    /// `MiniRex1` (rollback) and `MiniRex2` (restoration) introduce no new spec and are therefore
    /// not recoverable from a spec ordinal; nothing gates on them.
    ///
    /// Like `spec_id` and [`hardfork`](Self::hardfork), this is timestamp-scoped: a `MegaHardfork`
    /// registered with [`ForkCondition::Block`] or [`ForkCondition::TTD`] never reports active
    /// here. Every `MegaHardfork` in the canonical schedules uses `Timestamp` or `Never`.
    fn max_activated_spec_id(&self, timestamp: BlockTimestamp) -> MegaSpecId {
        MegaHardfork::VARIANTS
            .iter()
            .filter(|fork| self.mega_fork_activation(**fork).active_at_timestamp(timestamp))
            .map(|fork| fork.spec_id())
            .max()
            .unwrap_or(MegaSpecId::EQUIVALENCE)
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

    /// Returns `true` if [`MegaHardfork::Rex6`] is active at given block timestamp.
    fn is_rex_6_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.mega_fork_activation(MegaHardfork::Rex6).active_at_timestamp(timestamp)
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
    pub fn with_all_activated(self) -> Self {
        self.with_all_activated_through(MegaSpecId::default())
    }

    /// Activates every `MegaHardfork` whose spec is enabled under `spec` at timestamp 0, and
    /// leaves every later fork unregistered.
    ///
    /// This is how to express "a chain running spec N": the resulting config resolves to `spec`
    /// at any timestamp. Removing only the next fork up is not equivalent — the ladder runs past
    /// it, so the config would resolve to the newest fork still registered rather than to `spec`,
    /// and it would silently drift again the next time a spec is introduced. On top of the wrong
    /// executing spec, the leftover later forks also keep the activated-spec floor
    /// ([`max_activated_spec_id`](MegaHardforks::max_activated_spec_id)) high, so every pre-block
    /// setup gate below them stays open.
    ///
    /// The result is a function of `spec` alone, not of what the config held before: a later fork
    /// already registered is removed rather than left in place, so the resolved spec does not
    /// depend on builder call order.
    ///
    /// Patch hardforks are included by their spec, not their position: `MiniRex1` maps back to
    /// [`MegaSpecId::EQUIVALENCE`], so it is registered for every `spec`.
    pub fn with_all_activated_through(mut self, spec: MegaSpecId) -> Self {
        for fork in MegaHardfork::VARIANTS {
            if spec.is_enabled(fork.spec_id()) {
                self.insert(*fork, ForkCondition::Timestamp(0));
            } else {
                self = self.without(*fork);
            }
        }
        self
    }

    /// Attaches per-fork parameters to the entry identified by `P::FORK`.
    ///
    /// The fork must already exist in the config (via [`with`](Self::with) or default).
    /// Panics if the fork is not found or if `params.validate()` returns an error.
    pub fn with_params<P: HardforkParams>(mut self, params: P) -> Self {
        if let Err(e) = params.validate() {
            panic!("Invalid params for fork {:?}: {}", P::FORK, e.message,);
        }
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
            (MegaHardfork::Rex6, MegaSpecId::REX6),
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

        // Driven off `VARIANTS`, which the `hardfork!` macro generates from the same variant list
        // that declares the enum. A second hand-written list here would assert only that the
        // forks someone remembered to name are activated — a new fork missing from both the
        // builder and the list would fail neither.
        for hardfork in MegaHardfork::VARIANTS {
            assert_eq!(
                config.mega_fork_activation(*hardfork),
                ForkCondition::Timestamp(0),
                "{hardfork:?}"
            );
        }
    }

    #[test]
    fn test_fork_params_typed_access() {
        let params = SequencerRegistryConfig {
            rex5_initial_sequencer: alloy_primitives::address!(
                "0x2222222222222222222222222222222222222222"
            ),
            rex5_initial_admin: alloy_primitives::address!(
                "0x3333333333333333333333333333333333333333"
            ),
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
    fn test_default_validate_accepts_any_value() {
        #[derive(Debug)]
        struct NullParams;

        impl HardforkParams for NullParams {
            const FORK: MegaHardfork = MegaHardfork::Rex4;
        }

        assert!(NullParams.validate().is_ok());
    }

    #[test]
    fn test_hardfork_params_error_display() {
        let e = HardforkParamsError { message: "something went wrong".into() };
        assert_eq!(e.to_string(), "something went wrong");
    }

    #[test]
    #[should_panic(expected = "Invalid params for fork")]
    fn test_with_params_panics_on_validation_error() {
        #[derive(Debug)]
        struct AlwaysErrParams;

        impl HardforkParams for AlwaysErrParams {
            const FORK: MegaHardfork = MegaHardfork::Rex4;

            fn validate(&self) -> Result<(), HardforkParamsError> {
                Err(HardforkParamsError { message: "intentional test error".into() })
            }
        }

        MegaHardforkConfig::default().with_all_activated().with_params(AlwaysErrParams);
    }

    /// Mainnet runs a real spec rollback: `MiniRex1` maps to `EQUIVALENCE` while `MiniRex`'s
    /// timestamp has already passed. Inside that window the resolved spec and the activated-spec
    /// floor disagree, and only the floor keeps one-way setup (the Oracle predeploys) enabled.
    #[test]
    fn test_mainnet_rollback_window_separates_resolved_spec_from_floor() {
        let hf = crate::mainnet_hardforks();
        let ForkCondition::Timestamp(rollback_start) =
            hf.mega_fork_activation(MegaHardfork::MiniRex1)
        else {
            panic!("mainnet must schedule MiniRex1 by timestamp");
        };
        let ForkCondition::Timestamp(rollback_end) =
            hf.mega_fork_activation(MegaHardfork::MiniRex2)
        else {
            panic!("mainnet must schedule MiniRex2 by timestamp");
        };
        assert!(rollback_start < rollback_end, "the rollback window must be non-empty");

        for ts in [rollback_start, rollback_start + 1, rollback_end - 1] {
            assert_eq!(hf.spec_id(ts), MegaSpecId::EQUIVALENCE, "executing spec rolls back");
            assert_eq!(hf.max_activated_spec_id(ts), MegaSpecId::MINI_REX, "floor stays monotone");
            // Gating setup on the executing spec would drop the MiniRex predeploys here.
            assert!(!hf.spec_id(ts).is_enabled(MegaSpecId::MINI_REX));
            assert!(hf.max_activated_spec_id(ts).is_enabled(MegaSpecId::MINI_REX));
            assert!(hf.is_mini_rex_active_at_timestamp(ts));
        }
    }

    /// The floor reproduces every per-fork activation predicate exactly, for every
    /// *spec-introducing* fork, on every canonical schedule. This is what makes the switch a
    /// no-op on well-formed ladders.
    ///
    /// Forks that introduce no new spec — `MiniRex1` (rollback to `EQUIVALENCE`) and `MiniRex2`
    /// (restoration to `MINI_REX`) — are not recoverable from a spec ordinal by construction, so
    /// nothing may gate on them. The predicate is derived rather than hardcoded so a future
    /// rollback fork is classified automatically.
    #[test]
    fn test_floor_matches_per_fork_activation_for_spec_introducing_forks() {
        for hf in [
            crate::mainnet_hardforks(),
            crate::testnet_hardforks(),
            crate::all_activated_hardforks(),
        ] {
            let mut stamps = std::vec![0u64, u64::MAX];
            for fork in MegaHardfork::VARIANTS {
                if let ForkCondition::Timestamp(t) = hf.mega_fork_activation(*fork) {
                    stamps.extend([t.saturating_sub(1), t, t.saturating_add(1)]);
                }
            }

            for ts in stamps {
                let floor = hf.max_activated_spec_id(ts);
                for (i, fork) in MegaHardfork::VARIANTS.iter().enumerate() {
                    let introduces_spec =
                        MegaHardfork::VARIANTS[..i].iter().all(|e| e.spec_id() < fork.spec_id());
                    if !introduces_spec {
                        continue;
                    }
                    assert_eq!(
                        floor.is_enabled(fork.spec_id()),
                        hf.mega_fork_activation(*fork).active_at_timestamp(ts),
                        "floor disagrees with per-fork activation for {fork:?} at ts={ts}"
                    );
                }
            }
        }
    }

    /// The gap this change closes: a config that schedules a later fork without its predecessor
    /// resolves to that fork's spec, yet every per-fork predicate below it reports inactive.
    /// The floor makes the lower gates additive again.
    #[test]
    fn test_partial_ladder_floor_enables_unscheduled_predecessors() {
        let hf = MegaHardforkConfig::new()
            .with(MegaHardfork::Rex5, ForkCondition::Never)
            .with(MegaHardfork::Rex6, ForkCondition::Timestamp(0));

        assert!(!hf.is_rex_5_active_at_timestamp(0), "Rex5 is not scheduled");
        assert!(!hf.is_mini_rex_active_at_timestamp(0), "MiniRex is not scheduled");
        assert_eq!(hf.spec_id(0), MegaSpecId::REX6);

        let floor = hf.max_activated_spec_id(0);
        assert_eq!(floor, MegaSpecId::REX6);
        for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX2, MegaSpecId::REX4, MegaSpecId::REX5] {
            assert!(floor.is_enabled(spec), "floor must enable {spec:?} on a partial ladder");
        }
    }

    /// `with_all_activated_through` is the well-formed way to express "a chain running spec N":
    /// both the executing spec and the activated-spec floor resolve to exactly `N`, at any
    /// timestamp. Driven off `MegaSpecId`'s own progression rather than a second hand-written
    /// list, so a newly introduced spec fails here once instead of silently widening every
    /// "chain running spec N" config in the suite.
    #[test]
    fn test_with_all_activated_through_resolves_to_that_spec() {
        for spec in [
            MegaSpecId::EQUIVALENCE,
            MegaSpecId::MINI_REX,
            MegaSpecId::REX,
            MegaSpecId::REX1,
            MegaSpecId::REX2,
            MegaSpecId::REX3,
            MegaSpecId::REX4,
            MegaSpecId::REX5,
            MegaSpecId::REX6,
        ] {
            let config = MegaHardforkConfig::default().with_all_activated_through(spec);
            assert_eq!(config.spec_id(0), spec, "{spec:?} at genesis");
            assert_eq!(config.spec_id(u64::MAX), spec, "{spec:?} must be terminal");
            assert_eq!(config.max_activated_spec_id(0), spec, "{spec:?} floor at genesis");
            assert_eq!(
                config.max_activated_spec_id(u64::MAX),
                spec,
                "{spec:?} floor must be terminal"
            );

            // The same contract must hold when the config already carries later forks: the
            // builder states the whole ladder, so it removes what it does not activate. Without
            // that, the resolved spec would depend on which builder ran last.
            let downgraded =
                MegaHardforkConfig::default().with_all_activated().with_all_activated_through(spec);
            assert_eq!(downgraded.spec_id(u64::MAX), spec, "{spec:?} from an activated config");
            assert_eq!(
                downgraded.max_activated_spec_id(u64::MAX),
                spec,
                "{spec:?} floor from an activated config"
            );
        }
    }

    /// Removing a middle rung does NOT express "a chain running spec N". It is the partial-ladder
    /// shape: the executing spec follows the newest fork still registered, and the floor keeps
    /// every lower setup gate open.
    #[test]
    fn test_removing_a_middle_rung_does_not_lower_the_spec() {
        let partial =
            MegaHardforkConfig::default().with_all_activated().without(MegaHardfork::Rex4);

        assert!(!partial.is_rex_4_active_at_timestamp(0), "Rex4 itself is unregistered");
        assert_ne!(partial.spec_id(0), MegaSpecId::REX4, "the executing spec is not lowered");
        assert!(
            partial.max_activated_spec_id(0).is_enabled(MegaSpecId::REX4),
            "the floor still enables the removed fork's spec"
        );
    }

    /// The floor and the executing spec agree across the Rex5/Rex6 boundary on every canonical
    /// schedule, which is what makes the two-spec split in `resolve_system_address` inert today.
    /// Adding a hardfork that maps below `REX5` would break this and must be caught here.
    #[test]
    fn test_floor_and_executing_spec_agree_across_rex5_rex6_boundary() {
        for hf in [
            crate::mainnet_hardforks(),
            crate::testnet_hardforks(),
            crate::all_activated_hardforks(),
        ] {
            let mut stamps = std::vec![0u64, u64::MAX];
            for fork in MegaHardfork::VARIANTS {
                if let ForkCondition::Timestamp(t) = hf.mega_fork_activation(*fork) {
                    stamps.extend([t.saturating_sub(1), t, t.saturating_add(1)]);
                }
            }

            for ts in stamps {
                let (exec, floor) = (hf.spec_id(ts), hf.max_activated_spec_id(ts));
                for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
                    assert_eq!(
                        exec.is_enabled(spec),
                        floor.is_enabled(spec),
                        "executing spec and floor disagree on {spec:?} at ts={ts}"
                    );
                }
            }
        }
    }

    /// Documented domain limit: the floor is timestamp-scoped, so a `MegaHardfork` registered by
    /// block number never contributes to it and the equivalence with the per-fork predicates does
    /// not hold. `spec_id`/`hardfork` share this limitation; every canonical schedule uses
    /// `Timestamp` or `Never`.
    #[test]
    fn test_floor_ignores_block_numbered_forks() {
        let hf = MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Block(0))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(0));

        assert!(!hf.is_mini_rex_active_at_timestamp(0), "block-numbered forks are not timestamped");
        assert_eq!(hf.max_activated_spec_id(0), MegaSpecId::REX);
        assert!(hf.max_activated_spec_id(0).is_enabled(MegaSpecId::MINI_REX));
    }

    #[test]
    fn test_hardfork_and_spec_id_follow_latest_active_timestamp() {
        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(100))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(200))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(300))
            .with(MegaHardfork::Rex6, ForkCondition::Timestamp(400));

        assert_eq!(config.hardfork(99), None);
        assert_eq!(config.hardfork(100), Some(MegaHardfork::MiniRex));
        assert_eq!(config.hardfork(200), Some(MegaHardfork::Rex4));
        assert_eq!(config.hardfork(300), Some(MegaHardfork::Rex5));
        assert_eq!(config.hardfork(400), Some(MegaHardfork::Rex6));
        assert_eq!(config.spec_id(99), MegaSpecId::EQUIVALENCE);
        assert_eq!(config.spec_id(100), MegaSpecId::MINI_REX);
        assert_eq!(config.spec_id(200), MegaSpecId::REX4);
        assert_eq!(config.spec_id(300), MegaSpecId::REX5);
        assert_eq!(config.spec_id(400), MegaSpecId::REX6);
    }
}
