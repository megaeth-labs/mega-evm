use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use alloy_primitives::{hex, Address, Bytes, U256};
use clap::{Parser, ValueEnum};
use mega_evm::{
    revm::{
        context::{result::ExecutionResult, tx::TxEnv},
        primitives::TxKind,
        state::{AccountInfo, Bytecode, EvmState},
    },
    MegaContext, MegaTransaction,
};

use super::{load_code, load_input, EvmeState, Result};

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
    #[arg(long = "receiver", visible_aliases = ["to"], default_value = "0x0000000000000000000000000000000000000000")]
    pub receiver: Address,

    /// The transaction origin
    #[arg(long = "sender", visible_aliases = ["from"], default_value = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")]
    pub sender: Address,

    /// Value set for the evm
    #[arg(long = "value", default_value = "0")]
    pub value: U256,

    /// Benchmark the execution
    #[arg(long = "bench")]
    pub bench: bool,

    // Shared argument groups
    /// Pre-execution state configuration
    #[command(flatten)]
    pub prestate_args: super::PreStateArgs,

    /// Environment configuration
    #[command(flatten)]
    pub env_args: super::EnvArgs,

    /// State dump configuration
    #[command(flatten)]
    pub dump_args: super::StateDumpArgs,

    /// Trace configuration
    #[command(flatten)]
    pub trace_args: super::TraceArgs,
}

/// Execution result with optional trace data and state
struct RunResult {
    exec_result: ExecutionResult<mega_evm::MegaHaltReason>,
    state: EvmState,
    exec_time: Duration,
    trace_data: Option<String>,
}

impl Cmd {
    /// Execute the run command
    pub async fn run(&self) -> Result<()> {
        // Step 1: Load bytecode
        let code = load_code(self.code.clone(), self.codefile.clone())?;

        // Step 2: Load input data
        let input = load_input(
            self.input.clone(),
            self.inputfile.as_ref().map(|p| p.to_string_lossy().to_string()),
        )?;

        // Step 3: Setup initial state and environment
        // Load prestate from file and sender balance
        let (mut prestate, storage) = super::load_prestate(&self.prestate_args, self.sender)?;

        // Run-specific: If not in create mode, set the code at the receiver address
        if !self.create && !code.is_empty() {
            let bytecode = Bytecode::new_raw_checked(Bytes::copy_from_slice(&code))
                .unwrap_or_else(|_| Bytecode::new_legacy(Bytes::copy_from_slice(&code)));
            let code_hash = bytecode.hash_slow();

            let acc_info =
                AccountInfo { balance: U256::ZERO, code_hash, code: Some(bytecode), nonce: 0 };

            // Insert account into prestate
            prestate.insert(self.receiver, acc_info);
        }

        // Create provider if forking
        let provider = if self.prestate_args.fork {
            let url = self.prestate_args.fork_rpc.parse().map_err(|e| {
                super::RunError::RpcError(format!(
                    "Invalid RPC URL '{}': {}",
                    self.prestate_args.fork_rpc, e
                ))
            })?;
            eprintln!("Forking from RPC {}", self.prestate_args.fork_rpc);
            Some(alloy_provider::ProviderBuilder::new().connect_http(url))
        } else {
            None
        };

        // Create initial state with provider (if any)
        let mut state =
            super::create_initial_state(provider, self.prestate_args.fork_block, prestate, storage)
                .await?;

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

    /// Execute bytecode once
    fn execute_once<N, P>(
        &self,
        state: &mut EvmeState<N, P>,
        code: &[u8],
        input: &[u8],
    ) -> Result<RunResult>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        let start = Instant::now();

        let (exec_result, evm_state, trace_data) = self.execute(state, code, input)?;

        let duration = start.elapsed();

        Ok(RunResult { exec_result, state: evm_state, exec_time: duration, trace_data })
    }

    /// Execute bytecode multiple times for benchmarking
    fn execute_benchmark<N, P>(
        &self,
        state: &mut EvmeState<N, P>,
        code: &[u8],
        input: &[u8],
    ) -> Result<RunResult>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        // Warm-up run
        let (exec_result, evm_state, trace_data) = self.execute(state, code, input)?;

        // Benchmark runs
        const BENCH_ITERATIONS: u32 = 100;
        let mut total_duration = Duration::ZERO;

        for _ in 0..BENCH_ITERATIONS {
            let start = Instant::now();
            self.execute(state, code, input)?;
            total_duration += start.elapsed();
        }

        let avg_duration = total_duration / BENCH_ITERATIONS;

        Ok(RunResult { exec_result, state: evm_state, exec_time: avg_duration, trace_data })
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
            chain_id: Some(self.env_args.chain_id),
        }
    }

    /// Execute EVM with the given state, code, and input. State changes will not be committed to
    /// the state database.
    fn execute<N, P>(
        &self,
        state: &mut EvmeState<N, P>,
        code: &[u8],
        input: &[u8],
    ) -> Result<(ExecutionResult<mega_evm::MegaHaltReason>, EvmState, Option<String>)>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        // Setup configuration, block, transaction environments, and external environments
        let cfg = super::setup_cfg_env(&self.env_args);
        let block = super::setup_block_env(&self.env_args);
        let tx_env = self.setup_tx_env(code, input);
        let external_envs = super::setup_external_envs(&self.env_args.bucket_capacity)?;

        // Create EVM context and transaction
        let evm_context = MegaContext::new(state, cfg.spec)
            .with_cfg(cfg)
            .with_block(block)
            .with_external_envs(external_envs.into());

        let mut tx = MegaTransaction::new(tx_env);
        tx.enveloped_tx = Some(Bytes::default());

        // Execute transaction
        super::execute_transaction(evm_context, tx, &self.trace_args)
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
            if let Some(ref output_file) = self.trace_args.trace_output_file {
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
        if self.dump_args.dump {
            super::dump_state(&exec_result.state, &self.dump_args)?;
        }

        Ok(())
    }
}
