use std::{str::FromStr, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{B256, U256};
use alloy_provider::Provider;
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
use tracing::{info, warn};

use op_alloy_rpc_types::Transaction;

use crate::{
    common::{
        op_receipt_to_tx_receipt, parse_bucket_capacity, print_execution_summary,
        print_execution_trace, print_receipt, BuildProviderOutput, EvmeExternalEnvs, EvmeOutcome,
        ExecutionSummary, ExternalEnvSnapshot, OpTxReceipt, RpcCacheStore, TxOverrideArgs,
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

    /// RPC configuration
    #[command(flatten)]
    pub rpc_args: super::RpcArgs,

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

    /// Output format configuration
    #[command(flatten)]
    pub output_args: run::OutputArgs,
}

/// Resolved provider and associated metadata from `--rpc` / `--rpc.cache-file` flags.
struct ProviderContext {
    provider: crate::common::OpProvider,
    cache_store: RpcCacheStore,
    external_env: Option<ExternalEnvSnapshot>,
    chain_id: u64,
    is_replay_mode: bool,
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

/// Intermediate context fetched from RPC before execution.
struct ReplayContext {
    target_tx: Transaction,
    parent_block: Block<Transaction>,
    block: Block<Transaction>,
    chain_id: u64,
    preceding_tx_hashes: Vec<B256>,
}

impl Cmd {
    /// Replay a historical transaction.
    pub async fn run(&self) -> Result<()> {
        let pctx = self.resolve_provider().await?;
        let rctx = self.fetch_replay_context(&pctx.provider, pctx.chain_id).await?;
        let (external_envs, env_snapshot) = self.resolve_external_envs(&pctx)?;
        let result = self.execute(&pctx.provider, &rctx, external_envs).await?;
        self.output_results(&result)?;
        pctx.cache_store.persist(env_snapshot.as_ref())?;
        Ok(())
    }

    /// Select the right provider based on `--rpc` and `--rpc.cache-file` flags.
    async fn resolve_provider(&self) -> Result<ProviderContext> {
        let BuildProviderOutput { provider, cache_store, chain_id, external_env } =
            match (&self.rpc_args.rpc_url, &self.rpc_args.cache_file) {
                (Some(_), Some(_)) => self.rpc_args.build_capture_provider().await?,
                (None, Some(_)) => {
                    if !self.ext_args.bucket_capacity.is_empty() {
                        return Err(ReplayError::Other(
                            "'--bucket-capacity' cannot be used in offline replay mode \
                             (bucket capacities come from the fixture envelope)"
                                .to_string(),
                        ));
                    }
                    self.rpc_args.build_replay_provider().await?
                }
                (Some(_), None) => self.rpc_args.build_provider().await?,
                (None, None) => {
                    return Err(ReplayError::Other(
                    "'mega-evme replay' requires either '--rpc <URL>' or '--rpc.cache-file <PATH>'"
                        .to_string(),
                ));
                }
            };

        let is_replay_mode = self.rpc_args.rpc_url.is_none() && self.rpc_args.cache_file.is_some();
        Ok(ProviderContext { provider, cache_store, external_env, chain_id, is_replay_mode })
    }

    /// Fetch the transaction, its block, and preceding transaction hashes from the provider.
    async fn fetch_replay_context<P>(&self, provider: &P, chain_id: u64) -> Result<ReplayContext>
    where
        P: Provider<op_alloy_network::Optimism>,
    {
        let target_tx = provider
            .get_transaction_by_hash(self.tx_hash)
            .await
            .map_err(|e| ReplayError::RpcError(format!("Failed to fetch transaction: {e}")))?
            .ok_or_else(|| ReplayError::TransactionNotFound(self.tx_hash))?;

        let (state_base_block, block_number, is_pending) = if let Some(n) = target_tx.block_number {
            (n - 1, n, false)
        } else {
            let latest = provider
                .get_block_number()
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?;
            (latest, latest, true)
        };

        let parent_block = provider
            .get_block_by_number(state_base_block.into())
            .await
            .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
            .ok_or(ReplayError::BlockNotFound(state_base_block))?;
        let block = provider
            .get_block_by_number(block_number.into())
            .await
            .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
            .ok_or(ReplayError::BlockNotFound(block_number))?;

        let mut preceding_tx_hashes = vec![];
        if !is_pending {
            for hash in block.transactions.hashes() {
                if hash == self.tx_hash {
                    break;
                }
                preceding_tx_hashes.push(hash);
            }
        }

        Ok(ReplayContext { target_tx, parent_block, block, chain_id, preceding_tx_hashes })
    }

    /// Build the external environment and (for capture mode) the envelope snapshot.
    ///
    /// Parses `--bucket-capacity` exactly once: the parsed values feed both the
    /// runtime `EvmeExternalEnvs` and the `ExternalEnvSnapshot` for envelope persistence.
    fn resolve_external_envs(
        &self,
        pctx: &ProviderContext,
    ) -> Result<(EvmeExternalEnvs, Option<ExternalEnvSnapshot>)> {
        if pctx.is_replay_mode {
            let mut envs = EvmeExternalEnvs::new();
            if let Some(snapshot) = &pctx.external_env {
                for &(bucket_id, capacity) in &snapshot.bucket_capacities {
                    envs = envs.with_bucket_capacity(bucket_id, capacity);
                }
            }
            return Ok((envs, None));
        }

        // Online / capture: parse bucket capacities once.
        let parsed: Vec<(u32, u64)> = self
            .ext_args
            .bucket_capacity
            .iter()
            .map(|s| parse_bucket_capacity(s))
            .collect::<std::result::Result<_, _>>()?;

        // Determine the effective capacities: CLI values take precedence,
        // then the previous envelope's values (refresh without --bucket-capacity),
        // then empty (defaults to MIN_BUCKET_SIZE).
        let effective = if !parsed.is_empty() {
            parsed
        } else if let Some(prev) = &pctx.external_env {
            prev.bucket_capacities.clone()
        } else {
            vec![]
        };

        let mut envs = EvmeExternalEnvs::new();
        for &(id, cap) in &effective {
            envs = envs.with_bucket_capacity(id, cap);
        }

        // Build the envelope snapshot only in capture mode.
        let snapshot = self
            .rpc_args
            .cache_file
            .is_some()
            .then_some(ExternalEnvSnapshot { bucket_capacities: effective });

        Ok((envs, snapshot))
    }

    /// Execute the target transaction (with preceding transactions) and return the outcome.
    async fn execute<P>(
        &self,
        provider: &P,
        ctx: &ReplayContext,
        external_envs: EvmeExternalEnvs,
    ) -> Result<ReplayOutcome>
    where
        P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug,
    {
        let hardforks = get_hardfork_config(ctx.chain_id);
        let spec = hardforks.spec_id(ctx.block.header.timestamp());
        let chain_args = ChainArgs { chain_id: ctx.chain_id, spec: spec.to_string() };

        let mut database = EvmeState::new_forked(
            provider.clone(),
            Some(ctx.parent_block.header.number()),
            Default::default(),
            Default::default(),
        )
        .await?;

        let block_env = block_env_from_header(&ctx.block);
        let mut evm_env = EvmEnv::new(chain_args.create_cfg_env()?, block_env);

        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
        let block_executor_factory = MegaBlockExecutorFactory::new(
            &hardforks,
            evm_factory,
            OpAlloyReceiptBuilder::default(),
        );
        let mut block_limits = BlockLimits::from_hardfork_and_block_gas_limit(
            hardforks.hardfork(ctx.block.header.timestamp()).ok_or(ReplayError::Other(format!(
                "No `MegaHardfork` active at block timestamp: {}",
                ctx.block.header.timestamp()
            )))?,
            ctx.block.header.gas_limit(),
        );

        if let Some(spec_override) = &self.spec_override {
            let spec = MegaSpecId::from_str(spec_override)
                .map_err(|e| ReplayError::Other(format!("Invalid spec: {e:?}")))?;
            evm_env.cfg_env.spec = spec;
            block_limits = block_limits.with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
        }

        let block_ctx = MegaBlockExecutionCtx::new(
            ctx.parent_block.hash(),
            ctx.block.header.parent_beacon_block_root(),
            ctx.block.header.extra_data().clone(),
            block_limits,
        );

        let start = Instant::now();
        let mut inspector = self.trace_args.create_inspector();
        let mut state =
            StateBuilder::new().with_database(&mut database).with_bundle_update().build();
        let mut block_executor = block_executor_factory.create_executor_with_inspector(
            &mut state,
            block_ctx,
            evm_env,
            &mut inspector,
        );

        block_executor
            .apply_pre_execution_changes()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;

        // Execute preceding transactions
        for tx_hash in &ctx.preceding_tx_hashes {
            let tx = provider
                .get_transaction_by_hash(*tx_hash)
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
                .ok_or(ReplayError::TransactionNotFound(*tx_hash))?;
            let outcome = block_executor
                .run_transaction(tx.as_recovered())
                .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
            block_executor
                .commit_transaction_outcome(outcome)
                .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
        }

        // Execute target transaction
        let wrapped_tx = self.tx_override_args.wrap(ctx.target_tx.as_recovered())?;
        let pre_execution_nonce = block_executor
            .evm()
            .db_ref()
            .basic_ref(wrapped_tx.inner().signer())?
            .map(|acc| acc.nonce)
            .unwrap_or(0);

        block_executor.inspector_mut().fuse();
        let outcome = block_executor
            .run_transaction(wrapped_tx)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
        let exec_result = outcome.inner.result.clone();
        let evm_state = outcome.inner.state.clone();

        match &exec_result {
            ExecutionResult::Success { gas_used, .. } => info!(gas_used, "Execution succeeded"),
            ExecutionResult::Revert { gas_used, .. } => warn!(gas_used, "Execution reverted"),
            ExecutionResult::Halt { reason, gas_used } => {
                warn!(?reason, gas_used, "Execution halted")
            }
        }

        let result_and_state = mega_evm::revm::context::result::ResultAndState {
            result: exec_result.clone(),
            state: evm_state.clone(),
        };

        let trace_data = self.trace_args.is_tracing_enabled().then(|| {
            self.trace_args.generate_trace(
                block_executor.inspector(),
                &result_and_state,
                block_executor.evm().db_ref(),
            )
        });

        let gas_used = block_executor
            .commit_transaction_outcome(outcome)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
        let duration = start.elapsed();

        let (evm, block_result) = block_executor
            .finish()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
        let (db, _) = evm.finish();
        db.merge_transitions(BundleRetention::Reverts);
        let receipt_envelope = block_result.receipts.last().unwrap().clone();

        let from = ctx.target_tx.inner.inner.signer();
        let to = ctx.target_tx.inner.inner.to();
        let contract_address = (to.is_none() && receipt_envelope.is_success())
            .then(|| from.create(pre_execution_nonce));
        let receipt = op_receipt_to_tx_receipt(
            &receipt_envelope,
            ctx.block.number(),
            ctx.block.header.timestamp(),
            from,
            to,
            contract_address,
            ctx.target_tx.inner.effective_gas_price.unwrap_or(0),
            gas_used,
            Some(ctx.target_tx.inner.inner.tx_hash()),
            Some(ctx.block.hash()),
            ctx.preceding_tx_hashes.len() as u64,
        );

        Ok(ReplayOutcome {
            outcome: EvmeOutcome {
                pre_execution_nonce,
                exec_result,
                state: evm_state,
                exec_time: duration,
                trace_data,
            },
            original_tx: ctx.target_tx.clone(),
            receipt,
        })
    }

    /// Print execution results as JSON (`--json`) or human-readable text.
    fn output_results(&self, result: &ReplayOutcome) -> Result<()> {
        if self.output_args.json {
            let mut summary = ExecutionSummary::from_result(
                &result.outcome.exec_result,
                result.receipt.contract_address,
            );
            summary.fill_trace_and_dump(&result.outcome, &self.trace_args, &self.dump_args)?;
            summary.receipt =
                Some(serde_json::to_value(&result.receipt).expect("failed to serialize receipt"));
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).expect("failed to serialize output")
            );
        } else {
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
            if self.dump_args.dump {
                self.dump_args.dump_evm_state(&result.outcome.state)?;
            }
        }
        Ok(())
    }
}

/// Build a [`BlockEnv`] from a block header.
fn block_env_from_header(block: &Block<Transaction>) -> BlockEnv {
    BlockEnv {
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
    }
}
