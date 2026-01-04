//! Environment configuration for mega-evme

use std::str::FromStr;

use alloy_primitives::{Address, B256, U256};
use clap::{Args, Parser};
use mega_evm::{
    alloy_evm::Database,
    revm::{
        context::{block::BlockEnv, cfg::CfgEnv},
        primitives::eip4844,
    },
    MegaContext, MegaSpecId, TestExternalEnvs,
};
use tracing::{debug, trace};

use super::{EvmeError, Result};

/// Chain configuration arguments (spec and chain ID)
#[derive(Args, Debug, Clone)]
#[command(next_help_heading = "Chain Options")]
pub struct ChainArgs {
    /// Name of spec to use, possible values: `MiniRex`, `Equivalence`, `Rex`, `Rex1`, `Rex2`
    #[arg(long = "spec", default_value = "Rex2")]
    pub spec: String,

    /// `ChainID` to use
    #[arg(long = "chain-id", visible_aliases = ["chainid"], default_value = "6342")]
    pub chain_id: u64,
}

impl ChainArgs {
    /// Gets the spec ID from the spec name
    pub fn spec_id(&self) -> Result<MegaSpecId> {
        MegaSpecId::from_str(&self.spec)
            .map_err(|e| EvmeError::InvalidInput(format!("Invalid spec name: {:?}", e)))
    }

    /// Creates [`CfgEnv`].
    pub fn create_cfg_env(&self) -> Result<CfgEnv<MegaSpecId>> {
        let mut cfg = CfgEnv::default();
        cfg.chain_id = self.chain_id;
        cfg.spec = self.spec_id()?;
        debug!(cfg = ?cfg, "Evm CfgEnv created");
        Ok(cfg)
    }
}

/// Block environment configuration arguments
#[derive(Args, Debug, Clone)]
#[command(next_help_heading = "Block Options")]
pub struct BlockEnvArgs {
    /// Block number
    #[arg(long = "block.number", default_value = "1")]
    pub block_number: u64,

    /// Block coinbase/beneficiary address
    #[arg(long = "block.coinbase", visible_aliases = ["block.beneficiary"], default_value = "0x0000000000000000000000000000000000000000")]
    pub block_coinbase: Address,

    /// Block timestamp
    #[arg(long = "block.timestamp", default_value = "1")]
    pub block_timestamp: u64,

    /// Block gas limit
    #[arg(long = "block.gaslimit", visible_aliases = ["block.gas-limit", "block.gas"], default_value = "10000000000")]
    pub block_gas_limit: u64,

    /// Block base fee per gas (EIP-1559)
    #[arg(long = "block.basefee", visible_aliases = ["block.base-fee"], default_value = "0")]
    pub block_basefee: u64,

    /// Block difficulty
    #[arg(long = "block.difficulty", default_value = "0")]
    pub block_difficulty: U256,

    /// Block prevrandao (replaces difficulty post-merge). Required for post-merge blocks.
    #[arg(
        long = "block.prevrandao",
        visible_aliases = ["block.random"],
        default_value = "0x0000000000000000000000000000000000000000000000000000000000000000"
    )]
    pub block_prevrandao: B256,

    /// Excess blob gas for EIP-4844. Required for Cancun and later forks.
    #[arg(long = "block.blobexcessgas", visible_aliases = ["block.blob-excess-gas"], default_value = "0")]
    pub block_blob_excess_gas: Option<u64>,
}

impl BlockEnvArgs {
    /// Creates [`BlockEnv`].
    pub fn create_block_env(&self) -> Result<BlockEnv> {
        let mut block = BlockEnv {
            number: U256::from(self.block_number),
            beneficiary: self.block_coinbase,
            timestamp: U256::from(self.block_timestamp),
            gas_limit: self.block_gas_limit,
            basefee: self.block_basefee,
            difficulty: self.block_difficulty,
            prevrandao: Some(self.block_prevrandao),
            blob_excess_gas_and_price: None,
        };

        // Set blob excess gas if provided
        if let Some(excess_gas) = self.block_blob_excess_gas {
            block.set_blob_excess_gas_and_price(
                excess_gas,
                eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            );
        }
        debug!(block = ?block, "Evm BlockEnv created");

        Ok(block)
    }
}

/// External environment configuration arguments (SALT bucket capacity)
#[derive(Args, Debug, Clone)]
#[command(next_help_heading = "External Environment Options")]
pub struct ExtEnvArgs {
    /// Bucket capacity configuration in format "`bucket_id:capacity`"
    /// Can be specified multiple times for different buckets.
    /// Example: --bucket-capacity 123:1000000 --bucket-capacity 456:2000000
    #[arg(long = "bucket-capacity", value_name = "BUCKET_ID:CAPACITY")]
    pub bucket_capacity: Vec<String>,
}

impl ExtEnvArgs {
    /// Creates [`TestExternalEnvs`].
    pub fn create_external_envs(&self) -> Result<TestExternalEnvs> {
        let mut external_envs = TestExternalEnvs::new();

        // Parse and configure bucket capacities
        for bucket_capacity_str in &self.bucket_capacity {
            let (bucket_id, capacity) = parse_bucket_capacity(bucket_capacity_str)?;
            external_envs = external_envs.with_bucket_capacity(bucket_id, capacity);
        }
        debug!(external_envs = ?external_envs, "Evm TestExternalEnvs created");

        Ok(external_envs)
    }
}

/// Environment configuration arguments (chain config, block env, SALT bucket capacity)
#[derive(Parser, Debug, Clone)]
pub struct EnvArgs {
    /// Chain configuration
    #[command(flatten)]
    pub chain: ChainArgs,

    /// Block environment configuration
    #[command(flatten)]
    pub block: BlockEnvArgs,

    /// External environment configuration
    #[command(flatten)]
    pub ext: ExtEnvArgs,
}

impl EnvArgs {
    /// Gets the spec ID from the spec name
    pub fn spec_id(&self) -> Result<MegaSpecId> {
        self.chain.spec_id()
    }

    /// Creates [`CfgEnv`].
    pub fn create_cfg_env(&self) -> Result<CfgEnv<MegaSpecId>> {
        self.chain.create_cfg_env()
    }

    /// Creates [`BlockEnv`].
    pub fn create_block_env(&self) -> Result<BlockEnv> {
        self.block.create_block_env()
    }

    /// Creates [`TestExternalEnvs`].
    pub fn create_external_envs(&self) -> Result<TestExternalEnvs> {
        self.ext.create_external_envs()
    }

    /// Creates a [`MegaContext`] with all environment configurations.
    pub fn create_evm_context<DB: Database>(
        &self,
        db: DB,
    ) -> Result<MegaContext<DB, TestExternalEnvs>> {
        let cfg = self.create_cfg_env()?;
        let block = self.create_block_env()?;
        let external_envs = self.create_external_envs()?;

        Ok(MegaContext::new(db, cfg.spec)
            .with_cfg(cfg)
            .with_block(block)
            .with_external_envs(external_envs.into()))
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

    trace!(string = %s, bucket_id = %bucket_id, capacity = %capacity, "Parsed bucket capacity");
    Ok((bucket_id, capacity))
}
