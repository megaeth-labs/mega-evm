use std::time::{Duration, Instant};

use alloy_consensus::{
    Eip658Value, Receipt as ConsensusReceipt, ReceiptEnvelope, ReceiptWithBloom,
};
use alloy_primitives::{Address, Bloom, Bytes, B256, U256};
use alloy_rpc_types_eth::{Log, TransactionReceipt};
use clap::Parser;
use mega_evm::{
    revm::{
        context::{result::ExecutionResult, tx::TxEnv},
        primitives::TxKind,
        state::EvmState,
        DatabaseRef,
    },
    MegaContext, MegaTransaction,
};

use crate::run::EvmeState;

use super::{load_input, Result};

/// Run arbitrary transaction
#[derive(Parser, Debug)]
pub struct Cmd {
    // Shared argument groups
    /// Pre-execution state configuration
    #[command(flatten)]
    pub prestate_args: crate::run::PreStateArgs,

    /// Environment configuration
    #[command(flatten)]
    pub env_args: crate::run::EnvArgs,

    /// State dump configuration
    #[command(flatten)]
    pub dump_args: crate::run::StateDumpArgs,

    /// Trace configuration
    #[command(flatten)]
    pub trace_args: crate::run::TraceArgs,

    // Transaction configuration
    /// Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, etc.)
    #[arg(long = "tx-type", default_value = "0")]
    pub tx_type: u8,

    /// Gas limit for the evm
    #[arg(long = "gas", default_value = "1000000000")]
    pub gas: u64,

    /// Price set for the evm (gas price)
    #[arg(long = "price", default_value = "0")]
    pub price: u64,

    /// Gas priority fee (EIP-1559)
    #[arg(long = "priorityfee")]
    pub priority_fee: Option<u64>,

    /// The transaction origin
    #[arg(long = "sender", visible_aliases = ["from"], default_value = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")]
    pub sender: Address,

    /// The transaction receiver (execution context)
    #[arg(long = "receiver", visible_aliases = ["to"], default_value = "0x0000000000000000000000000000000000000000")]
    pub receiver: Address,

    /// The transaction nonce
    #[arg(long = "nonce")]
    pub nonce: Option<u64>,

    /// Indicates the action should be create rather than call
    #[arg(long = "create")]
    pub create: bool,

    /// Value set for the evm
    #[arg(long = "value", default_value = "0")]
    pub value: U256,

    /// Transaction data (input) as hex string (positional argument)
    #[arg(value_name = "INPUT")]
    pub input: Option<String>,

    /// File containing transaction data (input). If '-' is specified, code is read from stdin
    #[arg(long = "inputfile")]
    pub inputfile: Option<String>,
}

/// Execution result with optional trace data and state
struct TxResult {
    exec_result: ExecutionResult<mega_evm::MegaHaltReason>,
    state: EvmState,
    exec_time: Duration,
    trace_data: Option<String>,
}

impl Cmd {
    /// Execute the tx command
    pub async fn run(&self) -> Result<()> {
        // Step 1: Load input data
        let input = load_input(self.input.clone(), self.inputfile.clone())?;

        // Step 2: Setup initial state and environment
        // Load prestate from file and sender balance
        let (prestate, storage) = crate::run::load_prestate(&self.prestate_args, self.sender)?;

        // Create provider if forking
        let provider = if self.prestate_args.fork {
            let url = self.prestate_args.fork_rpc.parse().map_err(|e| {
                crate::run::RunError::RpcError(format!("Invalid RPC URL '{}': {}", self.prestate_args.fork_rpc, e))
            })?;
            eprintln!("Forking from RPC {}", self.prestate_args.fork_rpc);
            Some(alloy_provider::ProviderBuilder::new().connect_http(url))
        } else {
            None
        };

        // Create initial state with provider (if any)
        let mut state = crate::run::create_initial_state(provider, self.prestate_args.fork_block, prestate, storage).await?;

        // Step 3: Execute transaction
        let start = Instant::now();
        let (exec_result, evm_state, trace_data) = self.execute(&mut state, &input)?;
        let duration = start.elapsed();
        let result = TxResult { exec_result, state: evm_state, exec_time: duration, trace_data };

        // Step 4: Output results (including state dump if requested)
        self.output_results(&result)?;

        Ok(())
    }

    /// Setup transaction environment
    fn setup_tx_env<N, P>(&self, state: &EvmeState<N, P>, input: &[u8]) -> TxEnv
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        // Determine transaction data and kind based on create mode
        let (data, kind) = if self.create {
            // In create mode, input is the init code
            (Bytes::copy_from_slice(input), TxKind::Create)
        } else {
            // In call mode, input is calldata
            (Bytes::copy_from_slice(input), TxKind::Call(self.receiver))
        };

        // Get nonce from state
        let nonce = self
            .nonce
            .unwrap_or_else(|| state.basic_ref(self.sender).unwrap().unwrap_or_default().nonce);

        TxEnv {
            caller: self.sender,
            gas_price: self.price as u128,
            gas_priority_fee: self.priority_fee.map(|pf| pf as u128),
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: self.tx_type,
            gas_limit: self.gas,
            data,
            nonce,
            value: self.value,
            access_list: Default::default(),
            authorization_list: Vec::new(),
            kind,
            chain_id: Some(self.env_args.chain_id),
        }
    }

    /// Execute EVM with the given state and input. State changes will not be committed to
    /// the state database.
    fn execute<N, P>(
        &self,
        state: &mut EvmeState<N, P>,
        input: &[u8],
    ) -> Result<(ExecutionResult<mega_evm::MegaHaltReason>, EvmState, Option<String>)>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        // Setup configuration, block, transaction environments, and external environments
        let cfg = crate::run::setup_cfg_env(&self.env_args);
        let block = crate::run::setup_block_env(&self.env_args);
        let tx_env = self.setup_tx_env(state, input);
        let external_envs = crate::run::setup_external_envs(&self.env_args.bucket_capacity)?;

        // Create EVM context and transaction
        let evm_context = MegaContext::new(state, cfg.spec)
            .with_cfg(cfg)
            .with_block(block)
            .with_external_envs(external_envs.into());

        let mut tx = MegaTransaction::new(tx_env);
        tx.enveloped_tx = Some(Bytes::default());

        // Execute transaction
        crate::run::execute_transaction(evm_context, tx, &self.trace_args)
    }

    /// Output execution results
    fn output_results(&self, exec_result: &TxResult) -> Result<()> {
        // Create transaction receipt
        let receipt = self.create_receipt(&exec_result.exec_result, &exec_result.state)?;

        // Serialize and print receipt as JSON
        let receipt_json = serde_json::to_string_pretty(&receipt).map_err(|e| {
            super::TxError::ExecutionError(format!("Failed to serialize receipt: {}", e))
        })?;
        println!("{}", receipt_json);

        // Print execution time to stderr
        eprintln!();
        eprintln!("execution time:  {:?}", exec_result.exec_time);

        // Output trace data if available
        if let Some(ref trace) = exec_result.trace_data {
            if let Some(ref output_file) = self.trace_args.trace_output_file {
                // Write trace to file
                std::fs::write(output_file, trace).map_err(|e| {
                    super::TxError::ExecutionError(format!("Failed to write trace to file: {}", e))
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
            crate::run::dump_state(&exec_result.state, &self.dump_args)?;
        }

        Ok(())
    }

    /// Create a transaction receipt from execution result
    fn create_receipt(
        &self,
        exec_result: &ExecutionResult<mega_evm::MegaHaltReason>,
        _evm_state: &EvmState,
    ) -> Result<TransactionReceipt> {
        // Determine status: true for success, false for revert/halt
        let success = matches!(exec_result, ExecutionResult::Success { .. });

        // Extract logs from the execution result
        let logs: Vec<Log> = exec_result
            .logs()
            .iter()
            .enumerate()
            .map(|(log_index, log)| Log {
                inner: alloy_primitives::Log { address: log.address, data: log.data.clone() },
                block_hash: Some(B256::ZERO), // Would need actual block hash
                block_number: Some(self.env_args.block_number),
                block_timestamp: None,
                transaction_hash: Some(B256::ZERO), // Would need actual tx hash
                transaction_index: Some(0),
                log_index: Some(log_index as u64),
                removed: false,
            })
            .collect();

        // Compute logs bloom filter
        let logs_bloom: Bloom = exec_result.logs().iter().collect();

        // Create consensus receipt with alloy_primitives::Log
        let consensus_receipt = ConsensusReceipt {
            status: Eip658Value::Eip658(success),
            cumulative_gas_used: exec_result.gas_used(),
            logs: logs.iter().map(|log| log.inner.clone()).collect(),
        };

        // Wrap in ReceiptWithBloom
        let receipt_with_bloom = ReceiptWithBloom { receipt: consensus_receipt, logs_bloom };

        // Wrap in ReceiptEnvelope based on transaction type
        let inner = match self.tx_type {
            1 => ReceiptEnvelope::Eip2930(receipt_with_bloom),
            2 => ReceiptEnvelope::Eip1559(receipt_with_bloom),
            3 => ReceiptEnvelope::Eip4844(receipt_with_bloom),
            4 => ReceiptEnvelope::Eip7702(receipt_with_bloom),
            _ => ReceiptEnvelope::Legacy(receipt_with_bloom), // Type 0 or unknown types
        }
        .map_logs(|primitive_log| {
            // Convert alloy_primitives::Log to alloy_rpc_types_eth::Log
            logs.iter()
                .find(|l| {
                    l.inner.address == primitive_log.address && l.inner.data == primitive_log.data
                })
                .cloned()
                .unwrap_or_else(|| Log {
                    inner: primitive_log,
                    block_hash: Some(B256::ZERO),
                    block_number: Some(self.env_args.block_number),
                    block_timestamp: None,
                    transaction_hash: Some(B256::ZERO),
                    transaction_index: Some(0),
                    log_index: None,
                    removed: false,
                })
        });

        // Determine contract address for CREATE transactions
        let contract_address = if self.create {
            match exec_result {
                ExecutionResult::Success { .. } => {
                    // For CREATE, compute the contract address from sender and nonce
                    Some(self.sender.create(0)) // nonce is 0 by default
                }
                _ => None,
            }
        } else {
            None
        };

        Ok(TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO, // Would need actual tx hash
            transaction_index: None,
            block_hash: None,
            block_number: Some(self.env_args.block_number),
            gas_used: exec_result.gas_used(),
            effective_gas_price: self.price as u128,
            blob_gas_used: None, // Would need EIP-4844 support
            blob_gas_price: None,
            from: self.sender,
            to: if self.create { None } else { Some(self.receiver) },
            contract_address,
        })
    }
}
