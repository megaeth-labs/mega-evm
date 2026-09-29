use std::time::Instant;

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Block;
use clap::Parser;
use mega_evm::{
    alloy_evm::{block::BlockExecutor, Evm, EvmEnv},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{result::ExecutionResult, BlockEnv, CfgEnv},
        database::{states::bundle_state::BundleRetention, StateBuilder},
        primitives::eip4844,
        DatabaseRef,
    },
    system::SequencerRegistryConfig,
    BlockLimits, DeclaredObserver, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory,
    MegaHardforkConfig, MegaHardforks, MegaSpecId,
};
use tracing::{debug, info, trace, warn};

use op_alloy_rpc_types::Transaction;

use crate::{
    common::{
        op_receipt_to_tx_receipt, parse_bucket_capacity, print_execution_summary,
        print_execution_trace, print_receipt, print_satin_report, BuildProviderOutput,
        EvmeExternalEnvs, EvmeOutcome, ExecutionSummary, ExternalEnvSnapshot, OpTxReceipt,
        RpcCacheStore, SatinReport, TxOverrideArgs,
    },
    engine::Engine,
    run, EvmeState,
};

use super::{ReplayError, Result};

/// Replay a transaction from RPC
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Transaction hash to replay
    #[arg(value_name = "TX_HASH", required_unless_present = "block")]
    pub tx_hash: Option<B256>,

    /// Block replay configuration (`--block`)
    #[command(flatten)]
    pub block_args: crate::block::BlockArgs,

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

    /// Override the spec to use (default: auto-detect from chain ID and block timestamp).
    /// `Satin` replays on the Satin engine, a legacy spec on the legacy engine
    #[arg(long = "override.spec", value_name = "SPEC")]
    pub spec_override: Option<String>,

    /// Transaction override configuration
    #[command(flatten)]
    pub tx_override_args: TxOverrideArgs,

    /// Output format configuration
    #[command(flatten)]
    pub output_args: run::OutputArgs,

    /// Dump a self-validating EEST state-test fixture for the replayed
    /// transaction to the given file.
    ///
    /// The fixture captures the pre-state read closure, block environment,
    /// transaction, and `MegaETH` external environment, and records `post`
    /// expectations (state/logs roots, gas, status) computed by the state-test
    /// runner. Re-running the file through `state-test` self-validates the
    /// replay, and `state-test --bench` benchmarks it. The dump is rejected
    /// unless the local replay reproduces the on-chain receipt's gas and success
    /// status. Incompatible with transaction overrides and `--override.spec`. Legacy specs
    /// only: no state-test runner prices a transaction as Satin does.
    #[arg(long = "dump-fixture", value_name = "FILE")]
    pub dump_fixture: Option<std::path::PathBuf>,
}

/// Resolved provider and associated metadata from `--rpc` / `--rpc.capture-file` /
/// `--rpc.replay-file` flags.
struct ProviderContext {
    provider: crate::common::OpProvider,
    cache_store: RpcCacheStore,
    external_env: Option<ExternalEnvSnapshot>,
    chain_id: u64,
}

/// Replay-specific execution outcome
pub(super) struct ReplayOutcome {
    /// Common execution outcome
    pub outcome: EvmeOutcome,
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
    /// The engine the replayed transaction runs on: the one `--override.spec` names, otherwise
    /// the one its chain ran, or runs, its block on.
    ///
    /// Without an override this reads the chain id, and, only on a chain that switches engines
    /// at a scheduled timestamp, the block's timestamp, from the source the replay itself reads,
    /// through a provider that persists nothing.
    pub async fn engine(&self) -> Result<Engine> {
        let tx_hash = self.tx_hash();
        if let Some(spec) = &self.spec_override {
            return Engine::of_spec(spec);
        }
        let (provider, chain_id) = self.rpc_args.build_lookup_provider().await?;
        if let Some(engine) = Engine::of_chain(chain_id) {
            return Ok(engine);
        }
        let tx = provider
            .get_transaction_by_hash(tx_hash)
            .await
            .map_err(|e| ReplayError::RpcError(format!("Failed to fetch transaction: {e}")))?
            .ok_or(ReplayError::TransactionNotFound(tx_hash))?;
        let timestamp = match tx.block_number {
            Some(number) => provider
                .get_block_by_number(number.into())
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
                .ok_or(ReplayError::BlockNotFound(number))?
                .header
                .timestamp(),
            // A pending transaction executes on top of the latest block, at a later timestamp.
            None => u64::MAX,
        };
        Ok(Engine::of_block(chain_id, timestamp))
    }

    /// Whether this replays whole blocks (`--block`), which runs here for both engines.
    pub fn replays_blocks(&self) -> bool {
        self.block_args.block.is_some()
    }

    /// The transaction to replay; present unless `--block` is.
    fn tx_hash(&self) -> B256 {
        self.tx_hash.expect("clap requires TX_HASH unless --block is given")
    }

    /// Replay a historical transaction on the Satin engine, or whole blocks on either engine.
    pub async fn run(&self) -> Result<()> {
        if let Some(range) = self.block_args.block {
            let bucket_capacities = self
                .ext_args
                .bucket_capacity
                .iter()
                .map(|s| parse_bucket_capacity(s))
                .collect::<Result<Vec<_>>>()?;
            let summary = crate::block::replay_blocks(
                range,
                &self.block_args,
                &self.rpc_args,
                &bucket_capacities,
                self.spec_override.as_deref(),
                self.output_args.json,
            )
            .await?;
            let code = summary.exit_code(self.block_args.verify);
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }

        // A dumped fixture is re-executed by a state-test runner that prices the transaction as
        // the chain does; no such runner exists for Satin, so the dump is refused before any
        // network or state work. On a legacy spec the dump is the 1.7.1 tool's, unchanged.
        if self.dump_fixture.is_some() {
            return Err(ReplayError::Other(
                "--dump-fixture is not available on Satin: no state-test runner prices a \
                 transaction as Satin does; it is available on the legacy specs"
                    .to_string(),
            ));
        }

        let mut pctx = self.resolve_provider().await?;
        let rctx = self.fetch_replay_context(&pctx.provider, pctx.chain_id).await?;
        let (external_envs, env_snapshot) = self.resolve_external_envs(&pctx)?;

        // Execute and report, but defer error propagation until the cache store has persisted:
        // in capture mode an execution failure is exactly the case you'd want to debug
        // offline, so the captured RPC responses must not be discarded.
        let run_result = self.execute_and_report(&pctx.provider, &rctx, external_envs).await;

        // Hand the effective external-env snapshot to the store before the final
        // persist; no-op unless this is a fixture-capture store.
        if let Some(snapshot) = env_snapshot {
            pctx.cache_store.set_external_env(snapshot);
        }
        let persist_result = pctx.cache_store.persist();
        match run_result {
            Ok(()) => Ok(persist_result?),
            Err(run_err) => {
                // Surface the original error; a persist failure on top of it is
                // logged, not propagated, so it cannot mask the root cause.
                if let Err(persist_err) = persist_result {
                    warn!(
                        error = %persist_err,
                        "Failed to persist RPC cache while handling an earlier error",
                    );
                }
                Err(run_err)
            }
        }
    }

    /// Execute the replay and print the results.
    ///
    /// Split out of [`Self::run`] so the caller can persist the RPC cache store
    /// regardless of whether this step fails.
    async fn execute_and_report<P>(
        &self,
        provider: &P,
        rctx: &ReplayContext,
        external_envs: EvmeExternalEnvs,
    ) -> Result<()>
    where
        P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug,
    {
        let result = self.execute(provider, rctx, external_envs).await?;
        self.output_results(&result)
    }

    /// Select the right provider based on `--rpc`, `--rpc.capture-file`, and
    /// `--rpc.replay-file` flags.
    async fn resolve_provider(&self) -> Result<ProviderContext> {
        let output = if let Some(path) = &self.rpc_args.capture_file {
            info!(path = %path.display(), "Provider mode: capture to cache file");
            self.rpc_args.build_capture_provider().await?
        } else if let Some(path) = &self.rpc_args.replay_file {
            if !self.ext_args.bucket_capacity.is_empty() {
                return Err(ReplayError::Other(
                    "'--bucket-capacity' cannot be used in offline replay mode \
                         (bucket capacities come from the fixture envelope)"
                        .to_string(),
                ));
            }
            info!(path = %path.display(), "Provider mode: offline replay from cache file");
            self.rpc_args.build_replay_provider().await?
        } else if let Some(rpc) = &self.rpc_args.rpc_url {
            info!(rpc = %rpc, "Provider mode: online RPC");
            self.rpc_args.build_provider().await?
        } else {
            return Err(ReplayError::Other(
                "'mega-evme replay' requires '--rpc <URL>', '--rpc.capture-file <PATH>', \
                 or '--rpc.replay-file <PATH>'"
                    .to_string(),
            ));
        };

        let BuildProviderOutput { provider, cache_store, chain_id, external_env } = output;
        Ok(ProviderContext { provider, cache_store, external_env, chain_id })
    }

    /// Fetch the transaction, its block, and preceding transaction hashes from the provider.
    async fn fetch_replay_context<P>(&self, provider: &P, chain_id: u64) -> Result<ReplayContext>
    where
        P: Provider<op_alloy_network::Optimism>,
    {
        let tx_hash = self.tx_hash();
        info!(%tx_hash, "Fetching transaction");
        let target_tx = provider
            .get_transaction_by_hash(tx_hash)
            .await
            .map_err(|e| ReplayError::RpcError(format!("Failed to fetch transaction: {e}")))?
            .ok_or_else(|| ReplayError::TransactionNotFound(tx_hash))?;
        debug!(block_number = ?target_tx.block_number, "Transaction found");

        let (state_base_block, block_number, is_pending) = if let Some(n) = target_tx.block_number {
            (n - 1, n, false)
        } else {
            let latest = provider
                .get_block_number()
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?;
            (latest, latest, true)
        };
        debug!(
            state_base_block = state_base_block,
            block = block_number,
            is_pending,
            "Block numbers determined",
        );

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
                if hash == tx_hash {
                    break;
                }
                preceding_tx_hashes.push(hash);
            }
        }

        debug!(chain_id, preceding_count = preceding_tx_hashes.len(), "Replay context ready");

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
        if self.rpc_args.replay_file.is_some() {
            let mut envs = EvmeExternalEnvs::new();
            if let Some(snapshot) = &pctx.external_env {
                debug!(
                    bucket_count = snapshot.bucket_capacities.len(),
                    "Using bucket capacities from replay envelope",
                );
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
        debug!(
            bucket_count = effective.len(),
            from_cli = !self.ext_args.bucket_capacity.is_empty(),
            "Resolved bucket capacities for online/capture mode",
        );

        // Build the envelope snapshot only in capture mode.
        let snapshot = self
            .rpc_args
            .capture_file
            .is_some()
            .then_some(ExternalEnvSnapshot { bucket_capacities: effective });

        Ok((envs, snapshot))
    }

    /// Execute the target transaction (with preceding transactions) on Satin and return the
    /// outcome.
    async fn execute<P>(
        &self,
        provider: &P,
        ctx: &ReplayContext,
        external_envs: EvmeExternalEnvs,
    ) -> Result<ReplayOutcome>
    where
        P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug,
    {
        let timestamp = ctx.block.header.timestamp();
        let hardforks = satin_hardforks(ctx.chain_id, timestamp);
        debug!(chain_id = ctx.chain_id, spec = %MegaSpecId::SATIN, "Chain configuration");

        info!(fork_block = ctx.parent_block.header.number(), "Forking state from parent block",);
        let mut database = EvmeState::new_forked(
            provider.clone(),
            Some(ctx.parent_block.header.number()),
            Default::default(),
            Default::default(),
        )
        .await?;

        let block_env = retrieve_block_env(&ctx.block)?;
        trace!(?block_env, "Block environment built");
        let mut cfg_env = CfgEnv::new_with_spec(MegaSpecId::SATIN);
        cfg_env.chain_id = ctx.chain_id;
        let evm_env = EvmEnv::new(cfg_env, block_env);

        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
        let block_executor_factory = MegaBlockExecutorFactory::new(
            OpAlloyReceiptBuilder::default(),
            &hardforks,
            evm_factory,
        );
        // The limits a Satin block runs under when its node configures nothing else: the
        // production data-size caps and gas detention's caps; the block's gas limit comes from
        // its header.
        let block_ctx = MegaBlockExecutionCtx::new(
            ctx.parent_block.hash(),
            ctx.block.header.parent_beacon_block_root(),
            ctx.block.header.extra_data().clone(),
            BlockLimits::default(),
        );

        let start = Instant::now();
        // The tracer reads and writes nothing back, so block execution admits it declared.
        let mut inspector = DeclaredObserver::new(self.trace_args.create_inspector());
        let mut state =
            StateBuilder::new().with_database(&mut database).with_bundle_update().build();
        let mut block_executor = block_executor_factory.create_executor_with_trusted_inspector(
            &mut state,
            evm_env,
            block_ctx,
            &mut inspector,
        );

        block_executor
            .apply_pre_execution_changes()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;

        // Execute preceding transactions
        info!(preceding_count = ctx.preceding_tx_hashes.len(), "Executing preceding transactions",);
        for tx_hash in &ctx.preceding_tx_hashes {
            debug!(tx_hash = %tx_hash, "Executing preceding transaction");
            let tx = provider
                .get_transaction_by_hash(*tx_hash)
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
                .ok_or(ReplayError::TransactionNotFound(*tx_hash))?;
            let outcome = block_executor
                .run_transaction(tx.as_recovered())
                .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
            trace!(tx_hash = %tx_hash, ?outcome, "Preceding transaction executed");
            block_executor
                .commit_transaction_outcome(outcome)
                .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
        }

        // Execute target transaction.
        info!("Executing target transaction");
        if self.tx_override_args.has_overrides() {
            info!(overrides = ?self.tx_override_args, "Applying transaction overrides");
        }
        let wrapped_tx = self.tx_override_args.wrap(ctx.target_tx.as_recovered())?;
        let pre_execution_nonce = block_executor
            .evm()
            .db()
            .basic_ref(wrapped_tx.inner().signer())?
            .map(|acc| acc.nonce)
            .unwrap_or(0);

        // The trace covers the target transaction only.
        block_executor.evm_mut().inspector_mut().0.fuse();
        let outcome = block_executor
            .run_transaction(wrapped_tx)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
        trace!(tx_hash = %ctx.target_tx.inner.inner.tx_hash(), ?outcome, "Target transaction executed");
        let satin = SatinReport::of(&outcome.inner);
        let exec_result = outcome.inner.result.clone();
        let evm_state = outcome.inner.state.clone();

        match &exec_result {
            ExecutionResult::Success { .. } => {
                info!(gas_used = exec_result.tx_gas_used(), "Execution succeeded")
            }
            ExecutionResult::Revert { .. } => {
                warn!(gas_used = exec_result.tx_gas_used(), "Execution reverted")
            }
            ExecutionResult::Halt { reason, .. } => {
                warn!(?reason, gas_used = exec_result.tx_gas_used(), "Execution halted")
            }
        }

        let result_and_state = mega_evm::revm::context::result::ResultAndState {
            result: exec_result.clone(),
            state: evm_state.clone(),
        };

        let trace_data = self.trace_args.is_tracing_enabled().then(|| {
            self.trace_args.generate_trace(
                &block_executor.inspector().0,
                &result_and_state,
                block_executor.evm().db(),
            )
        });

        let gas_output = block_executor
            .commit_transaction_outcome(outcome)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
        let gas_used = gas_output.tx_gas_used();
        let duration = start.elapsed();

        let (evm, block_result) = block_executor
            .finish()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
        let (db, _) = evm.finish();
        db.merge_transitions(BundleRetention::Reverts);
        let receipt_envelope = block_result.receipts.last().unwrap().clone();
        trace!(?receipt_envelope, "Receipt envelope obtained");

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
                satin,
            },
            receipt,
        })
    }

    /// Print execution results as JSON (`--json`) or human-readable text.
    fn output_results(&self, result: &ReplayOutcome) -> Result<()> {
        trace!("Writing output results");
        if self.output_args.json {
            let mut summary = ExecutionSummary::from_result(
                &result.outcome.exec_result,
                result.receipt.contract_address,
            );
            summary.fill_trace_and_dump(&result.outcome, &self.trace_args, &self.dump_args)?;
            summary.receipt =
                Some(serde_json::to_value(&result.receipt).expect("failed to serialize receipt"));
            summary.satin = Some(result.outcome.satin);
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
            print_satin_report(&result.outcome.satin);
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

/// The hardfork schedule a Satin replay of a block of `chain_id` at `timestamp` runs under.
///
/// A block the chain's own schedule runs on Satin runs under that schedule. Any other block is a
/// counterfactual: it runs as if Satin were active from genesis, with the placeholder registry
/// parameters, which only a registry deployed from scratch reads; a chain whose registry is
/// already deployed keeps the roles in its storage.
pub(crate) fn satin_hardforks(chain_id: u64, timestamp: u64) -> MegaHardforkConfig {
    let schedule = mega_evm::hardfork_schedule(chain_id);
    if schedule.spec_id(timestamp) == Some(MegaSpecId::SATIN) &&
        schedule.fork_params::<SequencerRegistryConfig>().is_some()
    {
        return schedule;
    }
    MegaHardforkConfig::new()
        .with_all_activated()
        .with_params(SequencerRegistryConfig::placeholder())
}

/// Build a [`BlockEnv`] from the RPC block header.
///
/// Reads `excess_blob_gas` directly from the header rather than using a
/// hardcoded default, so blob-fee-sensitive opcodes (e.g. `BLOBBASEFEE`)
/// match on-chain semantics during replay.
fn retrieve_block_env(block: &Block<Transaction>) -> Result<BlockEnv> {
    let mut block_env = BlockEnv {
        number: U256::from(block.number()),
        beneficiary: block.header.beneficiary(),
        timestamp: U256::from(block.header.timestamp()),
        gas_limit: block.header.gas_limit(),
        basefee: block.header.base_fee_per_gas().unwrap_or_default(),
        difficulty: block.header.difficulty(),
        prevrandao: block.header.mix_hash(),
        blob_excess_gas_and_price: None,
        slot_num: 0,
    };

    let excess_blob_gas = block.header.excess_blob_gas().ok_or_else(|| {
        ReplayError::Other(format!(
            "block header missing excess_blob_gas (block {})",
            block.number()
        ))
    })?;
    block_env.set_blob_excess_gas_and_price(
        excess_blob_gas,
        eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
    );

    trace!(block_env = ?block_env, "Block environment retrieved");
    Ok(block_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header as ConsensusHeader;
    use alloy_rpc_types_eth::Header as RpcHeader;
    use mega_evm::revm::context_interface::block::BlobExcessGasAndPrice;

    fn make_block(excess_blob_gas: Option<u64>) -> Block<Transaction> {
        let inner = ConsensusHeader { excess_blob_gas, ..Default::default() };
        Block::empty(RpcHeader::new(inner))
    }

    #[test]
    fn test_retrieve_block_env_sets_blob_fee_from_header() {
        let excess_blob_gas: u64 = 786_432;
        let block = make_block(Some(excess_blob_gas));

        let env = retrieve_block_env(&block).expect("should build block env");

        let expected = BlobExcessGasAndPrice::new(
            excess_blob_gas,
            eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
        );
        assert_eq!(env.blob_excess_gas_and_price, Some(expected));
    }

    #[test]
    fn test_retrieve_block_env_zero_excess_blob_gas_yields_min_price() {
        let block = make_block(Some(0));

        let env = retrieve_block_env(&block).expect("should build block env");

        let blob = env.blob_excess_gas_and_price.expect("blob fields populated");
        assert_eq!(blob.excess_blob_gas, 0);
        assert_eq!(blob.blob_gasprice, u128::from(eip4844::MIN_BLOB_GASPRICE));
    }

    #[test]
    fn test_retrieve_block_env_missing_excess_blob_gas_errors() {
        let block = make_block(None);

        let err = retrieve_block_env(&block).expect_err("should reject pre-Cancun header");
        match err {
            ReplayError::Other(msg) => assert!(
                msg.contains("excess_blob_gas"),
                "error should mention missing field, got: {msg}"
            ),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}
