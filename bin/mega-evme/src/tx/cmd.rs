use std::time::Instant;

use clap::Parser;
use mega_evm::{
    revm::{context::result::ExecutionResult, primitives::TxKind, DatabaseRef},
    MegaTransaction, MegaTxType,
};
use tracing::{debug, info, trace, warn};

use crate::common::{
    load_hex, op_receipt_to_tx_receipt, print_execution_summary, print_execution_trace,
    print_receipt, DecodedRawTx, EvmeError, EvmeOutcome,
};

use super::Result;

/// Run arbitrary transaction
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Raw EIP-2718 encoded transaction (hex). When provided, used as the base
    /// transaction with CLI flags serving as overrides.
    #[arg(value_name = "RAW_TX")]
    pub raw: Option<String>,

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
        let chain_id = self.env_args.chain.chain_id;
        let spec = self.env_args.spec_id()?;

        // Step 1: Create transaction
        info!("Creating transaction");
        let tx = if let Some(ref raw) = self.raw {
            let raw_bytes = load_hex(Some(raw.clone()), None)?.unwrap_or_default();
            let decoded = DecodedRawTx::from_raw(raw_bytes)?.override_tx_env(&self.tx_args)?;
            if decoded.tx_env.chain_id != Some(chain_id) {
                warn!(
                    chain_id,
                    decoded_chain_id = decoded.tx_env.chain_id,
                    "Raw transaction chain_id does not match the configured chain_id"
                );
            }
            decoded.into_tx()
        } else {
            self.tx_args.create_tx(chain_id)?
        };

        debug!(
            tx_type = tx.base.tx_type,
            gas_limit = tx.base.gas_limit,
            value = %tx.base.value,
            "Transaction created"
        );

        // Step 2: Setup initial state and environment
        let sender = tx.base.caller;
        info!("Setting up initial state");
        let mut state =
            self.prestate_args.create_initial_state::<op_alloy_network::Optimism>(&sender).await?;
        debug!(sender = %sender, "State initialized");

        state.deploy_system_contracts(spec);
        debug!(spec = ?spec, "System contracts deployed");

        let pre_execution_nonce = state.basic_ref(sender)?.map(|acc| acc.nonce).unwrap_or(0);
        debug!(nonce = pre_execution_nonce, "Pre-execution nonce");

        // Step 3: Execute transaction
        info!("Executing transaction");
        let evm_context = self.env_args.create_evm_context(&mut state)?;
        let start = Instant::now();
        let (exec_result, evm_state, trace_data) =
            self.trace_args.execute_transaction(evm_context, tx.clone())?;
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
        self.output_results(&outcome, &tx)?;

        Ok(())
    }

    /// Output execution results
    fn output_results(&self, outcome: &EvmeOutcome, tx: &MegaTransaction) -> Result<()> {
        let tx_type = MegaTxType::try_from(tx.base.tx_type)
            .map_err(|_| EvmeError::UnsupportedTxType(tx.base.tx_type))?;
        let sender = tx.base.caller;
        let is_create = tx.base.kind == TxKind::Create;
        let receiver = match tx.base.kind {
            TxKind::Call(addr) => Some(addr),
            TxKind::Create => None,
        };
        let effective_gas_price = tx.base.gas_price + tx.base.gas_priority_fee.unwrap_or(0);

        // Create transaction receipt
        let op_receipt = outcome.to_op_receipt(tx_type, outcome.pre_execution_nonce);

        // Determine contract address for CREATE transactions
        let contract_address = (is_create && op_receipt.is_success())
            .then(|| sender.create(outcome.pre_execution_nonce));

        // Print human-readable summary
        print_execution_summary(&outcome.exec_result, contract_address, outcome.exec_time);

        let receipt = op_receipt_to_tx_receipt(
            &op_receipt,
            self.env_args.block.block_number,
            self.env_args.block.block_timestamp,
            sender,
            receiver,
            contract_address,
            effective_gas_price,
            outcome.exec_result.gas_used(),
            None,
            None,
            0,
        );

        print_receipt(&receipt);

        print_execution_trace(
            outcome.trace_data.as_deref(),
            self.trace_args.trace_output_file.as_deref(),
        )?;

        // Dump state if requested
        if self.dump_args.dump {
            self.dump_args.dump_evm_state(&outcome.state)?;
        }

        Ok(())
    }
}
