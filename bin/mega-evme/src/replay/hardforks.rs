use core::any::Any;

use mega_evm::{
    alloy_hardforks::{EthereumHardfork, ForkCondition},
    alloy_op_hardforks::{EthereumHardforks, OpHardfork, OpHardforks},
    MegaHardfork, MegaHardforkConfig, MegaHardforks, MegaSpecId,
};

use crate::common::FixedHardfork;

/// Returns the hardfork configuration for a given chain ID.
///
/// Delegates to the canonical per-chain schedule in `mega-evm`
/// ([`mega_evm::hardfork_schedule`]), which is the single source of truth for
/// `MegaETH` mainnet/testnet activation timestamps.
pub fn get_hardfork_config(chain_id: u64) -> MegaHardforkConfig {
    mega_evm::hardfork_schedule(chain_id)
}

/// The hardfork schedule a replay executes under.
///
/// Without a spec override this is the chain's real schedule, so the replay reproduces the block
/// as it happened. With `--override.spec` it is a schedule synthesized from the forced spec, which
/// makes the override a coherent what-if: the pre-block predeploys, the EIP-2935 / EIP-4788
/// gating, the block-level resource limits and the EVM semantics all come from the same spec,
/// instead of mixing the historical setup with forced semantics into a world that never existed.
///
/// The synthesized schedule takes activation from the forced spec but keeps the chain's per-fork
/// parameters, which are chain data rather than spec data (the Rex5+ `SequencerRegistry` seeds).
#[derive(Debug, Clone, Copy)]
pub enum ReplayHardforks<'a> {
    /// The chain's published activation schedule.
    Chain(&'a MegaHardforkConfig),
    /// A schedule synthesized from a forced spec, with parameters from the chain.
    Forced(FixedHardfork<'a>),
}

impl<'a> ReplayHardforks<'a> {
    /// Selects the schedule for a replay: the chain's own, or one synthesized from
    /// `spec_override`.
    pub fn resolve(chain: &'a MegaHardforkConfig, spec_override: Option<MegaSpecId>) -> Self {
        match spec_override {
            Some(spec) => Self::Forced(FixedHardfork::new(spec).with_params_from(chain)),
            None => Self::Chain(chain),
        }
    }
}

impl EthereumHardforks for ReplayHardforks<'_> {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        match self {
            Self::Chain(chain) => chain.ethereum_fork_activation(fork),
            Self::Forced(forced) => forced.ethereum_fork_activation(fork),
        }
    }
}

impl OpHardforks for ReplayHardforks<'_> {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        match self {
            Self::Chain(chain) => chain.op_fork_activation(fork),
            Self::Forced(forced) => forced.op_fork_activation(fork),
        }
    }
}

impl MegaHardforks for ReplayHardforks<'_> {
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition {
        match self {
            Self::Chain(chain) => chain.mega_fork_activation(fork),
            Self::Forced(forced) => forced.mega_fork_activation(fork),
        }
    }

    fn fork_params_any(&self, fork: MegaHardfork) -> Option<&(dyn Any + Send + Sync)> {
        match self {
            Self::Chain(chain) => chain.fork_params_any(fork),
            Self::Forced(forced) => forced.fork_params_any(fork),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::{
        flat_system_contract_specs, BlockLimits, EvmTxRuntimeLimits, SequencerRegistryConfig,
        SequencerRegistryRex6Config, MAINNET_CHAIN_ID, TESTNET_CHAIN_ID,
    };

    /// A mainnet timestamp inside the Rex4 window: Rex4 is active, Rex5 is not.
    const REX4_TIMESTAMP: u64 = 1_776_700_000;

    /// A mainnet timestamp inside the `MiniRex` window, before any Rex fork.
    const MINI_REX_TIMESTAMP: u64 = 1_764_000_000;

    /// Without an override the replay world is the chain's schedule, unchanged.
    #[test]
    fn test_without_override_the_chain_schedule_is_used() {
        let chain = get_hardfork_config(MAINNET_CHAIN_ID);
        let world = ReplayHardforks::resolve(&chain, None);

        for timestamp in [0, MINI_REX_TIMESTAMP, REX4_TIMESTAMP, u64::MAX] {
            assert_eq!(world.spec_id(timestamp), chain.spec_id(timestamp), "at {timestamp}");
            assert_eq!(world.hardfork(timestamp), chain.hardfork(timestamp), "at {timestamp}");
        }
        for fork in MegaHardfork::VARIANTS {
            assert_eq!(
                world.mega_fork_activation(*fork),
                chain.mega_fork_activation(*fork),
                "{fork:?}",
            );
        }
        assert_eq!(
            world.fork_params::<SequencerRegistryConfig>(),
            chain.fork_params::<SequencerRegistryConfig>(),
        );
    }

    /// A block before the first `MegaHardfork` has no active fork, and the replay must report that
    /// rather than silently pick one. Testnet's `MiniRex` activates at timestamp 0, so this is
    /// checked on a config whose first fork activates later.
    #[test]
    fn test_without_override_a_block_before_any_fork_has_no_hardfork() {
        let chain =
            MegaHardforkConfig::new().with(MegaHardfork::Rex, ForkCondition::Timestamp(100));
        let world = ReplayHardforks::resolve(&chain, None);

        assert_eq!(world.hardfork(99), None);
        assert_eq!(world.hardfork(100), Some(MegaHardfork::Rex));
    }

    /// With an override, the whole schedule follows the forced spec: it resolves to that spec at
    /// the block's timestamp (and at any other), so every consumer that reads the schedule —
    /// predeploys, block limits, EVM semantics — sees the same world.
    #[test]
    fn test_override_makes_the_schedule_follow_the_forced_spec() {
        let chain = get_hardfork_config(MAINNET_CHAIN_ID);
        let world = ReplayHardforks::resolve(&chain, Some(MegaSpecId::REX5));

        assert_eq!(world.spec_id(MINI_REX_TIMESTAMP), MegaSpecId::REX5);
        assert_eq!(world.spec_id(REX4_TIMESTAMP), MegaSpecId::REX5);
        assert_eq!(world.hardfork(MINI_REX_TIMESTAMP), Some(MegaHardfork::Rex5));
        assert!(world.is_rex_5_active_at_timestamp(MINI_REX_TIMESTAMP));
        assert!(!world.is_rex_6_active_at_timestamp(MINI_REX_TIMESTAMP));
    }

    /// The forced schedule keeps the chain's per-fork parameters. Without this the pre-block
    /// `SequencerRegistry` deploy fails closed on every Rex5+ override.
    #[test]
    fn test_override_keeps_the_chain_fork_params() {
        for chain_id in [MAINNET_CHAIN_ID, TESTNET_CHAIN_ID] {
            let chain = get_hardfork_config(chain_id);
            let world = ReplayHardforks::resolve(&chain, Some(MegaSpecId::REX5));

            assert_eq!(
                world.fork_params::<SequencerRegistryConfig>(),
                chain.fork_params::<SequencerRegistryConfig>(),
                "chain {chain_id}",
            );
            assert!(
                world.fork_params::<SequencerRegistryConfig>().is_some(),
                "chain {chain_id} must carry the Rex5 registry parameters",
            );
        }
    }

    /// Every parameter type the chain carries is delegated, not just the one the Rex5 deploy path
    /// needs today.
    #[test]
    fn test_override_delegates_every_params_type() {
        // The unknown-chain fallback carries both registry parameter types.
        let chain = get_hardfork_config(0xdead_beef);
        let world = ReplayHardforks::resolve(&chain, Some(MegaSpecId::REX6));

        assert_eq!(
            world.fork_params::<SequencerRegistryRex6Config>(),
            chain.fork_params::<SequencerRegistryRex6Config>(),
        );
        assert!(world.fork_params::<SequencerRegistryRex6Config>().is_some());
    }

    /// The predeploy set follows the override, in both directions: forcing a newer spec on an old
    /// block installs contracts that did not exist at that block, and forcing an older spec on a
    /// recent block withholds contracts that did.
    #[test]
    fn test_override_switches_the_predeploy_set() {
        let chain = get_hardfork_config(MAINNET_CHAIN_ID);

        let historical = ReplayHardforks::resolve(&chain, None);
        let upgraded = ReplayHardforks::resolve(&chain, Some(MegaSpecId::REX5));
        let downgraded = ReplayHardforks::resolve(&chain, Some(MegaSpecId::MINI_REX));

        let at = |world: &ReplayHardforks<'_>, timestamp| {
            flat_system_contract_specs(world, timestamp)
                .into_iter()
                .map(|spec| spec.address)
                .collect::<Vec<_>>()
        };

        // MegaLimitControl arrives with Rex4, so a MiniRex-era block gains it under a Rex5
        // override and a Rex4-era block loses it under a MiniRex override.
        let historical_mini_rex = at(&historical, MINI_REX_TIMESTAMP);
        let forced_rex5 = at(&upgraded, MINI_REX_TIMESTAMP);
        assert!(forced_rex5.len() > historical_mini_rex.len());
        assert!(forced_rex5.contains(&mega_evm::LIMIT_CONTROL_ADDRESS));
        assert!(!historical_mini_rex.contains(&mega_evm::LIMIT_CONTROL_ADDRESS));

        let historical_rex4 = at(&historical, REX4_TIMESTAMP);
        let forced_mini_rex = at(&downgraded, REX4_TIMESTAMP);
        assert!(historical_rex4.contains(&mega_evm::LIMIT_CONTROL_ADDRESS));
        assert!(!forced_mini_rex.contains(&mega_evm::LIMIT_CONTROL_ADDRESS));

        // The registry is deployed separately from the flat predeploys; its gate reads the same
        // schedule, so a Rex5 override activates it on a MiniRex-era block.
        assert!(upgraded.is_rex_5_active_at_timestamp(MINI_REX_TIMESTAMP));
        assert!(!historical.is_rex_5_active_at_timestamp(MINI_REX_TIMESTAMP));
    }

    /// Block-level limits follow the override too, not only the per-transaction ones. The
    /// block-level dimensions are the ones a per-transaction patch cannot reach: they come from
    /// the hardfork resolved out of the schedule.
    #[test]
    fn test_override_switches_block_level_limits() {
        let chain = get_hardfork_config(MAINNET_CHAIN_ID);
        let gas_limit = 10_000_000_000;

        let historical = ReplayHardforks::resolve(&chain, None);
        let forced = ReplayHardforks::resolve(&chain, Some(MegaSpecId::REX5));

        let historical_limits = BlockLimits::from_hardfork_and_block_gas_limit(
            historical.hardfork(MINI_REX_TIMESTAMP).expect("MiniRex is active"),
            gas_limit,
        );
        let forced_limits = BlockLimits::from_hardfork_and_block_gas_limit(
            forced.hardfork(MINI_REX_TIMESTAMP).expect("the forced spec is always active"),
            gas_limit,
        );

        // State growth metering arrives with Rex: the block-level budget is unlimited under
        // MiniRex and bounded under the forced Rex5 world.
        assert_eq!(historical_limits.block_state_growth_limit, u64::MAX);
        assert_ne!(forced_limits.block_state_growth_limit, u64::MAX);
        assert_eq!(
            forced_limits,
            BlockLimits::from_hardfork_and_block_gas_limit(MegaHardfork::Rex5, gas_limit),
        );

        // The per-transaction dimensions follow as well, which is what makes the previous
        // per-transaction patch redundant rather than merely subsumed.
        assert_eq!(
            forced_limits.to_evm_tx_runtime_limits(),
            EvmTxRuntimeLimits::from_spec(MegaSpecId::REX5),
        );
    }
}
