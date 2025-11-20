use std::{
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

use alloy_primitives::{hex, Address, Bytes, U256};
use alloy_rpc_types_trace::geth::GethDefaultTracingOptions;
use clap::{Parser, ValueEnum};
use mega_evm::{
    revm::{
        context::{block::BlockEnv, cfg::CfgEnv, result::ExecutionResult, tx::TxEnv},
        database::{CacheState, EmptyDB, State},
        primitives::{eip4844, hardfork::SpecId, TxKind, KECCAK_EMPTY},
        state::{AccountInfo, Bytecode, EvmState},
        ExecuteEvm, InspectEvm,
    },
    DefaultExternalEnvs, HashMap, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use super::{load_code, load_input, Result};

/// Tracer type for execution analysis
#[derive(Debug, Clone, Copy, ValueEnum)]
#[non_exhaustive]
pub enum TracerType {
    /// Enable execution tracing (opcode-level trace in Geth format)
    Trace,
}

/// Run arbitrary EVM bytecode
#[derive(Parser, Debug)]
pub struct Cmd {
    /// EVM bytecode as hex string (positional argument)
    #[arg(value_name = "CODE")]
    pub code: Option<String>,

    /// File containing EVM code. If '-' is specified, code is read from stdin
    #[arg(long = "codefile")]
    pub codefile: Option<String>,

    /// Indicates the action should be create rather than call
    #[arg(long = "create")]
    pub create: bool,

    /// Gas limit for the evm
    #[arg(long = "gas", default_value = "10000000")]
    pub gas: u64,

    /// JSON file with prestate (genesis) config
    #[arg(long = "prestate")]
    pub prestate: Option<PathBuf>,

    /// Input for the EVM (hex string)
    #[arg(long = "input")]
    pub input: Option<String>,

    /// File containing input for the EVM
    #[arg(long = "inputfile")]
    pub inputfile: Option<PathBuf>,

    /// Price set for the evm (gas price)
    #[arg(long = "price", default_value = "0")]
    pub price: u64,

    /// Gas priority fee (EIP-1559)
    #[arg(long = "priorityfee")]
    pub priority_fee: Option<u64>,

    /// Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, etc.)
    #[arg(long = "tx-type", default_value = "0")]
    pub tx_type: u8,

    /// The transaction receiver (execution context)
    #[arg(long = "receiver", default_value = "0x0000000000000000000000000000000000000000")]
    pub receiver: Address,

    /// The transaction origin
    #[arg(long = "sender", default_value = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")]
    pub sender: Address,

    /// Value set for the evm
    #[arg(long = "value", default_value = "0")]
    pub value: U256,

    /// Balance to allocate to the sender account
    /// If not specified, sender balance is not set (remains at 0)
    #[arg(long = "sender.balance")]
    pub sender_balance: Option<U256>,

    /// Dumps the state after the run
    #[arg(long = "dump")]
    pub dump: bool,

    /// Output file for state dump (if not specified, prints to console)
    #[arg(long = "dump.output")]
    pub dump_output_file: Option<PathBuf>,

    /// Benchmark the execution
    #[arg(long = "bench")]
    pub bench: bool,

    /// Tracer to enable during execution
    #[arg(long = "tracer", value_enum)]
    pub tracer: Option<TracerType>,

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

    /// Name of hardfork to use, possible values: `MiniRex`, `Equivalence`, `Rex`
    #[arg(long = "state.fork", default_value = "MiniRex")]
    pub fork: String,

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
    #[arg(long = "block.gaslimit", default_value = "30000000")]
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

/// Execution result with optional trace data and state
struct RunResult {
    exec_result: ExecutionResult<mega_evm::MegaHaltReason>,
    state: EvmState,
    exec_time: Duration,
    trace_data: Option<String>,
}

impl Cmd {
    fn spec_id(&self) -> MegaSpecId {
        MegaSpecId::from_str(&self.fork).expect("Invalid hardfork name")
    }

    /// Execute the run command
    pub fn run(&self) -> Result<()> {
        // Step 1: Load bytecode
        let code = load_code(self.code.clone(), self.codefile.clone())?;

        // Step 2: Load input data
        let input = load_input(
            self.input.clone(),
            self.inputfile.as_ref().map(|p| p.to_string_lossy().to_string()),
        )?;

        // Step 3: Setup initial state and environment
        let mut state = self.create_initial_state(&code)?;

        // Step 4: Execute bytecode
        let result = if self.bench {
            self.execute_benchmark(&mut state, &code, &input)?
        } else {
            self.execute_once(&mut state, &code, &input)?
        };

        // Step 5: Output results (including state dump if requested)
        self.output_results(&result)?;

        Ok(())
    }

    /// Load prestate from JSON file and populate cache state
    fn load_prestate(
        &self,
        prestate_path: &std::path::Path,
        cache_state: &mut CacheState,
    ) -> Result<()> {
        // Read and parse JSON file using serde
        let prestate_content = std::fs::read_to_string(prestate_path)?;
        let state_dump: super::StateDump =
            serde_json::from_str(&prestate_content).map_err(|e| {
                super::RunError::InvalidInput(format!("Failed to parse prestate JSON: {}", e))
            })?;

        // Iterate over all accounts in prestate
        for (address, account_state) in state_dump.accounts {
            // Create bytecode from code bytes
            let bytecode = if account_state.code.is_empty() {
                Bytecode::default()
            } else {
                Bytecode::new_raw_checked(account_state.code.clone())
                    .unwrap_or_else(|_| Bytecode::new_legacy(account_state.code.clone()))
            };
            let code_hash = bytecode.hash_slow();

            // Create account info
            let account_info = AccountInfo {
                balance: account_state.balance,
                code_hash,
                code: Some(bytecode),
                nonce: account_state.nonce,
            };

            // Convert storage HashMap to revm's HashMap
            let storage: HashMap<_, _> = account_state.storage.into_iter().collect();

            // Insert account with storage
            cache_state.insert_account_with_storage(address, account_info, storage);
        }

        Ok(())
    }

    /// Create initial state from prestate (if provided) or empty state
    fn create_initial_state(&self, code: &[u8]) -> Result<State<EmptyDB>> {
        // Determine state clear flag based on EVM spec
        let has_state_clear = self.spec_id().into_eth_spec().is_enabled_in(SpecId::SPURIOUS_DRAGON);
        let mut cache_state = CacheState::new(has_state_clear);

        // Load prestate if provided
        if let Some(ref prestate_path) = self.prestate {
            self.load_prestate(prestate_path, &mut cache_state)?;
        }

        // If not in create mode, set the code at the receiver address
        if !self.create && !code.is_empty() {
            let bytecode = Bytecode::new_raw_checked(Bytes::copy_from_slice(code))
                .unwrap_or_else(|_| Bytecode::new_legacy(Bytes::copy_from_slice(code)));
            let code_hash = bytecode.hash_slow();

            let acc_info =
                AccountInfo { balance: U256::ZERO, code_hash, code: Some(bytecode), nonce: 0 };

            cache_state.insert_account_with_storage(self.receiver, acc_info, Default::default());
        }

        // Set balance for sender if specified
        if let Some(balance) = self.sender_balance {
            let sender_info = AccountInfo {
                balance,
                code_hash: KECCAK_EMPTY,
                code: Some(Bytecode::default()),
                nonce: 0,
            };
            cache_state.insert_account_with_storage(self.sender, sender_info, Default::default());
        }

        Ok(State::builder().with_cached_prestate(cache_state).with_bundle_update().build())
    }

    /// Execute bytecode once
    fn execute_once(
        &self,
        state: &mut State<EmptyDB>,
        code: &[u8],
        input: &[u8],
    ) -> Result<RunResult> {
        let start = Instant::now();

        let (exec_result, evm_state, trace_data) = self.execute_evm(state, code, input)?;

        let duration = start.elapsed();

        Ok(RunResult { exec_result, state: evm_state, exec_time: duration, trace_data })
    }

    /// Execute bytecode multiple times for benchmarking
    fn execute_benchmark(
        &self,
        state: &mut State<EmptyDB>,
        code: &[u8],
        input: &[u8],
    ) -> Result<RunResult> {
        // Warm-up run
        let (exec_result, evm_state, trace_data) = self.execute_evm(state, code, input)?;

        // Benchmark runs
        const BENCH_ITERATIONS: u32 = 100;
        let mut total_duration = Duration::ZERO;

        for _ in 0..BENCH_ITERATIONS {
            let start = Instant::now();
            self.execute_evm(state, code, input)?;
            total_duration += start.elapsed();
        }

        let avg_duration = total_duration / BENCH_ITERATIONS;

        Ok(RunResult { exec_result, state: evm_state, exec_time: avg_duration, trace_data })
    }

    /// Setup configuration environment
    fn setup_cfg_env(&self) -> CfgEnv<MegaSpecId> {
        let mut cfg = CfgEnv::default();
        cfg.chain_id = self.chain_id;
        cfg.spec = self.spec_id();
        cfg
    }

    /// Setup block environment
    fn setup_block_env(&self) -> BlockEnv {
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

        block
    }

    /// Setup external environments with bucket capacity configuration
    fn setup_external_envs(&self) -> Result<DefaultExternalEnvs> {
        let mut external_envs = DefaultExternalEnvs::new();

        // Parse and configure bucket capacities
        for bucket_capacity_str in &self.bucket_capacity {
            let (bucket_id, capacity) = super::parse_bucket_capacity(bucket_capacity_str)?;
            // Use the block number from the configuration
            external_envs =
                external_envs.with_bucket_capacity(bucket_id, self.block_number, capacity);
        }

        Ok(external_envs)
    }

    /// Setup transaction environment
    fn setup_tx_env(&self, code: &[u8], input: &[u8]) -> TxEnv {
        // Determine transaction data and kind based on create mode
        let (data, kind) = if self.create {
            // In create mode, code is the init code, input is appended
            let mut init_code = code.to_vec();
            init_code.extend_from_slice(input);
            (Bytes::copy_from_slice(&init_code), TxKind::Create)
        } else {
            // In call mode, code is already set at receiver, input is calldata
            (Bytes::copy_from_slice(input), TxKind::Call(self.receiver))
        };

        TxEnv {
            caller: self.sender,
            gas_price: self.price as u128,
            gas_priority_fee: self.priority_fee.map(|pf| pf as u128),
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: self.tx_type,
            gas_limit: self.gas,
            data,
            nonce: 0,
            value: self.value,
            access_list: Default::default(),
            authorization_list: Vec::new(),
            kind,
            chain_id: Some(self.chain_id),
        }
    }

    /// Execute transaction with optional tracing
    fn execute_transaction(
        &self,
        evm_context: MegaContext<&mut State<EmptyDB>, DefaultExternalEnvs>,
        tx: MegaTransaction,
    ) -> Result<(ExecutionResult<mega_evm::MegaHaltReason>, EvmState, Option<String>)> {
        if matches!(self.tracer, Some(TracerType::Trace)) {
            // Execute with tracing inspector
            let config = TracingInspectorConfig::all();
            let mut inspector = TracingInspector::new(config);
            let mut evm = MegaEvm::new(evm_context).with_inspector(&mut inspector);

            let result_and_state = evm.inspect_tx(tx).map_err(|e| {
                super::RunError::ExecutionError(format!("EVM execution failed: {:?}", e))
            })?;

            // Generate GethTrace using GethTraceBuilder
            let geth_builder = inspector.geth_builder();

            // Create GethDefaultTracingOptions based on CLI arguments
            let opts = GethDefaultTracingOptions {
                disable_storage: Some(self.trace_disable_storage),
                disable_memory: Some(self.trace_disable_memory),
                disable_stack: Some(self.trace_disable_stack),
                enable_return_data: Some(self.trace_enable_return_data),
                ..Default::default()
            };

            // Get output for trace generation
            let output = match &result_and_state.result {
                ExecutionResult::Success { output, .. } => output.data().to_vec(),
                ExecutionResult::Revert { output, .. } => output.to_vec(),
                _ => Vec::new(),
            };

            // Generate the geth trace
            let geth_trace = geth_builder.geth_traces(
                result_and_state.result.gas_used(),
                Bytes::from(output),
                opts,
            );

            // Format as JSON
            let trace_str = serde_json::to_string_pretty(&geth_trace)
                .unwrap_or_else(|e| format!("Error serializing trace: {}", e));

            Ok((result_and_state.result, result_and_state.state, Some(trace_str)))
        } else {
            // Execute without tracing
            let mut evm = MegaEvm::new(evm_context);
            let result_and_state = evm.transact(tx).map_err(|e| {
                super::RunError::ExecutionError(format!("EVM execution failed: {:?}", e))
            })?;

            Ok((result_and_state.result, result_and_state.state, None))
        }
    }

    /// Execute EVM with the given state, code, and input. State changes will not be committed to
    /// the state database.
    fn execute_evm(
        &self,
        state: &mut State<EmptyDB>,
        code: &[u8],
        input: &[u8],
    ) -> Result<(ExecutionResult<mega_evm::MegaHaltReason>, EvmState, Option<String>)> {
        // Setup configuration, block, transaction environments, and external environments
        let cfg = self.setup_cfg_env();
        let block = self.setup_block_env();
        let tx_env = self.setup_tx_env(code, input);
        let external_envs = self.setup_external_envs()?;

        // Create EVM context and transaction
        let evm_context =
            MegaContext::new(state, cfg.spec, external_envs).with_cfg(cfg).with_block(block);

        let mut tx = MegaTransaction::new(tx_env);
        tx.enveloped_tx = Some(Bytes::default());

        // Execute transaction
        self.execute_transaction(evm_context, tx)
    }

    /// Output execution results
    fn output_results(&self, exec_result: &RunResult) -> Result<()> {
        // Extract output from execution result
        let output = match &exec_result.exec_result {
            ExecutionResult::Success { output, .. } => output.data(),
            ExecutionResult::Revert { output, .. } => output.as_ref(),
            ExecutionResult::Halt { .. } => &[],
        };

        // Print execution output
        if output.is_empty() {
            println!("0x");
        } else {
            println!("0x{}", hex::encode(output));
        }

        // Print error if any
        let error = match &exec_result.exec_result {
            ExecutionResult::Success { .. } => None,
            ExecutionResult::Revert { .. } => Some("Revert"),
            ExecutionResult::Halt { reason, .. } => Some(&format!("Halt: {:?}", reason) as &str),
        };

        if let Some(err) = error {
            eprintln!(" error: {}", err);
        }

        // Print statistics
        eprintln!();
        eprintln!("EVM gas used:    {}", exec_result.exec_result.gas_used());
        eprintln!("execution time:  {:?}", exec_result.exec_time);
        if self.bench {
            eprintln!("(averaged over 100 runs)");
        }

        // Output trace data if available
        if let Some(ref trace) = exec_result.trace_data {
            if let Some(ref output_file) = self.trace_output_file {
                // Write trace to file
                std::fs::write(output_file, trace).map_err(|e| {
                    super::RunError::ExecutionError(format!("Failed to write trace to file: {}", e))
                })?;
                eprintln!();
                eprintln!("Trace written to: {}", output_file.display());
            } else {
                // Print trace to console
                eprintln!();
                eprintln!("=== Execution Trace ===");
                eprintln!("{}", trace);
            }
        }

        // Dump state if requested
        if self.dump {
            self.dump_state(&exec_result.state)?;
        }

        Ok(())
    }

    /// Dump final state as JSON
    fn dump_state(&self, evm_state: &EvmState) -> Result<()> {
        // Create state dump from EVM state
        let state_dump = super::StateDump::from_evm_state(evm_state);

        // Serialize the state dump as pretty JSON
        let state_json = serde_json::to_string_pretty(&state_dump).map_err(|e| {
            super::RunError::ExecutionError(format!("Failed to serialize state: {}", e))
        })?;

        // Output to file or console
        if let Some(ref output_file) = self.dump_output_file {
            // Write state to file
            std::fs::write(output_file, state_json).map_err(|e| {
                super::RunError::ExecutionError(format!("Failed to write state to file: {}", e))
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
}
