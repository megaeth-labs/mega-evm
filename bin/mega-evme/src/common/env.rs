//! Environment configuration for mega-evme

use std::{path::PathBuf, str::FromStr};

use alloy_primitives::{Address, U256};
use clap::Parser;
use mega_evm::{
    revm::{
        context::{block::BlockEnv, cfg::CfgEnv},
        primitives::eip4844,
    },
    MegaSpecId, TestExternalEnvs,
};

use super::{EvmeError, Result};

/// Environment configuration arguments (chain config, block env, SALT bucket capacity)
#[derive(Parser, Debug, Clone)]
pub struct EnvArgs {
    /// Name of hardfork to use, possible values: `MiniRex`, `Equivalence`, `Rex`
    #[arg(long = "state.fork", default_value = "MiniRex")]
    pub hardfork: String,

    /// `ChainID` to use
    #[arg(long = "state.chainid", default_value = "6342")]
    pub chain_id: u64,

    // BlockEnv configuration
    /// Block number
    #[arg(long = "block.number", default_value = "1")]
    pub block_number: u64,

    /// Block coinbase/beneficiary address
    #[arg(long = "block.coinbase", default_value = "0x0000000000000000000000000000000000000000")]
    pub block_coinbase: Address,

    /// Block timestamp
    #[arg(long = "block.timestamp", default_value = "1")]
    pub block_timestamp: u64,

    /// Block gas limit
    #[arg(long = "block.gaslimit", default_value = "10000000000")]
    pub block_gas_limit: u64,

    /// Block base fee per gas (EIP-1559)
    #[arg(long = "block.basefee", default_value = "0")]
    pub block_basefee: u64,

    /// Block difficulty
    #[arg(long = "block.difficulty", default_value = "0")]
    pub block_difficulty: U256,

    /// Block prevrandao (replaces difficulty post-merge). Required for post-merge blocks.
    #[arg(
        long = "block.prevrandao",
        default_value = "0x0000000000000000000000000000000000000000000000000000000000000000"
    )]
    pub block_prevrandao: Option<String>,

    /// Excess blob gas for EIP-4844. Required for Cancun and later forks.
    #[arg(long = "block.blobexcessgas", default_value = "0")]
    pub block_blob_excess_gas: Option<u64>,

    // SALT bucket capacity configuration
    /// Bucket capacity configuration in format "`bucket_id:capacity`"
    /// Can be specified multiple times for different buckets.
    /// Example: --bucket-capacity 123:1000000 --bucket-capacity 456:2000000
    #[arg(long = "bucket-capacity", value_name = "BUCKET_ID:CAPACITY")]
    pub bucket_capacity: Vec<String>,
}

impl EnvArgs {
    /// Gets the spec ID from the hardfork name
    pub fn spec_id(&self) -> Result<MegaSpecId> {
        MegaSpecId::from_str(&self.hardfork)
            .map_err(|e| EvmeError::InvalidInput(format!("Invalid hardfork name: {:?}", e)))
    }

    /// Creates [`CfgEnv`].
    pub fn create_cfg_env(&self) -> Result<CfgEnv<MegaSpecId>> {
        let mut cfg = CfgEnv::default();
        cfg.chain_id = self.chain_id;
        cfg.spec = self.spec_id()?;
        Ok(cfg)
    }

    /// Creates [`BlockEnv`].
    pub fn create_block_env(&self) -> Result<BlockEnv> {
        let mut block = BlockEnv {
            number: U256::from(self.block_number),
            beneficiary: self.block_coinbase,
            timestamp: U256::from(self.block_timestamp),
            gas_limit: self.block_gas_limit,
            basefee: self.block_basefee,
            difficulty: self.block_difficulty,
            prevrandao: self.block_prevrandao.as_ref().and_then(|s| {
                let trimmed = s.trim().trim_start_matches("0x");
                alloy_primitives::FixedBytes::from_str(trimmed).ok()
            }),
            blob_excess_gas_and_price: None,
        };

        // Set blob excess gas if provided
        if let Some(excess_gas) = self.block_blob_excess_gas {
            block.set_blob_excess_gas_and_price(
                excess_gas,
                eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            );
        }

        Ok(block)
    }

    /// Creates [`TestExternalEnvs`].
    pub fn create_external_envs(&self) -> Result<TestExternalEnvs> {
        let mut external_envs = TestExternalEnvs::new();

        // Parse and configure bucket capacities
        for bucket_capacity_str in &self.bucket_capacity {
            let (bucket_id, capacity) = parse_bucket_capacity(&bucket_capacity_str)?;
            external_envs = external_envs.with_bucket_capacity(bucket_id, capacity);
        }

        Ok(external_envs)
    }
}

/// Parse bucket capacity string in format "`bucket_id:capacity`"
/// Returns (`bucket_id`, capacity) tuple
pub fn parse_bucket_capacity(s: &str) -> Result<(u32, u64)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(EvmeError::InvalidInput(format!(
            "Invalid bucket capacity format: '{}'. Expected format: 'bucket_id:capacity'",
            s
        )));
    }

    let bucket_id = parts[0]
        .parse::<u32>()
        .map_err(|e| EvmeError::InvalidInput(format!("Invalid bucket ID '{}': {}", parts[0], e)))?;

    let capacity = parts[1]
        .parse::<u64>()
        .map_err(|e| EvmeError::InvalidInput(format!("Invalid capacity '{}': {}", parts[1], e)))?;

    Ok((bucket_id, capacity))
}
