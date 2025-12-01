//! Common argument groups and functions shared between run and tx commands

use std::path::PathBuf;

use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_trace::geth::GethDefaultTracingOptions;
use clap::Parser;
use mega_evm::{
    revm::{
        context::{block::BlockEnv, cfg::CfgEnv, result::ExecutionResult},
        primitives::{eip4844, HashMap, KECCAK_EMPTY},
        state::{AccountInfo, Bytecode, EvmState},
        ExecuteEvm, InspectEvm,
    },
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, TestExternalEnvs,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use super::{parse_bucket_capacity, EvmeState, Result, RunError, StateDump, TracerType};

/// Pre-execution state configuration arguments
#[derive(Parser, Debug, Clone)]
pub struct PreStateArgs {
    /// Fork state from a remote RPC endpoint.
    #[arg(long = "fork")]
    pub fork: bool,

    /// Block number of the state (post-block state) to fork from. If not specified, the latest
    /// block is used. Only used if `fork` is true.
    #[arg(long = "fork.block")]
    pub fork_block: Option<u64>,

    /// RPC URL to use for the fork. Only used if `fork` is true.
    #[arg(long = "fork.rpc", default_value = "http://localhost:8545")]
    pub fork_rpc: String,

    /// JSON file with prestate (genesis) config
    #[arg(long = "prestate")]
    pub prestate: Option<PathBuf>,

    /// Balance to allocate to the sender account
    /// If not specified, sender balance is not set (fallback to `prestate` if specified,
    /// otherwise 0)
    #[arg(long = "sender.balance", visible_aliases = ["from.balance"])]
    pub sender_balance: Option<U256>,
}

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
    pub fn spec_id(&self) -> MegaSpecId {
        spec_id(&self.hardfork)
    }
}

/// State dump configuration arguments
#[derive(Parser, Debug, Clone)]
pub struct StateDumpArgs {
    /// Dumps the state after the run
    #[arg(long = "dump")]
    pub dump: bool,

    /// Output file for state dump (if not specified, prints to console)
    #[arg(long = "dump.output")]
    pub dump_output_file: Option<PathBuf>,
}

/// Trace configuration arguments
#[derive(Parser, Debug, Clone)]
pub struct TraceArgs {
    /// Tracer to enable during execution
    #[arg(long = "tracer", value_enum)]
    pub tracer: Option<super::TracerType>,

    /// Disable memory capture in traces
    #[arg(long = "trace.disable-memory")]
    pub trace_disable_memory: bool,

    /// Disable stack capture in traces
    #[arg(long = "trace.disable-stack")]
    pub trace_disable_stack: bool,

    /// Disable storage capture in traces
    #[arg(long = "trace.disable-storage")]
    pub trace_disable_storage: bool,

    /// Enable return data capture in traces
    #[arg(long = "trace.enable-return-data")]
    pub trace_enable_return_data: bool,

    /// Output file for trace data (if not specified, prints to console)
    #[arg(long = "trace.output")]
    pub trace_output_file: Option<PathBuf>,
}

/// Parse hardfork name to spec ID
pub fn spec_id(fork: &str) -> MegaSpecId {
    use std::str::FromStr;
    MegaSpecId::from_str(fork).expect("Invalid hardfork name")
}

/// Load prestate from file and sender balance into prestate maps
pub fn load_prestate(
    prestate_args: &PreStateArgs,
    sender: Address,
) -> Result<(HashMap<Address, AccountInfo>, HashMap<Address, HashMap<U256, U256>>)> {
    // Collect prestate overrides
    let mut prestate = HashMap::default();
    let mut storage = HashMap::default();

    // Load prestate from JSON file if provided
    if let Some(ref prestate_path) = prestate_args.prestate {
        let prestate_content = std::fs::read_to_string(prestate_path)?;
        let state_dump: StateDump = serde_json::from_str(&prestate_content)
            .map_err(|e| RunError::InvalidInput(format!("Failed to parse prestate JSON: {}", e)))?;

        // Convert StateDump to prestate maps
        for (address, account_state) in state_dump.accounts {
            // Create bytecode from code bytes
            let bytecode = if account_state.code.is_empty() {
                Bytecode::default()
            } else {
                Bytecode::new_raw_checked(account_state.code.clone())
                    .unwrap_or_else(|_| Bytecode::new_legacy(account_state.code.clone()))
            };
            let code_hash = bytecode.hash_slow();
            assert_eq!(
                code_hash, account_state.code_hash,
                "Code hash mismatch for account {}",
                address
            );

            // Create account info
            let account_info = AccountInfo {
                balance: account_state.balance,
                code_hash,
                code: Some(bytecode),
                nonce: account_state.nonce,
            };

            prestate.insert(address, account_info);

            // Store storage - convert to revm HashMap
            if !account_state.storage.is_empty() {
                let revm_storage: HashMap<U256, U256> = account_state.storage.into_iter().collect();
                storage.insert(address, revm_storage);
            }
        }
    }

    // Set balance for sender if specified (overrides prestate)
    if let Some(balance) = prestate_args.sender_balance {
        let sender_info = AccountInfo {
            balance,
            code_hash: KECCAK_EMPTY,
            code: Some(Bytecode::default()),
            nonce: 0,
        };
        prestate.insert(sender, sender_info);
    }

    Ok((prestate, storage))
}

/// Create initial state from prestate and optional provider
pub async fn create_initial_state<N, P>(
    provider: Option<P>,
    fork_block: Option<u64>,
    prestate: HashMap<Address, AccountInfo>,
    storage: HashMap<Address, HashMap<U256, U256>>,
) -> Result<EvmeState<N, P>>
where
    N: alloy_network::Network,
    P: alloy_provider::Provider<N> + Clone + std::fmt::Debug,
{
    // Create the appropriate state based on whether provider is provided
    if let Some(provider) = provider {
        EvmeState::new_forked(provider, fork_block, prestate, storage).await
    } else {
        Ok(EvmeState::new_empty(prestate, storage))
    }
}

/// Setup configuration environment
pub fn setup_cfg_env(env_args: &EnvArgs) -> CfgEnv<MegaSpecId> {
    let mut cfg = CfgEnv::default();
    cfg.chain_id = env_args.chain_id;
    cfg.spec = spec_id(&env_args.hardfork);
    cfg
}

/// Setup block environment
pub fn setup_block_env(env_args: &EnvArgs) -> BlockEnv {
    use std::str::FromStr;

    let mut block = BlockEnv {
        number: U256::from(env_args.block_number),
        beneficiary: env_args.block_coinbase,
        timestamp: U256::from(env_args.block_timestamp),
        gas_limit: env_args.block_gas_limit,
        basefee: env_args.block_basefee,
        difficulty: env_args.block_difficulty,
        prevrandao: env_args.block_prevrandao.as_ref().and_then(|s| {
            let trimmed = s.trim().trim_start_matches("0x");
            alloy_primitives::FixedBytes::from_str(trimmed).ok()
        }),
        blob_excess_gas_and_price: None,
    };

    // Set blob excess gas if provided
    if let Some(excess_gas) = env_args.block_blob_excess_gas {
        block.set_blob_excess_gas_and_price(
            excess_gas,
            eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
        );
    }

    block
}

/// Setup external environments with bucket capacity configuration
pub fn setup_external_envs(bucket_capacity: &[String]) -> Result<TestExternalEnvs> {
    let mut external_envs = TestExternalEnvs::new();

    // Parse and configure bucket capacities
    for bucket_capacity_str in bucket_capacity {
        let (bucket_id, capacity) = parse_bucket_capacity(bucket_capacity_str)?;
        external_envs = external_envs.with_bucket_capacity(bucket_id, capacity);
    }

    Ok(external_envs)
}

/// Dump final state as JSON
pub fn dump_state(evm_state: &EvmState, dump_args: &StateDumpArgs) -> Result<()> {
    // Create state dump from EVM state
    let state_dump = StateDump::from_evm_state(evm_state);

    // Serialize the state dump as pretty JSON
    let state_json = serde_json::to_string_pretty(&state_dump)
        .map_err(|e| RunError::ExecutionError(format!("Failed to serialize state: {}", e)))?;

    // Output to file or console
    if let Some(ref output_file) = dump_args.dump_output_file {
        // Write state to file
        std::fs::write(output_file, state_json).map_err(|e| {
            RunError::ExecutionError(format!("Failed to write state to file: {}", e))
        })?;
        eprintln!();
        eprintln!("State dump written to: {}", output_file.display());
    } else {
        // Print state to console
        eprintln!();
        eprintln!("=== State Dump ===");
        eprintln!("{}", state_json);
    }

    Ok(())
}

/// Execute transaction with optional tracing
pub fn execute_transaction<N, P>(
    evm_context: MegaContext<&mut EvmeState<N, P>, TestExternalEnvs>,
    tx: MegaTransaction,
    trace_args: &TraceArgs,
) -> Result<(ExecutionResult<mega_evm::MegaHaltReason>, EvmState, Option<String>)>
where
    N: alloy_network::Network,
    P: alloy_provider::Provider<N> + std::fmt::Debug,
{
    if matches!(trace_args.tracer, Some(TracerType::Trace)) {
        // Execute with tracing inspector
        let config = TracingInspectorConfig::all();
        let mut inspector = TracingInspector::new(config);
        let mut evm = MegaEvm::new(evm_context).with_inspector(&mut inspector);

        let result_and_state =
            if trace_args.tracer.is_some() { evm.inspect_tx(tx) } else { evm.transact(tx) }
                .map_err(|e| RunError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;

        // Generate GethTrace using GethTraceBuilder
        let geth_builder = inspector.geth_builder();

        // Create GethDefaultTracingOptions based on CLI arguments
        let opts = GethDefaultTracingOptions {
            disable_storage: Some(trace_args.trace_disable_storage),
            disable_memory: Some(trace_args.trace_disable_memory),
            disable_stack: Some(trace_args.trace_disable_stack),
            enable_return_data: Some(trace_args.trace_enable_return_data),
            ..Default::default()
        };

        // Get output for trace generation
        let output = match &result_and_state.result {
            ExecutionResult::Success { output, .. } => output.data().to_vec(),
            ExecutionResult::Revert { output, .. } => output.to_vec(),
            _ => Vec::new(),
        };

        // Generate the geth trace
        let geth_trace =
            geth_builder.geth_traces(result_and_state.result.gas_used(), Bytes::from(output), opts);

        // Format as JSON
        let trace_str = serde_json::to_string_pretty(&geth_trace)
            .unwrap_or_else(|e| format!("Error serializing trace: {}", e));

        Ok((result_and_state.result, result_and_state.state, Some(trace_str)))
    } else {
        // Execute without tracing
        let mut evm = MegaEvm::new(evm_context);
        let result_and_state = evm
            .transact(tx)
            .map_err(|e| RunError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;

        Ok((result_and_state.result, result_and_state.state, None))
    }
}
