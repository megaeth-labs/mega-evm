//! Canonical per-chain `MegaETH` hardfork schedules.
//!
//! This is the single source of truth for when each [`MegaHardfork`] activates on
//! the known `MegaETH` chains. Tools that need a real chain schedule (the
//! `mega-evme` replay command, Foundry-based tooling, integration tests) should
//! use [`hardfork_schedule`] rather than re-declaring timestamps locally.
//!
//! Per-fork parameters that are chain-specific data — currently the
//! [`SequencerRegistryConfig`] seeded at Rex5 activation — are attached here too.

use alloy_hardforks::ForkCondition;
use alloy_primitives::address;

use crate::{
    MegaHardfork, MegaHardforkConfig, SequencerRegistryConfig, SequencerRegistryRex6Config,
    MEGA_SYSTEM_ADDRESS,
};

/// `MegaETH` mainnet chain ID.
pub const MAINNET_CHAIN_ID: u64 = 4326;

/// `MegaETH` testnet v2 chain ID.
pub const TESTNET_CHAIN_ID: u64 = 6343;

/// Canonical hardfork schedule for `MegaETH` mainnet (chain `4326`).
pub fn mainnet_hardforks() -> MegaHardforkConfig {
    MegaHardforkConfig::new()
        .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
        .with(MegaHardfork::MiniRex1, ForkCondition::Timestamp(1764845637))
        .with(MegaHardfork::MiniRex2, ForkCondition::Timestamp(1764849932))
        .with(MegaHardfork::Rex, ForkCondition::Timestamp(1764851940))
        .with(MegaHardfork::Rex1, ForkCondition::Timestamp(1766282400))
        .with(MegaHardfork::Rex2, ForkCondition::Timestamp(1770246000))
        .with(MegaHardfork::Rex3, ForkCondition::Timestamp(1771639200))
        .with(MegaHardfork::Rex4, ForkCondition::Timestamp(1776659200))
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(1780632000))
        // Seeded values read from the on-chain SequencerRegistry (0x6342…0006) at
        // Rex5 init: INITIAL_SEQUENCER (slot 5) and the admin (slot 2). At replay
        // time these only satisfy the activation guard — the live system address
        // is read from forked registry storage.
        .with_params(SequencerRegistryConfig {
            rex5_initial_sequencer: address!("0x7a49197dd1ebb8d38c67e4eb7626af6ade432445"),
            rex5_initial_admin: address!("0x92e0e0b15e3e99b32c9ed9ad284f939553c7b7d6"),
        })
}

/// Canonical hardfork schedule for `MegaETH` testnet v2 (chain `6343`).
pub fn testnet_hardforks() -> MegaHardforkConfig {
    MegaHardforkConfig::new()
        .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
        .with(MegaHardfork::MiniRex1, ForkCondition::Never)
        .with(MegaHardfork::MiniRex2, ForkCondition::Never)
        .with(MegaHardfork::Rex, ForkCondition::Timestamp(1764694618))
        .with(MegaHardfork::Rex1, ForkCondition::Timestamp(1766147599))
        .with(MegaHardfork::Rex2, ForkCondition::Timestamp(1770116400))
        .with(MegaHardfork::Rex3, ForkCondition::Timestamp(1771380000))
        .with(MegaHardfork::Rex4, ForkCondition::Timestamp(1776400000))
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(1780459200))
        // Seeded values read from the on-chain SequencerRegistry (0x6342…0006) at
        // Rex5 init on testnet: INITIAL_SEQUENCER (slot 5) and the admin (slot 2).
        // At replay time these only satisfy the activation guard — the live system
        // address is read from forked registry storage.
        .with_params(SequencerRegistryConfig {
            rex5_initial_sequencer: address!("0xB8DB54eBA7Ae650d14F362de461516a4FF1551FC"),
            rex5_initial_admin: address!("0x1d9BD232C44B39341e670B735c7F423c40426b34"),
        })
}

/// All `MegaETH` hardforks activated at genesis.
///
/// Used for unknown chains and local/standalone execution where the chain has no
/// published schedule. Rex5 requires a [`SequencerRegistryConfig`] (block
/// execution fails pre-block without one), and an unknown chain has no published
/// roles, so both roles are seeded with [`MEGA_SYSTEM_ADDRESS`] as a
/// placeholder. The placeholder only matters when bootstrapping a fresh
/// registry: on a chain whose registry is already deployed, the live roles are
/// read from the registry's storage.
///
/// Rex6 (active at genesis here too) likewise requires a
/// [`SequencerRegistryRex6Config`] for the v2.0.0 registry deploy. The fallback
/// seeds the smallest valid rotation delay (1 block) so local and dev chains can
/// exercise rotations without friction; a real network must attach a
/// governance-approved value in its published schedule when it schedules Rex6.
pub fn all_activated_hardforks() -> MegaHardforkConfig {
    MegaHardforkConfig::new()
        .with_all_activated()
        .with_params(SequencerRegistryConfig {
            rex5_initial_sequencer: MEGA_SYSTEM_ADDRESS,
            rex5_initial_admin: MEGA_SYSTEM_ADDRESS,
        })
        .with_params(SequencerRegistryRex6Config { rex6_min_rotation_delay: 1 })
}

/// Returns the canonical hardfork schedule for a chain ID.
///
/// Mainnet (`4326`) and testnet v2 (`6343`) use their published schedules; any
/// other chain gets [`all_activated_hardforks`].
pub fn hardfork_schedule(chain_id: u64) -> MegaHardforkConfig {
    match chain_id {
        MAINNET_CHAIN_ID => mainnet_hardforks(),
        TESTNET_CHAIN_ID => testnet_hardforks(),
        _ => all_activated_hardforks(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MegaHardforks, MegaSpecId};

    #[test]
    fn test_mainnet_schedule_resolves_specs_by_timestamp() {
        let hf = mainnet_hardforks();
        assert_eq!(hf.spec_id(1764851940), MegaSpecId::REX);
        assert_eq!(hf.spec_id(1776659200), MegaSpecId::REX4);
        // Just before Rex5, still Rex4; at/after Rex5, Rex5.
        assert_eq!(hf.spec_id(1780631999), MegaSpecId::REX4);
        assert_eq!(hf.spec_id(1780632000), MegaSpecId::REX5);
        // Rex5 carries the SequencerRegistryConfig.
        assert!(hf.fork_params::<SequencerRegistryConfig>().is_some());
    }

    #[test]
    fn test_testnet_schedule_resolves_specs_by_timestamp() {
        let hf = testnet_hardforks();
        assert_eq!(hf.spec_id(1776400000), MegaSpecId::REX4);
        // Just before Rex5, still Rex4; at/after Rex5, Rex5.
        assert_eq!(hf.spec_id(1780459199), MegaSpecId::REX4);
        assert_eq!(hf.spec_id(1780459200), MegaSpecId::REX5);
        // Rex5 carries the SequencerRegistryConfig.
        assert!(hf.fork_params::<SequencerRegistryConfig>().is_some());
    }

    #[test]
    fn test_schedule_dispatch_by_chain_id() {
        assert_eq!(hardfork_schedule(MAINNET_CHAIN_ID).spec_id(1780632000), MegaSpecId::REX5);
        assert_eq!(hardfork_schedule(TESTNET_CHAIN_ID).spec_id(1780459200), MegaSpecId::REX5);
        // Unknown chain: everything active at genesis, including the unstable REX6.
        assert_eq!(hardfork_schedule(1).spec_id(0), MegaSpecId::REX6);
    }

    #[test]
    fn test_unknown_chain_fallback_carries_sequencer_registry_config() {
        // Rex5 block execution fails pre-block without a SequencerRegistryConfig,
        // so the all-activated fallback must attach placeholder roles.
        let hf = hardfork_schedule(999_999);
        assert_eq!(hf.spec_id(0), MegaSpecId::REX6);
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
}
