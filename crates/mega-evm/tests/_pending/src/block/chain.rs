//! Unit tests extracted from `crates/mega-evm/src/block/chain.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/block/chain.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MegaHardfork, MegaHardforks, MegaSpecId};

    #[test]
    fn test_mainnet_schedule_resolves_specs_by_timestamp() {
        let hf = mainnet_hardforks();
        assert_eq!(hf.spec_id(1764851940), MegaSpecId::REX);
        assert_eq!(hf.spec_id(1776659200), MegaSpecId::REX4);
        // Just before Rex5, still Rex4; at/after Rex5, Rex5.
        assert_eq!(hf.spec_id(1780631999), MegaSpecId::REX4);
        assert_eq!(hf.spec_id(1780632000), MegaSpecId::REX5);
        // Just before Rex6, still Rex5; at/after Rex6, Rex6.
        assert_eq!(hf.spec_id(1787626799), MegaSpecId::REX5);
        assert_eq!(hf.spec_id(1787626800), MegaSpecId::REX6);
        // Rex5 carries the SequencerRegistryConfig, Rex6 the SequencerRegistryRex6Config.
        assert!(hf.fork_params::<SequencerRegistryConfig>().is_some());
        assert!(hf.fork_params::<SequencerRegistryRex6Config>().is_some());
    }

    #[test]
    fn test_testnet_schedule_resolves_specs_by_timestamp() {
        let hf = testnet_hardforks();
        assert_eq!(hf.spec_id(1776400000), MegaSpecId::REX4);
        // Just before Rex5, still Rex4; at/after Rex5, Rex5.
        assert_eq!(hf.spec_id(1780459199), MegaSpecId::REX4);
        assert_eq!(hf.spec_id(1780459200), MegaSpecId::REX5);
        // Just before Rex6, still Rex5; at/after Rex6, Rex6.
        assert_eq!(hf.spec_id(1786330799), MegaSpecId::REX5);
        assert_eq!(hf.spec_id(1786330800), MegaSpecId::REX6);
        // Rex5 carries the SequencerRegistryConfig, Rex6 the SequencerRegistryRex6Config.
        assert!(hf.fork_params::<SequencerRegistryConfig>().is_some());
        assert!(hf.fork_params::<SequencerRegistryRex6Config>().is_some());
    }

    #[test]
    fn test_schedule_dispatch_by_chain_id() {
        assert_eq!(hardfork_schedule(MAINNET_CHAIN_ID).spec_id(1780632000), MegaSpecId::REX5);
        assert_eq!(hardfork_schedule(TESTNET_CHAIN_ID).spec_id(1780459200), MegaSpecId::REX5);
        assert_eq!(hardfork_schedule(MAINNET_CHAIN_ID).spec_id(1787626800), MegaSpecId::REX6);
        assert_eq!(hardfork_schedule(TESTNET_CHAIN_ID).spec_id(1786330800), MegaSpecId::REX6);
        // Unknown chain: every fork up to the pinned rung, active at genesis.
        assert_eq!(hardfork_schedule(1).spec_id(0), MegaSpecId::REX7);
    }

    #[test]
    fn test_unknown_chain_fallback_carries_sequencer_registry_config() {
        // Rex5 block execution fails pre-block without a SequencerRegistryConfig,
        // so the all-activated fallback must attach placeholder roles.
        let hf = hardfork_schedule(999_999);
        assert_eq!(hf.spec_id(0), MegaSpecId::REX7);
        let params = hf
            .fork_params::<SequencerRegistryConfig>()
            .expect("fallback schedule must carry a SequencerRegistryConfig");
        assert_eq!(params.rex5_initial_sequencer, MEGA_SYSTEM_ADDRESS);
        assert_eq!(params.rex5_initial_admin, MEGA_SYSTEM_ADDRESS);
        // Rex6 is also active at genesis in the fallback, and the v2.0.0 registry
        // deploy fails pre-block without a SequencerRegistryRex6Config — without
        // this, every unknown-chain block aborts before the registry can deploy.
        let rex6_params = hf
            .fork_params::<SequencerRegistryRex6Config>()
            .expect("fallback schedule must carry a SequencerRegistryRex6Config");
        assert!(rex6_params.rex6_min_rotation_delay > 0);
    }

    /// The fallback rung is pinned, not inherited from [`MegaSpecId::default`].
    ///
    /// Unknown chains run their rung from genesis, so it is their semantics from block zero with
    /// no fork boundary. Introducing a spec must therefore leave them alone: registering a fork
    /// above the pinned rung would rewrite what history they have already produced means, and
    /// `mega-evme replay` resolves an unknown chain ID through this same schedule.
    ///
    /// A newly introduced spec leaves this test green — the pin holding still is the safe
    /// direction, so nothing fails to force a decision. What the test pins is drift: the
    /// fallback silently following `MegaSpecId::default` again (it fails here once the default
    /// advances past the rung), and the rung advancing without this `RUNG` constant being edited
    /// in the same change.
    #[test]
    fn test_unknown_chain_fallback_pins_its_rung() {
        let hf = all_activated_hardforks();
        const RUNG: MegaSpecId = MegaSpecId::REX7;

        assert_eq!(hf.spec_id(0), RUNG, "the rung applies from genesis");
        assert_eq!(hf.spec_id(u64::MAX), RUNG, "and is terminal — no later fork is registered");

        for fork in MegaHardfork::VARIANTS {
            let registered = hf.mega_fork_activation(*fork) != ForkCondition::Never;
            assert_eq!(
                registered,
                RUNG.is_enabled(fork.spec_id()),
                "{fork:?} registration must follow the pinned rung, not MegaSpecId::default()"
            );
        }
    }
}
