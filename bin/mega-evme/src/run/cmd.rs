use clap::Parser;
use mega_evm::revm::state::Bytecode;
use tracing::{debug, info, trace};

use super::{load_hex, Result, RunError};
use crate::common::{
    create_address, log_execution_result, pre_execution_nonce, print_run_artifacts,
    print_transaction_report, EvmeOutcome, ExecutionSummary,
};

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

    /// RPC configuration (used when --fork is enabled)
    #[command(flatten)]
    pub rpc_args: super::RpcArgs,

    /// Environment configuration
    #[command(flatten)]
    pub env_args: super::EnvArgs,

    /// State dump configuration
    #[command(flatten)]
    pub dump_args: super::StateDumpArgs,

    /// Trace configuration
    #[command(flatten)]
    pub trace_args: super::TraceArgs,

    /// Output format configuration
    #[command(flatten)]
    pub output_args: super::OutputArgs,
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
        let sender = self.tx_args.sender();
        let (mut state, cache_store) =
            self.prestate_args.create_initial_state(&sender, &self.rpc_args).await?;
        debug!(sender = %sender, "State initialized");

        // Deploy system contracts based on spec
        let spec = self.env_args.spec_id()?;
        state.deploy_system_contracts(spec);
        debug!(spec = ?spec, "System contracts deployed");

        let pre_execution_nonce = pre_execution_nonce(&state, sender)?;
        debug!(nonce = pre_execution_nonce, "Pre-execution nonce");

        // Run-specific: If not in create mode, set the code at the receiver address
        if !self.tx_args.create() && !code.is_empty() {
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
        if self.tx_args.create() {
            debug!("Create mode: prepending code to input data");
            tx.base.data = [code.as_ref(), tx.base.data.as_ref()].concat().into();
        }

        let outcome = EvmeOutcome::execute(
            &mut state,
            &self.env_args,
            &self.trace_args,
            tx,
            pre_execution_nonce,
        )?;
        log_execution_result!(target: "mega_evme::run::cmd", &outcome.exec_result);

        // Step 4: Output results (including state dump if requested)
        trace!("Writing output results");
        self.output_results(&outcome)?;

        // Step 5: Persist the RPC cache (clean-exit only).
        cache_store.persist()?;

        Ok(())
    }

    /// Output execution results
    fn output_results(&self, outcome: &EvmeOutcome) -> Result<()> {
        // `run` emits no receipt; the summary names this address only if the creation deployed.
        let create_address =
            create_address(self.tx_args.sender(), self.tx_args.kind(), outcome.pre_execution_nonce);

        if self.output_args.json {
            let mut summary =
                ExecutionSummary::of_transaction(&outcome.exec_result, create_address, None);
            summary.fill_trace_and_dump(outcome, &self.trace_args, &self.dump_args)?;
            summary.print_pretty();
        } else {
            print_transaction_report(
                &outcome.exec_result,
                create_address,
                outcome.exec_time,
                None,
                &[],
            );
            print_run_artifacts(outcome, &self.trace_args, &self.dump_args)?;
        }

        Ok(())
    }
}
