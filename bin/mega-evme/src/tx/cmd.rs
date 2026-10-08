use clap::Parser;
use mega_evm::{
    revm::{context_interface::transaction::Transaction as _, primitives::TxKind},
    MegaTransaction, MegaTxType,
};
use tracing::{debug, info, trace, warn};

use crate::common::{
    create_address, load_hex, log_execution_result, op_receipt_to_tx_receipt, pre_execution_nonce,
    print_execution_summary, print_execution_trace, print_receipt, DecodedRawTx, EnvArgs,
    EvmeError, EvmeOutcome, ExecutionSummary, OutputArgs, PreStateArgs, RpcArgs, StateDumpArgs,
    TraceArgs, TxArgs,
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
    pub tx_args: TxArgs,

    /// Pre-execution state configuration
    #[command(flatten)]
    pub prestate_args: PreStateArgs,

    /// RPC configuration (used when --fork is enabled)
    #[command(flatten)]
    pub rpc_args: RpcArgs,

    /// Environment configuration
    #[command(flatten)]
    pub env_args: EnvArgs,

    /// State dump configuration
    #[command(flatten)]
    pub dump_args: StateDumpArgs,

    /// Trace configuration
    #[command(flatten)]
    pub trace_args: TraceArgs,

    /// Output format configuration
    #[command(flatten)]
    pub output_args: OutputArgs,
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
            if decoded.tx.base.chain_id != Some(chain_id) {
                warn!(
                    chain_id,
                    decoded_chain_id = decoded.tx.base.chain_id,
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
        let (mut state, cache_store) =
            self.prestate_args.create_initial_state(&sender, &self.rpc_args).await?;
        debug!(sender = %sender, "State initialized");

        state.deploy_system_contracts(spec);
        debug!(spec = ?spec, "System contracts deployed");

        let pre_execution_nonce = pre_execution_nonce(&state, sender)?;
        debug!(nonce = pre_execution_nonce, "Pre-execution nonce");

        // Step 3: Execute transaction
        info!("Executing transaction");
        let outcome = EvmeOutcome::execute(
            &mut state,
            &self.env_args,
            &self.trace_args,
            tx.clone(),
            pre_execution_nonce,
        )?;
        log_execution_result!(target: "mega_evme::tx::cmd", &outcome.exec_result);

        // Step 4: Output results (including state dump if requested)
        trace!("Writing output results");
        self.output_results(&outcome, &tx)?;

        // Step 5: Persist the RPC cache (clean-exit only).
        cache_store.persist()?;

        Ok(())
    }

    /// Output execution results
    fn output_results(&self, outcome: &EvmeOutcome, tx: &MegaTransaction) -> Result<()> {
        let tx_type = MegaTxType::try_from(tx.base.tx_type)
            .map_err(|_| EvmeError::UnsupportedTxType(tx.base.tx_type))?;
        let sender = tx.base.caller;
        let receiver = match tx.base.kind {
            TxKind::Call(addr) => Some(addr),
            TxKind::Create => None,
        };
        let effective_gas_price = tx.effective_gas_price(self.env_args.block.block_basefee as u128);

        // Create transaction receipt
        let op_receipt = outcome.to_op_receipt(tx_type, outcome.pre_execution_nonce);

        // Reported for a failed creation too, as an execution client's receipt does; the
        // summary names it as a deployed contract only on success.
        let create_address = create_address(sender, tx.base.kind, outcome.pre_execution_nonce);

        let receipt = op_receipt_to_tx_receipt(
            &op_receipt,
            self.env_args.block.block_number,
            self.env_args.block.block_timestamp,
            sender,
            receiver,
            create_address,
            effective_gas_price,
            outcome.exec_result.tx_gas_used(),
            None,
            None,
            0,
            0,
        );

        if self.output_args.json {
            let mut summary = ExecutionSummary::from_result(&outcome.exec_result, create_address);
            summary.fill_trace_and_dump(outcome, &self.trace_args, &self.dump_args)?;
            summary.receipt =
                Some(serde_json::to_value(&receipt).expect("failed to serialize receipt"));
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).expect("failed to serialize output")
            );
        } else {
            // Human-readable summary
            print_execution_summary(&outcome.exec_result, create_address, outcome.exec_time);

            print_receipt(&receipt);

            print_execution_trace(
                outcome.trace_data.as_deref(),
                self.trace_args.trace_output_file.as_deref(),
            )?;

            if self.dump_args.dump {
                self.dump_args.dump_evm_state(&outcome.state)?;
            }
        }

        Ok(())
    }
}
