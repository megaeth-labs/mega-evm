use std::{str::FromStr, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::Block;
use clap::{Parser, ValueEnum};
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
    common::{
        op_receipt_to_tx_receipt, print_execution_summary, print_execution_trace, print_receipt,
        EvmeOutcome, OpTxReceipt, TxOverrideArgs,
    },
    replay::get_hardfork_config,
    run, ChainArgs, EvmeState,
};

use super::{ReplayError, Result};

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

    /// Override the spec to use (default: auto-detect from chain ID and block timestamp)
    #[arg(long = "override.spec", value_name = "SPEC")]
    pub spec_override: Option<String>,

    /// Transaction override configuration
    #[command(flatten)]
    pub tx_override_args: TxOverrideArgs,

    /// Output format: "human" (default) or "json" (structured, for benchmarking)
    #[arg(long = "output", value_name = "FORMAT", default_value = "human")]
    pub output_format: OutputFormat,
}

/// Output format for replay results.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable summary with receipt and trace
    #[default]
    Human,
    /// Structured JSON for benchmarking
    Json,
}

/// Phase timing breakdown for benchmarking.
#[derive(Debug, serde::Serialize)]
pub(super) struct PhaseTiming {
    pub pre_execution_ms: f64,
    pub preceding_txs_ms: f64,
    pub target_tx_ms: f64,
    pub commit_ms: f64,
    pub total_ms: f64,
}

/// Benchmark-relevant metrics extracted from the transaction outcome.
#[derive(Debug, serde::Serialize)]
pub(super) struct BenchMetrics {
    pub compute_gas_used: u64,
    pub data_size: u64,
    pub kv_updates: u64,
    pub state_growth: u64,
    pub mgas_per_sec: f64,
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
    /// Phase timing breakdown
    pub timing: PhaseTiming,
    /// Benchmark metrics
    pub bench_metrics: BenchMetrics,
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
        let result = self
            .execute_transactions(
                hardforks,
                &mut database,
                &parent_block,
                &block,
                evm_env,
                &provider,
                preceding_transactions,
                &target_tx,
            )
            .await?;

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
        mut evm_env: mega_evm::alloy_evm::EvmEnv<MegaSpecId>,
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
            info!(spec_override = %spec_override, "Overriding EVM spec");
            let spec = MegaSpecId::from_str(spec_override)
                .map_err(|e| ReplayError::Other(format!("Invalid spec: {:?}", e)))?;
            evm_env.cfg_env.spec = spec;
            block_limits = block_limits.with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
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

        // Phase 2: Apply pre-execution changes
        let t_pre = Instant::now();
        block_executor
            .apply_pre_execution_changes()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        let pre_exec_duration = t_pre.elapsed();

        // Phase 3: Execute preceding transactions
        let t_preceding = Instant::now();
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
        let preceding_duration = t_preceding.elapsed();
        debug!(preceding_count = preceding_transactions.len(), "Preceding transactions executed");

        // Phase 4: Execute target transaction
        info!("Executing target transaction");
        if self.tx_override_args.has_overrides() {
            info!(overrides = ?self.tx_override_args, "Applying transaction overrides");
        }

        // Wrap transaction with overrides (if any)
        let wrapped_tx = self.tx_override_args.wrap(target_tx.as_recovered())?;

        let pre_execution_nonce = block_executor
            .evm()
            .db_ref()
            .basic_ref(wrapped_tx.inner().signer())?
            .map(|acc| acc.nonce)
            .unwrap_or(0);

        block_executor.inspector_mut().fuse();
        let t_target = Instant::now();
        let outcome = block_executor
            .run_transaction(wrapped_tx)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        let target_tx_duration = t_target.elapsed();

        trace!(tx_hash = %target_tx.inner.inner.tx_hash(), outcome = ?outcome, "Target transaction executed");

        // Extract benchmark metrics before consuming outcome
        let compute_gas_used = outcome.inner.compute_gas_used;
        let data_size = outcome.inner.data_size;
        let kv_updates = outcome.inner.kv_updates;
        let state_growth = outcome.inner.state_growth_used;

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

        // Phase 5: Commit transaction outcome
        let t_commit = Instant::now();
        let gas_used = block_executor
            .commit_transaction_outcome(outcome)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        let commit_duration = t_commit.elapsed();

        let duration = start.elapsed();

        // Build phase timing and benchmark metrics
        let timing = PhaseTiming {
            pre_execution_ms: pre_exec_duration.as_secs_f64() * 1000.0,
            preceding_txs_ms: preceding_duration.as_secs_f64() * 1000.0,
            target_tx_ms: target_tx_duration.as_secs_f64() * 1000.0,
            commit_ms: commit_duration.as_secs_f64() * 1000.0,
            total_ms: duration.as_secs_f64() * 1000.0,
        };

        let target_secs = target_tx_duration.as_secs_f64();
        let mgas_per_sec =
            if target_secs > 0.0 { gas_used as f64 / target_secs / 1_000_000.0 } else { 0.0 };

        let bench_metrics =
            BenchMetrics { compute_gas_used, data_size, kv_updates, state_growth, mgas_per_sec };

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
            timing,
            bench_metrics,
        })
    }

    async fn retrieve_block_env(&self, block: &Block<Transaction>) -> Result<BlockEnv> {
        let block_env = BlockEnv {
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
        };
        trace!(block_env = ?block_env, "Block environment retrieved");
        Ok(block_env)
    }

    /// Output execution results
    fn output_results(&self, result: &ReplayOutcome) -> Result<()> {
        if matches!(self.output_format, OutputFormat::Json) {
            self.output_json(result)?;
        } else {
            // Print human-readable summary
            print_execution_summary(
                &result.outcome.exec_result,
                result.receipt.contract_address,
                result.outcome.exec_time,
            );

            print_receipt(&result.receipt);

            print_execution_trace(
                result.outcome.trace_data.as_deref(),
                self.trace_args.trace_output_file.as_deref(),
            )?;

            // Dump state if requested
            if self.dump_args.dump {
                self.dump_args.dump_evm_state(&result.outcome.state)?;
            }
        }

        Ok(())
    }

    /// Output structured JSON for benchmarking.
    fn output_json(&self, result: &ReplayOutcome) -> Result<()> {
        let status = if result.outcome.exec_result.is_success() {
            "success"
        } else {
            match &result.outcome.exec_result {
                ExecutionResult::Revert { .. } => "revert",
                ExecutionResult::Halt { .. } => "halt",
                _ => "unknown",
            }
        };

        let output = serde_json::json!({
            "tx_hash": format!("{:#x}", self.tx_hash),
            "status": status,
            "gas_used": result.outcome.exec_result.gas_used(),
            "timing": {
                "pre_execution_ms": result.timing.pre_execution_ms,
                "preceding_txs_ms": result.timing.preceding_txs_ms,
                "target_tx_ms": result.timing.target_tx_ms,
                "commit_ms": result.timing.commit_ms,
                "total_ms": result.timing.total_ms,
            },
            "performance": {
                "mgas_per_sec": result.bench_metrics.mgas_per_sec,
            },
            "gas_breakdown": {
                "compute_gas": result.bench_metrics.compute_gas_used,
                "storage_gas_approx": result.outcome.exec_result.gas_used().saturating_sub(result.bench_metrics.compute_gas_used),
            },
            "resource_usage": {
                "data_size": result.bench_metrics.data_size,
                "kv_updates": result.bench_metrics.kv_updates,
                "state_growth": result.bench_metrics.state_growth,
            },
        });

        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|e| ReplayError::Other(format!("JSON serialization failed: {e}")))?
        );
        Ok(())
    }
}
