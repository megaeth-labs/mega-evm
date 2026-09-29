//! Environment configuration for mega-evme

use std::str::FromStr;

use alloy_primitives::{Address, B256, U256};
use clap::{Args, Parser};
use std::convert::Infallible;

use mega_evm::{
    alloy_evm::Database,
    revm::{
        context::{block::BlockEnv, cfg::CfgEnv},
        primitives::eip4844,
    },
    AHashBucketHasher, MegaContext, MegaHardforks, MegaSpecId, ProtocolLimits, TestExternalEnvs,
};

/// External environment type for mega-evme using the real AHash-based SALT bucket hasher.
pub type EvmeExternalEnvs = TestExternalEnvs<Infallible, AHashBucketHasher>;
use tracing::{debug, trace};

use super::{parse_limits_override, satin_schedule, EvmeError, LimitsOverride, Result};

/// Chain configuration arguments (spec and chain ID)
#[derive(Args, Debug, Clone)]
#[command(next_help_heading = "Chain Options")]
pub struct ChainArgs {
    /// Name of spec to use. `Satin` runs on the Satin engine; `Equivalence`, `MiniRex`,
    /// `MiniRex1`, `MiniRex2`, `Rex`, `Rex1`, `Rex2`, `Rex3`, `Rex4`, `Rex5`, `Rex6` run on the
    /// legacy engine, the released 1.7.1 (`MiniRex1`/`MiniRex2` are alias specs executing
    /// `Equivalence`/`MiniRex` behavior)
    #[arg(long = "spec", default_value = "Rex6")]
    pub spec: String,

    /// `ChainID` to use
    #[arg(long = "chain-id", visible_aliases = ["chainid"], default_value = "6342")]
    pub chain_id: u64,

    /// Satin only: run under these protocol limits instead of the chain's, a counterfactual. A
    /// JSON object in the shape a chain configuration carries `ProtocolLimits` in (camelCase,
    /// per-transaction limits under `txRuntimeLimits`), inline or in a file; the fields it names
    /// replace the chain's, every other stays. Refused when it names an unknown field or a value
    /// no chain may carry
    #[arg(long = "override.limits", value_name = "JSON|FILE", value_parser = parse_limits_override)]
    pub limits_override: Option<LimitsOverride>,
}

impl ChainArgs {
    /// Gets the spec ID from the spec name. Only `Satin` parses: a legacy spec runs on the
    /// legacy engine, which this crate hands the command to before it gets here.
    pub fn spec_id(&self) -> Result<MegaSpecId> {
        MegaSpecId::from_str(&self.spec)
            .map_err(|e| EvmeError::InvalidInput(format!("Invalid spec name: {e}")))
    }

    /// Creates [`CfgEnv`]. The fields Satin fixes (its gas schedule, the EIP-8037 and EIP-2780
    /// switches, the execution cap, the code-size limits) are set when the context takes it.
    pub fn create_cfg_env(&self) -> Result<CfgEnv<MegaSpecId>> {
        let mut cfg = CfgEnv::new_with_spec(self.spec_id()?);
        cfg.chain_id = self.chain_id;
        debug!(cfg = ?cfg, "Evm CfgEnv created");
        Ok(cfg)
    }

    /// The protocol limits a Satin run of this chain at `timestamp` is held to: those of the
    /// schedule [`satin_schedule`] gives, which for a chain that does not run Satin at `timestamp`
    /// is a counterfactual on [`ProtocolLimits::DEFAULT`] (refused under `--genesis`), with
    /// `--override.limits` over them.
    pub fn protocol_limits(&self, timestamp: u64) -> Result<ProtocolLimits> {
        satin_schedule(self.chain_id, timestamp, self.limits_override.as_ref())?
            .protocol_limits(timestamp)
            .ok_or_else(|| {
                EvmeError::Other(format!("the Satin schedule carries no limits at {timestamp}"))
            })
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
            slot_num: 0,
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
    /// Creates [`EvmeExternalEnvs`].
    pub fn create_external_envs(&self) -> Result<EvmeExternalEnvs> {
        let mut external_envs = EvmeExternalEnvs::new();

        // Parse and configure bucket capacities
        for bucket_capacity_str in &self.bucket_capacity {
            let (bucket_id, capacity) = parse_bucket_capacity(bucket_capacity_str)?;
            external_envs = external_envs.with_bucket_capacity(bucket_id, capacity);
        }
        debug!(external_envs = ?external_envs, "Evm EvmeExternalEnvs created");

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

    /// Creates [`EvmeExternalEnvs`].
    pub fn create_external_envs(&self) -> Result<EvmeExternalEnvs> {
        self.ext.create_external_envs()
    }

    /// The protocol limits a Satin run is held to when `--override.limits` replaced the
    /// schedule's, for its report; `None` without an override.
    pub fn limits_override_in_force(&self) -> Result<Option<Box<ProtocolLimits>>> {
        if self.chain.limits_override.is_none() {
            return Ok(None);
        }
        self.chain.protocol_limits(self.block.block_timestamp).map(|limits| Some(Box::new(limits)))
    }

    /// Creates a [`MegaContext`] with all environment configurations.
    ///
    /// The transaction is held to the per-transaction limits a block of the chain at the block's
    /// timestamp would hold it to ([`ChainArgs::protocol_limits`]), as a node's EVM factory holds
    /// an EVM it builds outside block execution.
    ///
    /// The `system_address` defaults to `MEGA_SYSTEM_ADDRESS`. For `run`/`tx` modes this is
    /// correct: these paths don't go through the block executor and don't resolve from
    /// `SequencerRegistry`. If fork-state simulation with a changed sequencer is needed,
    /// a `--system-address` CLI override can be added in the future.
    pub fn create_evm_context<DB: Database>(
        &self,
        db: DB,
    ) -> Result<MegaContext<DB, EvmeExternalEnvs>> {
        let cfg = self.create_cfg_env()?;
        let block = self.create_block_env()?;
        let external_envs = self.create_external_envs()?;
        let limits = self.chain.protocol_limits(self.block.block_timestamp)?;
        debug!(limits = ?limits.tx_runtime_limits, "Per-transaction limits resolved");

        Ok(MegaContext::new_with_external_envs(db, cfg.spec, external_envs.into())
            .with_cfg(cfg)
            .with_block(block)
            .with_tx_runtime_limits(limits.tx_runtime_limits))
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

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::{revm::database::EmptyDB, MegaEvm};

    /// A `run` or `tx` on Satin is held to the chain's per-transaction limits: on the tool's
    /// default chain, which does not run Satin, the protocol's defaults, where a bare context
    /// would hold it to gas detention's caps alone.
    #[test]
    fn test_a_satin_run_is_held_to_the_protocol_limits() {
        let args = EnvArgs::parse_from(["run", "--spec", "Satin"]);
        let evm = MegaEvm::new(args.create_evm_context(EmptyDB::default()).unwrap());
        assert_eq!(*evm.tx_runtime_limits(), ProtocolLimits::DEFAULT.tx_runtime_limits);
        assert_ne!(
            *MegaEvm::new(MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN))
                .tx_runtime_limits(),
            ProtocolLimits::DEFAULT.tx_runtime_limits,
            "a bare context does not hold the transaction data-size limit"
        );
    }
}
