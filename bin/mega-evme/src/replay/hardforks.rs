use mega_evm::{
    alloy_hardforks::ForkCondition, revm::primitives::address, MegaHardfork, MegaHardforkConfig,
    SequencerRegistryConfig,
};

/// Returns the hardfork configuration for a given chain ID.
pub fn get_hardfork_config(chain_id: u64) -> MegaHardforkConfig {
    match chain_id {
        // MegaETH testnet v2
        6343 => MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::MiniRex1, ForkCondition::Never)
            .with(MegaHardfork::MiniRex2, ForkCondition::Never)
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(1764694618))
            .with(MegaHardfork::Rex1, ForkCondition::Timestamp(1766147599))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(1770116400))
            .with(MegaHardfork::Rex3, ForkCondition::Timestamp(1771380000))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(1776400000))
            .with(MegaHardfork::Rex5, ForkCondition::Never),
        // MegaETH mainnet
        4326 => MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::MiniRex1, ForkCondition::Timestamp(1764845637))
            .with(MegaHardfork::MiniRex2, ForkCondition::Timestamp(1764849932))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(1764851940))
            .with(MegaHardfork::Rex1, ForkCondition::Timestamp(1766282400))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(1770246000))
            .with(MegaHardfork::Rex3, ForkCondition::Timestamp(1771639200))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(1776659200))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(1780632000))
            // Seeded values read from the on-chain SequencerRegistry
            // (0x6342…0006) at Rex5 init: INITIAL_SEQUENCER (slot 5) and the
            // admin (slot 2). For replay these only satisfy the activation guard
            // — the live system address is read from forked registry storage.
            .with_params(SequencerRegistryConfig {
                rex5_initial_sequencer: address!("0x7a49197dd1ebb8d38c67e4eb7626af6ade432445"),
                rex5_initial_admin: address!("0x92e0e0b15e3e99b32c9ed9ad284f939553c7b7d6"),
            }),
        // Default: all hardforks enabled at genesis
        _ => MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::MiniRex1, ForkCondition::Timestamp(0))
            .with(MegaHardfork::MiniRex2, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex1, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex3, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0)),
    }
}
