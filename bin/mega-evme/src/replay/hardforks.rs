use mega_evm::{alloy_hardforks::ForkCondition, MegaHardfork, MegaHardforkConfig};

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
            .with(MegaHardfork::Rex3, ForkCondition::Never)
            .with(MegaHardfork::Rex4, ForkCondition::Never),
        // MegaETH mainnet
        4326 => MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::MiniRex1, ForkCondition::Timestamp(1764845637))
            .with(MegaHardfork::MiniRex2, ForkCondition::Timestamp(1764849932))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(1764851940))
            .with(MegaHardfork::Rex1, ForkCondition::Timestamp(1766282400))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(1770246000))
            .with(MegaHardfork::Rex3, ForkCondition::Never)
            .with(MegaHardfork::Rex4, ForkCondition::Never),
        // Default: all hardforks enabled at genesis
        _ => MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::MiniRex1, ForkCondition::Timestamp(0))
            .with(MegaHardfork::MiniRex2, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex1, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex3, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(0)),
    }
}
