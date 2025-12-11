use std::time::Instant;

use alloy_primitives::hex;
use clap::Parser;
use mega_evm::revm::{context::result::ExecutionResult, state::Bytecode, DatabaseRef};
use tracing::{debug, info, trace, warn};

use super::{load_hex, Result, RunError};
use crate::common::EvmeOutcome;

// Re-export TracerType from common module
pub use crate::common::TracerType;

/// Run arbitrary EVM bytecode
#[derive(Parser, Debug)]
pub struct Cmd {
    /// EVM bytecode as hex string (positional argument)
    #[arg(value_name = "CODE")]
    pub code: Option<String>,

    /// File containing EVM code. If '-' is specified, code is read from stdin
    #[arg(long = "codefile")]
    pub codefile: Option<String>,

    // Shared argument groups
    /// Transaction configuration
    #[command(flatten)]
    pub tx_args: super::TxArgs,

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

impl Cmd {
    /// Execute the run command
    pub async fn run(&self) -> Result<()> {
        // Step 1: Load bytecode
        info!("Loading bytecode");
        let code = load_hex(self.code.clone(), self.codefile.clone())?.ok_or_else(|| {
            RunError::InvalidInput(
                "No code provided. Use --codefile or provide code as argument".to_string(),
            )
        })?;
        debug!(code_len = code.len(), "Bytecode loaded");

        // Step 2: Setup initial state and environment
        info!("Setting up initial state");
        let mut state = self
            .prestate_args
            .create_initial_state::<op_alloy_network::Optimism>(&self.tx_args.sender)
            .await?;
        debug!(sender = %self.tx_args.sender, "State initialized");

        let pre_execution_nonce =
            state.basic_ref(self.tx_args.sender)?.map(|acc| acc.nonce).unwrap_or(0);
        debug!(nonce = pre_execution_nonce, "Pre-execution nonce");

        // Run-specific: If not in create mode, set the code at the receiver address
        if !self.tx_args.create && !code.is_empty() {
            let bytecode = Bytecode::new_raw_checked(code.clone())
                .unwrap_or_else(|_| Bytecode::new_legacy(code.clone()));
            debug!(receiver = %self.tx_args.receiver(), "Setting code at receiver address");
            state.set_account_code(self.tx_args.receiver(), bytecode);
        }

        // Step 3: Execute bytecode
        info!("Executing transaction");
        let mut tx = self.tx_args.create_tx(self.env_args.chain.chain_id)?;
        debug!(
            tx_type = tx.base.tx_type,
            gas_limit = tx.base.gas_limit,
            value = %tx.base.value,
            "Transaction created"
        );

        // In create mode, prepend code to input data
        if self.tx_args.create {
            debug!("Create mode: prepending code to input data");
            tx.base.data = [code.as_ref(), tx.base.data.as_ref()].concat().into();
        }

        // Create EVM context and execute transaction
        let evm_context = self.env_args.create_evm_context(&mut state)?;
        let start = Instant::now();
        let (exec_result, evm_state, trace_data) =
            self.trace_args.execute_transaction(evm_context, tx)?;
        let exec_time = start.elapsed();

        // Log execution result
        match &exec_result {
            ExecutionResult::Success { gas_used, .. } => {
                info!(gas_used, "Execution succeeded");
            }
            ExecutionResult::Revert { gas_used, .. } => {
                warn!(gas_used, "Execution reverted");
            }
            ExecutionResult::Halt { reason, gas_used } => {
                warn!(?reason, gas_used, "Execution halted");
            }
        }

        let outcome = EvmeOutcome {
            pre_execution_nonce,
            exec_result,
            state: evm_state,
            exec_time,
            trace_data,
        };

        // Step 4: Output results (including state dump if requested)
        trace!("Writing output results");
        self.output_results(&outcome)?;

        Ok(())
    }

    /// Output execution results
    fn output_results(&self, outcome: &EvmeOutcome) -> Result<()> {
        println!();
        println!("execution time:  {:?}", outcome.exec_time);
        println!();
        println!("=== Execution Output ===");
        println!("EVM gas used:    {}", outcome.exec_result.gas_used());
        match &outcome.exec_result {
            ExecutionResult::Success { output, .. } => {
                println!("0x{}", hex::encode(output.data()));
            }
            ExecutionResult::Revert { output, .. } => {
                println!("Revert: 0x{}", hex::encode(output.as_ref()));
            }
            ExecutionResult::Halt { reason, .. } => {
                println!("Halt: {:?}", reason);
            }
        };

        // Print statistics

        // Output trace data if available
        if let Some(ref trace) = outcome.trace_data {
            println!();
            println!("=== Execution Trace ===");
            if let Some(ref output_file) = self.trace_args.trace_output_file {
                // Write trace to file
                std::fs::write(output_file, trace).map_err(|e| {
                    super::RunError::ExecutionError(format!("Failed to write trace to file: {}", e))
                })?;
                println!("Trace written to: {}", output_file.display());
            } else {
                // Print trace to console
                println!("{}", trace);
            }
        }

        // Dump state if requested
        if self.dump_args.dump {
            self.dump_args.dump_evm_state(&outcome.state)?;
        }

        Ok(())
    }
}
