//! The `alloy_evm::Evm` accessors of `MegaEvm` report the environment the EVM was built with.

use alloy_evm::{EvmEnv, EvmFactory};
use mega_evm::{test_utils::MemoryDatabase, MegaEvmFactory, MegaSpecId, MAINNET_CHAIN_ID};
use revm::context::{BlockEnv, CfgEnv};

/// `chain_id()` is the configured chain id, not mainnet Ethereum's `1` that a default `CfgEnv`
/// carries.
#[test]
fn test_chain_id_reports_the_configured_chain_id() {
    let mut cfg_env = CfgEnv::default();
    cfg_env.spec = MegaSpecId::EQUIVALENCE;
    cfg_env.chain_id = MAINNET_CHAIN_ID;
    let evm = MegaEvmFactory::new()
        .create_evm(MemoryDatabase::default(), EvmEnv::new(cfg_env, BlockEnv::default()));

    assert_eq!(alloy_evm::Evm::chain_id(&evm), MAINNET_CHAIN_ID);
    assert_eq!(alloy_evm::Evm::cfg_env(&evm).chain_id, MAINNET_CHAIN_ID);
}
