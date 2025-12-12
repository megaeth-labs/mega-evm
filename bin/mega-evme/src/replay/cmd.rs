use std::{str::FromStr, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::Block;
use clap::Parser;
use mega_evm::{
    alloy_evm::{block::BlockExecutor, Evm, EvmEnv},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{result::ExecutionResult, BlockEnv, ContextTr},
        context_interface::block::BlobExcessGasAndPrice,
        database::{states::bundle_state::BundleRetention, StateBuilder},
        DatabaseRef,
    },
    BlockLimits, EvmTxRuntimeLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory,
    MegaEvmFactory, MegaHardforks, MegaSpecId,
};
use tracing::{debug, info, trace, warn};

use op_alloy_rpc_types::Transaction;

use crate::{
    common::{op_receipt_to_tx_receipt, EvmeOutcome, OpTxReceipt},
    replay::get_hardfork_config,
    run, ChainArgs, EvmeState,
};

use super::{v1_0_1, ReplayError, Result};

/// Replay a transaction from RPC
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Transaction hash to replay
    #[arg(value_name = "TX_HASH")]
    pub tx_hash: B256,

    /// RPC URL to fetch transaction from
    #[arg(long = "rpc", visible_aliases = ["rpc-url"], env = "RPC_URL", default_value = "http://localhost:8545")]
    pub rpc: String,

    /// External environment configuration (bucket capacities)
    #[command(flatten)]
    pub ext_args: run::ExtEnvArgs,

    /// State dump configuration
    #[command(flatten)]
    pub dump_args: run::StateDumpArgs,

    /// Trace configuration
    #[command(flatten)]
    pub trace_args: run::TraceArgs,

    /// Use v1.0.1 of the mega-evm crate
    #[arg(long = "use-v1-0-1")]
    pub use_v1_0_1: bool,

    /// Override the spec to use (default: auto-detect from chain ID and block timestamp)
    #[arg(long = "spec", value_name = "SPEC")]
    pub spec_override: Option<String>,
}

/// Replay-specific execution outcome
#[allow(dead_code)]
pub(super) struct ReplayOutcome {
    /// Common execution outcome
    pub outcome: EvmeOutcome,
    /// The original transaction that was replayed
    pub original_tx: Transaction,
    /// The transaction receipt
    pub receipt: OpTxReceipt,
}

impl Cmd {
    /// Execute the replay command
    pub async fn run(&self) -> Result<()> {
        // Step 0: Build up rpc provider
        info!(rpc = %self.rpc, "Connecting to RPC");
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<op_alloy_network::Optimism>()
            .connect_http(self.rpc.parse().map_err(|e| {
                ReplayError::RpcError(format!("Invalid RPC URL '{}': {}", self.rpc, e))
            })?);

        // Step 1: fetch transaction
        info!(tx_hash = %self.tx_hash, "Fetching transaction");
        let target_tx = provider
            .get_transaction_by_hash(self.tx_hash)
            .await
            .map_err(|e| ReplayError::RpcError(format!("Failed to fetch transaction: {}", e)))?
            .ok_or_else(|| ReplayError::TransactionNotFound(self.tx_hash))?;
        debug!(block_number = ?target_tx.block_number, "Transaction found");

        // Step 2: determine block number to execute the transaction
        let (state_base_block_number, block_number, is_pending) =
            if let Some(block_number) = target_tx.block_number {
                (block_number - 1, block_number, false)
            } else {
                let latest_block_number = provider
                    .get_block_number()
                    .await
                    .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?;
                (latest_block_number, latest_block_number, true)
            };
        debug!(
            state_base_block = state_base_block_number,
            block = block_number,
            is_pending,
            "Block numbers determined"
        );

        let parent_block = provider
            .get_block_by_number(state_base_block_number.into())
            .await
            .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?
            .ok_or(ReplayError::BlockNotFound(state_base_block_number))?;
        let block = provider
            .get_block_by_number(block_number.into())
            .await
            .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?
            .ok_or(ReplayError::BlockNotFound(block_number))?;

        // Step 3: Obtain chain ID and spec
        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|e| ReplayError::RpcError(format!("Failed to get chain ID: {}", e)))?;
        let hardforks = get_hardfork_config(chain_id);
        let spec = hardforks.spec_id(block.header.timestamp());
        let chain_args = ChainArgs { chain_id, spec: spec.to_string() };
        debug!(chain_id, spec = %spec, "Chain configuration");

        // Step 4: Setup initial state by forking from the parent block
        info!(fork_block = state_base_block_number, "Forking state from parent block");
        let mut database = EvmeState::new_forked(
            provider.clone(),
            Some(state_base_block_number),
            Default::default(),
            Default::default(), // block_hashes - not used in replay
        )
        .await?;

        // Step 5: Setup BlockEnv and CfgEnv
        let block_env = self.retrieve_block_env(&block).await?;
        let cfg_env = chain_args.create_cfg_env()?;
        let evm_env = EvmEnv::new(cfg_env, block_env);

        // Step 6: fetch preceding transactions
        let mut preceding_transactions = vec![];
        if !is_pending {
            for tx in block.transactions.hashes() {
                if tx == self.tx_hash {
                    break;
                }
                preceding_transactions.push(tx);
            }
        }
        info!(preceding_count = preceding_transactions.len(), "Executing preceding transactions");

        // Step 7: Execute transactions with inspector
        let result = if self.use_v1_0_1 {
            debug!("Using v1.0.1 execution path");
            v1_0_1::execute_transactions_v1_0_1(
                &mut database,
                &parent_block,
                &block,
                evm_env,
                &provider,
                preceding_transactions,
                &target_tx,
                chain_args.spec_id()?,
                &self.trace_args,
            )
            .await?
        } else {
            self.execute_transactions(
                hardforks,
                &mut database,
                &parent_block,
                &block,
                evm_env,
                &provider,
                preceding_transactions,
                &target_tx,
            )
            .await?
        };

        // Step 8: Output results
        trace!("Writing output results");
        self.output_results(&result)?;

        Ok(())
    }

    /// Execute transactions with block executor and optional tracing
    #[allow(clippy::too_many_arguments)]
    async fn execute_transactions<P>(
        &self,
        hardforks: impl MegaHardforks,
        database: &mut run::EvmeState<op_alloy_network::Optimism, P>,
        parent_block: &Block<Transaction>,
        block: &Block<Transaction>,
        evm_env: mega_evm::alloy_evm::EvmEnv<MegaSpecId>,
        provider: &P,
        preceding_transactions: Vec<B256>,
        target_tx: &Transaction,
    ) -> Result<ReplayOutcome>
    where
        P: alloy_provider::Provider<op_alloy_network::Optimism> + std::fmt::Debug,
    {
        let transaction_index = preceding_transactions.len() as u64;
        debug!(transaction_index, "Setting up block executor");

        let external_env_factory = self.ext_args.create_external_envs()?;
        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_env_factory);
        let block_executor_factory = MegaBlockExecutorFactory::new(
            &hardforks,
            evm_factory,
            OpAlloyReceiptBuilder::default(),
        );
        let mut block_limits = BlockLimits::from_hardfork_and_block_gas_limit(
            hardforks.hardfork(block.header.timestamp()).ok_or(ReplayError::Other(format!(
                "No `MegaHardfork` active at block timestamp: {}",
                block.header.timestamp()
            )))?,
            block.header.gas_limit(),
        );
        if let Some(spec_override) = &self.spec_override {
            debug!(spec_override = %spec_override, "Overriding EVM spec");
            block_limits = block_limits.with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(
                MegaSpecId::from_str(spec_override)
                    .map_err(|e| ReplayError::Other(format!("Invalid spec: {:?}", e)))?,
            ));
        }
        let block_ctx = MegaBlockExecutionCtx::new(
            parent_block.hash(),
            block.header.parent_beacon_block_root(),
            block.header.extra_data().clone(),
            block_limits,
        );

        // Execute transactions with inspector (trace will be generated only if enabled)
        let start = Instant::now();

        // Setup tracing inspector
        let mut inspector = self.trace_args.create_inspector();

        // Create state and block executor with inspector
        let mut state = StateBuilder::new().with_database(database).with_bundle_update().build();
        let mut block_executor = block_executor_factory.create_executor_with_inspector(
            &mut state,
            block_ctx,
            evm_env,
            &mut inspector,
        );

        // Apply pre-execution changes
        block_executor
            .apply_pre_execution_changes()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;

        // Execute preceding transactions
        for tx_hash in &preceding_transactions {
            debug!(tx_hash = %tx_hash, "Executing preceding transaction");
            let tx = provider
                .get_transaction_by_hash(*tx_hash)
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?
                .ok_or(ReplayError::TransactionNotFound(*tx_hash))?;
            let tx = tx.as_recovered();
            let outcome = block_executor
                .run_transaction(tx)
                .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
            trace!(tx_hash = %tx_hash, outcome = ?outcome, "Preceding transaction executed");
            block_executor
                .commit_transaction_outcome(outcome)
                .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        }
        debug!(preceding_count = preceding_transactions.len(), "Preceding transactions executed");

        // Execute target transaction
        info!("Executing target transaction");
        let pre_execution_nonce = block_executor
            .evm()
            .db_ref()
            .basic_ref(target_tx.as_recovered().signer())?
            .map(|acc| acc.nonce)
            .unwrap_or(0);

        block_executor.inspector_mut().fuse();
        let outcome = block_executor
            .run_transaction(target_tx.as_recovered())
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        trace!(tx_hash = %target_tx.inner.inner.tx_hash(), outcome = ?outcome, "Target transaction executed");
        let exec_result = outcome.inner.result.clone();
        let evm_state = outcome.inner.state.clone();

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

        let result_and_state = mega_evm::revm::context::result::ResultAndState {
            result: exec_result.clone(),
            state: evm_state.clone(),
        };

        // Generate trace only if tracing is enabled
        let trace_data = self.trace_args.is_tracing_enabled().then(|| {
            self.trace_args.generate_trace(
                block_executor.inspector(),
                &result_and_state,
                block_executor.evm().db_ref(),
            )
        });

        // Commit transaction outcome
        let gas_used = block_executor
            .commit_transaction_outcome(outcome)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;

        let duration = start.elapsed();

        // Obtain receipt envelope
        let (evm, block_result) = block_executor
            .finish()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        let (db, _evm_env) = evm.finish();
        db.merge_transitions(BundleRetention::Reverts);
        let receipt_envelope = block_result.receipts.last().unwrap().clone();
        trace!(receipt = ?receipt_envelope, "Receipt envelope obtained");

        // Convert OpReceiptEnvelope to TransactionReceipt
        let from = target_tx.inner.inner.signer();
        let to = target_tx.inner.inner.to();
        let contract_address = (to.is_none() && receipt_envelope.is_success())
            .then(|| from.create(pre_execution_nonce));
        let receipt = op_receipt_to_tx_receipt(
            &receipt_envelope,
            block.number(),
            block.header.timestamp(),
            from,
            to,
            contract_address,
            target_tx.inner.effective_gas_price.unwrap_or(0),
            gas_used,
            Some(target_tx.inner.inner.tx_hash()),
            Some(block.hash()),
            transaction_index,
        );

        Ok(ReplayOutcome {
            outcome: EvmeOutcome {
                pre_execution_nonce,
                exec_result,
                state: evm_state,
                exec_time: duration,
                trace_data,
            },
            original_tx: target_tx.clone(),
            receipt,
        })
    }

    async fn retrieve_block_env(&self, block: &Block<Transaction>) -> Result<BlockEnv> {
        Ok(BlockEnv {
            number: U256::from(block.number()),
            beneficiary: block.header.beneficiary(),
            timestamp: U256::from(block.header.timestamp()),
            gas_limit: block.header.gas_limit(),
            basefee: block.header.base_fee_per_gas().unwrap_or_default(),
            difficulty: block.header.difficulty(),
            prevrandao: block.header.mix_hash(),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            }),
        })
    }

    /// Output execution results
    fn output_results(&self, result: &ReplayOutcome) -> Result<()> {
        // Print execution time to stderr
        println!();
        println!("execution time:  {:?}", result.outcome.exec_time);

        // Serialize and print receipt as JSON
        println!();
        println!("=== Receipt ===");
        let receipt_json = serde_json::to_string_pretty(&result.receipt)
            .map_err(|e| ReplayError::Other(format!("Failed to serialize receipt: {}", e)))?;
        println!("{}", receipt_json);

        // Output trace data if available
        if let Some(ref trace) = result.outcome.trace_data {
            println!();
            println!("=== Execution Trace ===");
            if let Some(ref output_file) = self.trace_args.trace_output_file {
                // Write trace to file
                std::fs::write(output_file, trace).map_err(|e| {
                    ReplayError::Other(format!("Failed to write trace to file: {}", e))
                })?;
                println!("Trace written to: {}", output_file.display());
            } else {
                // Print trace to console
                println!("{}", trace);
            }
        }

        // Dump state if requested
        if self.dump_args.dump {
            self.dump_args.dump_evm_state(&result.outcome.state)?;
        }

        Ok(())
    }
}
