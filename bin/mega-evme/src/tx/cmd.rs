use std::time::Instant;

use clap::Parser;
use mega_evm::revm::{context::result::ExecutionResult, state::EvmState, DatabaseRef};

use crate::{
    common::{op_receipt_to_tx_receipt, EvmeOutcome},
    run::EvmeState,
};

use super::Result;

/// Run arbitrary transaction
#[derive(Parser, Debug)]
pub struct Cmd {
    // Shared argument groups
    /// Transaction configuration
    #[command(flatten)]
    pub tx_args: crate::run::TxArgs,

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
}

impl Cmd {
    /// Execute the tx command
    pub async fn run(&self) -> Result<()> {
        // Step 1: Setup initial state and environment
        // Create initial state with provider (if any)
        let mut state = self
            .prestate_args
            .create_initial_state::<op_alloy_network::Optimism>(&self.tx_args.sender)
            .await?;
        let pre_execution_nonce =
            state.basic_ref(self.tx_args.sender)?.map(|acc| acc.nonce).unwrap_or(0);

        // Step 2: Execute transaction
        let start = Instant::now();
        let (exec_result, evm_state, trace_data) = self.execute(&mut state)?;
        let exec_time = start.elapsed();
        let outcome = EvmeOutcome {
            pre_execution_nonce,
            exec_result,
            state: evm_state,
            exec_time,
            trace_data,
        };

        // Step 3: Output results (including state dump if requested)
        self.output_results(&outcome)?;

        Ok(())
    }

    /// Execute EVM with the given state. State changes will not be committed to
    /// the state database.
    fn execute<N, P>(
        &self,
        state: &mut EvmeState<N, P>,
    ) -> Result<(ExecutionResult<mega_evm::MegaHaltReason>, EvmState, Option<String>)>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        // Create transaction and EVM context
        let tx = self.tx_args.create_tx(self.env_args.chain.chain_id)?;
        let evm_context = self.env_args.create_evm_context(state)?;

        // Execute transaction
        self.trace_args.execute_transaction(evm_context, tx)
    }

    /// Output execution results
    fn output_results(&self, outcome: &EvmeOutcome) -> Result<()> {
        // Create transaction receipt
        let op_receipt =
            outcome.to_op_receipt(self.tx_args.tx_type()?, outcome.pre_execution_nonce);

        // Determine contract address for CREATE transactions
        let contract_address = (self.tx_args.create && op_receipt.is_success())
            .then(|| self.tx_args.sender.create(outcome.pre_execution_nonce));

        let receipt = op_receipt_to_tx_receipt(
            &op_receipt,
            self.env_args.block.block_number,
            self.env_args.block.block_timestamp,
            self.tx_args.sender,
            if self.tx_args.create { None } else { Some(self.tx_args.receiver()) },
            contract_address,
            self.tx_args.effective_gas_price()?,
        );

        // Serialize and print receipt as JSON
        let receipt_json = serde_json::to_string_pretty(&receipt).map_err(|e| {
            super::TxError::ExecutionError(format!("Failed to serialize receipt: {}", e))
        })?;
        println!("{}", receipt_json);

        // Print execution time to stderr
        eprintln!();
        eprintln!("execution time:  {:?}", outcome.exec_time);

        // Output trace data if available
        if let Some(ref trace) = outcome.trace_data {
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
            self.dump_args.dump_evm_state(&outcome.state)?;
        }

        Ok(())
    }
}
