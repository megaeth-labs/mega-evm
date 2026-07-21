use mega_evm::MegaHardforkConfig;

/// Returns the hardfork configuration for a given chain ID.
///
/// Delegates to the canonical per-chain schedule in `mega-evm`
/// ([`mega_evm::hardfork_schedule`]), which is the single source of truth for
/// `MegaETH` mainnet/testnet activation timestamps.
pub fn get_hardfork_config(chain_id: u64) -> MegaHardforkConfig {
    mega_evm::hardfork_schedule(chain_id)
}
