use core::any::Any;

use mega_evm::{
    alloy_hardforks::{EthereumHardfork, ForkCondition},
    alloy_op_hardforks::{EthereumHardforks, OpHardfork, OpHardforks},
    MegaHardfork, MegaHardforkConfig, MegaHardforks, MegaSpecId,
};

/// Fixed hardfork configuration for replay
///
/// Activation follows the fixed spec alone: every hardfork whose spec is included in `spec` is
/// active at timestamp 0, and every later hardfork never activates. This is how mega-evme
/// expresses "a chain running spec N" without a real activation schedule.
///
/// Per-fork parameters are *not* synthesized. A fixed-spec world still needs the chain's
/// parameters (the Rex5+ `SequencerRegistry` seeds are chain-specific data, not spec data), so an
/// optional parameter source can be attached with [`with_params_from`](Self::with_params_from).
/// Without one, parameter lookups return `None` as before.
#[derive(Debug, Clone, Copy)]
pub struct FixedHardfork<'a> {
    spec: MegaSpecId,
    params: Option<&'a MegaHardforkConfig>,
}

impl<'a> FixedHardfork<'a> {
    /// Create a new [`FixedHardfork`] with the given `spec`
    pub fn new(spec: MegaSpecId) -> Self {
        Self { spec, params: None }
    }

    /// Delegates per-fork parameter lookups to `config` while activation stays fixed.
    ///
    /// The delegation is wholesale rather than per parameter type: a parameter query carries no
    /// activation check, so forwarding the whole lookup keeps every parameter type reachable —
    /// including ones added later, which a hand-listed forwarding would silently drop.
    pub fn with_params_from(self, config: &'a MegaHardforkConfig) -> Self {
        Self { params: Some(config), ..self }
    }
}

impl EthereumHardforks for FixedHardfork<'_> {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        if fork <= EthereumHardfork::Prague {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}

impl OpHardforks for FixedHardfork<'_> {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        if fork <= OpHardfork::Isthmus {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}

impl MegaHardforks for FixedHardfork<'_> {
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition {
        let mapped_spec = fork.spec_id();
        if mapped_spec <= self.spec {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }

    fn fork_params_any(&self, fork: MegaHardfork) -> Option<&(dyn Any + Send + Sync)> {
        self.params?.fork_params_any(fork)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::{
        hardfork_schedule, SequencerRegistryConfig, SequencerRegistryRex6Config, MAINNET_CHAIN_ID,
    };

    /// A chain configuration carrying both parameter types, so the delegation can be checked for a
    /// type the published schedules do not (yet) attach.
    fn config_with_all_params() -> MegaHardforkConfig {
        // The unknown-chain fallback attaches both the Rex5 and the Rex6 registry parameters.
        hardfork_schedule(0xdead_beef)
    }

    /// Activation is a function of the fixed spec alone, in both directions: everything up to the
    /// spec is active at timestamp 0, everything above it never activates — no matter which
    /// timestamp is asked about, and no matter what the attached parameter source schedules.
    #[test]
    fn test_activation_follows_the_fixed_spec_only() {
        let chain = hardfork_schedule(MAINNET_CHAIN_ID);
        let fixed = FixedHardfork::new(MegaSpecId::REX5).with_params_from(&chain);

        for fork in MegaHardfork::VARIANTS {
            let expected = if fork.spec_id() <= MegaSpecId::REX5 {
                ForkCondition::Timestamp(0)
            } else {
                ForkCondition::Never
            };
            assert_eq!(fixed.mega_fork_activation(*fork), expected, "{fork:?}");
        }

        // The chain's own schedule puts Rex5 far in the future; the fixed world ignores it.
        assert_eq!(fixed.spec_id(0), MegaSpecId::REX5);
        assert_eq!(fixed.spec_id(u64::MAX), MegaSpecId::REX5);
    }

    /// Every spec on the ladder resolves to itself, including the patch hardforks that map back to
    /// an earlier spec.
    #[test]
    fn test_every_spec_resolves_to_itself() {
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
            MegaSpecId::REX7,
        ] {
            assert_eq!(FixedHardfork::new(spec).spec_id(0), spec, "{spec:?}");
        }
    }

    /// Without a parameter source, parameter lookups stay empty — the behavior the `run` / `tx`
    /// commands rely on.
    #[test]
    fn test_bare_fixed_hardfork_has_no_params() {
        let fixed = FixedHardfork::new(MegaSpecId::REX6);
        assert!(fixed.fork_params::<SequencerRegistryConfig>().is_none());
        assert!(fixed.fork_params::<SequencerRegistryRex6Config>().is_none());
    }

    /// With a parameter source, lookups return the source's values verbatim — for every parameter
    /// type it carries, not just the one the current deploy path happens to need.
    #[test]
    fn test_params_are_delegated_to_the_chain_config() {
        let chain = config_with_all_params();
        let fixed = FixedHardfork::new(MegaSpecId::REX7).with_params_from(&chain);

        assert_eq!(
            fixed.fork_params::<SequencerRegistryConfig>(),
            chain.fork_params::<SequencerRegistryConfig>(),
        );
        assert_eq!(
            fixed.fork_params::<SequencerRegistryRex6Config>(),
            chain.fork_params::<SequencerRegistryRex6Config>(),
        );
        assert!(fixed.fork_params::<SequencerRegistryConfig>().is_some());
        assert!(fixed.fork_params::<SequencerRegistryRex6Config>().is_some());
    }

    /// The mainnet schedule's Rex5 parameters survive the swap: this is what keeps a Rex5+
    /// override from failing closed in the pre-block `SequencerRegistry` deploy.
    #[test]
    fn test_mainnet_rex5_params_survive_the_swap() {
        let chain = hardfork_schedule(MAINNET_CHAIN_ID);
        let fixed = FixedHardfork::new(MegaSpecId::REX5).with_params_from(&chain);

        assert_eq!(
            fixed.fork_params::<SequencerRegistryConfig>(),
            chain.fork_params::<SequencerRegistryConfig>(),
        );
        assert!(fixed.fork_params::<SequencerRegistryConfig>().is_some());
    }

    /// Parameter lookups do not consult activation: a parameter attached to a fork the fixed spec
    /// never activates is still returned. Delegating wholesale therefore cannot depend on the
    /// order in which parameters and specs were chosen.
    #[test]
    fn test_params_lookup_is_independent_of_activation() {
        let chain = config_with_all_params();
        let fixed = FixedHardfork::new(MegaSpecId::EQUIVALENCE).with_params_from(&chain);

        assert_eq!(fixed.mega_fork_activation(MegaHardfork::Rex5), ForkCondition::Never);
        assert!(fixed.fork_params::<SequencerRegistryConfig>().is_some());
    }
}
