use std::{path::PathBuf, str::FromStr, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Block;
use clap::{ArgGroup, Parser};
use mega_evm::{
    alloy_evm::{block::BlockExecutor, Evm, EvmEnv},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{result::ExecutionResult, BlockEnv, ContextTr},
        database::{states::bundle_state::BundleRetention, StateBuilder},
        primitives::eip4844,
        DatabaseRef,
    },
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaHardforks,
    MegaSpecId,
};
use tracing::{debug, error, info, trace, warn};

use alloy_network::ReceiptResponse;
use op_alloy_rpc_types::Transaction;

use crate::{
    common::{
        op_receipt_to_tx_receipt, parse_bucket_capacity, print_execution_summary,
        print_execution_trace, print_receipt, BuildProviderOutput, EvmeExternalEnvs, EvmeOutcome,
        ExecutionSummary, ExternalEnvSnapshot, OpTxReceipt, RpcArgs, RpcCacheStore, TracerType,
        TxOverrideArgs,
    },
    replay::{get_hardfork_config, ReplayHardforks},
    run, ChainArgs, EvmeState,
};

use super::{
    batch,
    verify::{self, VerificationOutcome},
    ReplayError, Result,
};

/// Replay a transaction from RPC
#[derive(Parser, Debug)]
#[command(group(
    ArgGroup::new("replay_target").required(true).args(["tx_hash", "tx_file", "block"])
))]
pub struct Cmd {
    /// Transaction hash to replay
    #[arg(value_name = "TX_HASH")]
    pub tx_hash: Option<B256>,

    /// Replay every transaction hash listed in the given file, one per line.
    ///
    /// Blank lines and `#`-prefixed comment lines are ignored, and duplicates are
    /// replayed once. All hashes are replayed in a single process: transactions
    /// are grouped by their containing block and each block is executed once.
    /// Batch mode does not support `--dump-fixture` (use `--dump-fixture-dir`),
    /// transaction overrides, `--override.spec`, tracing, or state dumps.
    #[arg(long = "tx-file", value_name = "PATH")]
    pub tx_file: Option<PathBuf>,

    /// Replay every transaction of the given block (decimal or `0x`-prefixed hex).
    ///
    /// Same batch semantics and restrictions as `--tx-file`.
    #[arg(long = "block", value_name = "N", value_parser = batch::parse_block_number)]
    pub block: Option<u64>,

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

    /// Dump a self-validating EEST state-test fixture for the replayed
    /// transaction to the given file.
    ///
    /// The fixture captures the pre-state read closure, block environment,
    /// transaction, and `MegaETH` external environment, and records `post`
    /// expectations (state/logs roots, gas, status) computed by the state-test
    /// runner. Re-running the file through `state-test` self-validates the
    /// replay, and `state-test --bench` benchmarks it. The dump is rejected
    /// unless the local replay reproduces the on-chain receipt's gas and success
    /// status. Incompatible with transaction overrides and `--override.spec`.
    /// Single-transaction only; for batch mode use `--dump-fixture-dir`.
    #[arg(long = "dump-fixture", value_name = "FILE")]
    pub dump_fixture: Option<std::path::PathBuf>,

    /// Dump a self-validating EEST state-test fixture for every successfully
    /// replayed target into `<DIR>/<tx_hash>.json`.
    ///
    /// Batch mode only (`--tx-file` / `--block`). Per-target gating mirrors the
    /// single-transaction dump: fidelity-gate failures and BLOCKHASH readers are
    /// skipped with a recorded reason instead of failing the run; pending or
    /// unresolvable targets stay error entries. Existing files are refused unless
    /// `--overwrite` is set. Registration into `bench/replay/manifest.json` is
    /// not performed — corpus curation stays manual.
    #[arg(long = "dump-fixture-dir", value_name = "DIR")]
    pub dump_fixture_dir: Option<std::path::PathBuf>,

    /// Replace existing files when writing fixtures with `--dump-fixture-dir`.
    ///
    /// Without this flag, a target whose `<DIR>/<tx_hash>.json` already exists is
    /// reported as an infrastructure error for that target.
    #[arg(long = "overwrite")]
    pub overwrite: bool,

    /// Verify every replayed transaction against its on-chain receipt.
    ///
    /// Fetches the receipt of each target and compares the success status, the
    /// gas used, and the emitted logs (count plus each log's address, topics,
    /// and data). The verdict is reported per transaction, and a mismatch makes
    /// the run exit non-zero. A target whose receipt cannot be fetched, or whose
    /// receipt describes a different inclusion than the replayed block, is
    /// reported as an infrastructure failure rather than a mismatch. Supported
    /// in both single-transaction and batch mode.
    #[arg(long = "verify-receipt")]
    pub verify_receipt: bool,
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
    /// Self-validating fixture draft, present iff `--dump-fixture` was given.
    pub fixture: Option<super::fixture::FixtureDraft>,
    /// On-chain receipt verdict, present iff `--verify-receipt` was given.
    pub verification: Option<VerificationOutcome>,
}

/// Intermediate context fetched from RPC before execution.
struct ReplayContext {
    tx_hash: B256,
    target_tx: Transaction,
    parent_block: Block<Transaction>,
    block: Block<Transaction>,
    chain_id: u64,
    preceding_tx_hashes: Vec<B256>,
}

/// What the command was asked to replay, resolved from the target argument group.
enum ReplayMode {
    /// The single transaction named by the positional `TX_HASH`.
    Single(B256),
    /// Many transactions replayed in one process (`--tx-file` / `--block`).
    Batch(batch::BatchMode),
}

impl Cmd {
    /// Replay one or more historical transactions.
    pub async fn run(&self) -> Result<()> {
        self.validate()?;
        let mode = self.resolve_mode()?;

        let mut pctx = self.resolve_provider().await?;

        // Execute, report, and (for --dump-fixture) finalize/write — but defer
        // error propagation until the cache store has persisted: in capture mode
        // an execution or dump-gate failure is exactly the case you'd want to
        // debug offline, so the captured RPC responses must not be discarded.
        let run_result = match &mode {
            ReplayMode::Single(tx_hash) => self.run_single(&mut pctx, *tx_hash).await,
            ReplayMode::Batch(batch_mode) => self.run_batch(&mut pctx, batch_mode).await,
        };

        let persist_result = pctx.cache_store.persist();
        match run_result {
            Ok(()) => Ok(persist_result?),
            Err(run_err) => {
                // The run error is the root cause and keeps the exit code, so
                // the persist failure is not propagated. It is still reported on
                // stderr the way the central reporter reports a failure — a
                // capture file that never reached disk must not be silent just
                // because the run it captured also failed.
                if let Err(persist_err) = persist_result {
                    error!(
                        error = %persist_err,
                        "Failed to persist RPC cache while handling an earlier error",
                    );
                    eprintln!("error: {persist_err}");
                }
                Err(run_err)
            }
        }
    }

    /// Pure input validation — reject before any network/state work.
    fn validate(&self) -> Result<()> {
        if self.is_batch() {
            self.validate_batch_args()?;
        } else {
            self.validate_single_args()?;
        }

        // A dumped fixture must represent the on-chain transaction, so it can
        // neither apply transaction overrides nor force a spec: both would make
        // the recorded execution a what-if, not the on-chain one.
        if self.dump_fixture.is_some() {
            if self.tx_override_args.has_overrides() {
                return Err(ReplayError::Other(
                    "--dump-fixture cannot be combined with transaction overrides (the \
                     isolated execution would not represent the on-chain transaction)"
                        .to_string(),
                ));
            }
            if self.spec_override.is_some() {
                return Err(ReplayError::Other(
                    "--dump-fixture cannot be combined with --override.spec (the fixture \
                     must record the spec auto-detected for the on-chain block, not a \
                     manually forced one)"
                        .to_string(),
                ));
            }
        }

        if self.dump_fixture.is_some() && self.dump_fixture_dir.is_some() {
            return Err(ReplayError::Other(
                "--dump-fixture and --dump-fixture-dir are mutually exclusive: dump one \
                 transaction with --dump-fixture, or a batch with --dump-fixture-dir"
                    .to_string(),
            ));
        }

        Ok(())
    }

    /// Reject batch-only flags in single-transaction mode.
    fn validate_single_args(&self) -> Result<()> {
        if self.dump_fixture_dir.is_some() {
            return Err(ReplayError::Other(
                "--dump-fixture-dir is only supported by batch replay (--tx-file / --block); \
                 dump a single transaction with --dump-fixture <FILE>"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// The spec forced by `--override.spec`, parsed.
    fn resolve_spec_override(&self) -> Result<Option<MegaSpecId>> {
        self.spec_override
            .as_deref()
            .map(|spec| {
                MegaSpecId::from_str(spec)
                    .map_err(|e| ReplayError::Other(format!("Invalid spec: {e:?}")))
            })
            .transpose()
    }

    /// Whether this invocation selects a batch of transactions.
    fn is_batch(&self) -> bool {
        self.tx_file.is_some() || self.block.is_some()
    }

    /// Reject the single-transaction-only flags in batch mode.
    ///
    /// Batch mode reports one summary per transaction; single-file fixture dumps,
    /// tracing, state dumps, and what-if knobs (overrides, forced spec) have no
    /// meaningful batch semantics, so they are rejected up front rather than
    /// silently ignored. Per-target fixture sedimentation uses
    /// `--dump-fixture-dir` instead of `--dump-fixture`.
    fn validate_batch_args(&self) -> Result<()> {
        const MODE: &str = "batch replay (--tx-file / --block)";

        // Genesis has no transactions and no parent to fork from. Without this
        // the run would collect zero targets, emit nothing, and exit 0 — the
        // block-0 rejection raised during replay never reaching the user.
        if self.block == Some(0) {
            return Err(ReplayError::Other(
                "--block 0 cannot be replayed: the genesis block has no transactions \
                 and no parent block to fork from"
                    .to_string(),
            ));
        }
        if self.dump_fixture.is_some() {
            return Err(ReplayError::Other(format!(
                "--dump-fixture is not supported by {MODE}; dump fixtures for a batch \
                 with --dump-fixture-dir <DIR>"
            )));
        }
        if self.tx_override_args.has_overrides() {
            return Err(ReplayError::Other(format!(
                "transaction overrides (--override.gas-limit / --override.value / \
                 --override.input / --override.input-file) are not supported by {MODE}"
            )));
        }
        if self.spec_override.is_some() {
            return Err(ReplayError::Other(format!(
                "--override.spec is not supported by {MODE}; each block's spec is \
                 auto-detected from its timestamp"
            )));
        }
        if has_trace_args(&self.trace_args) {
            return Err(ReplayError::Other(format!(
                "trace options (--trace / --trace.output / --tracer / --trace.*) are not \
                 supported by {MODE}"
            )));
        }
        if has_dump_args(&self.dump_args) {
            return Err(ReplayError::Other(format!(
                "state dump options (--dump / --dump.output) are not supported by {MODE}"
            )));
        }

        Ok(())
    }

    /// Resolve the target argument group into an execution mode.
    ///
    /// Reads `--tx-file` from disk here so a malformed list fails before any
    /// provider is built.
    fn resolve_mode(&self) -> Result<ReplayMode> {
        if let Some(path) = &self.tx_file {
            let contents = std::fs::read_to_string(path).map_err(|e| {
                ReplayError::InvalidInput(format!(
                    "Failed to read --tx-file '{}': {e}",
                    path.display()
                ))
            })?;
            let hashes = batch::parse_tx_hash_list(&contents)?;
            if hashes.is_empty() {
                return Err(ReplayError::InvalidInput(format!(
                    "--tx-file '{}' contains no transaction hashes",
                    path.display()
                )));
            }
            return Ok(ReplayMode::Batch(batch::BatchMode::TxList(hashes)));
        }
        if let Some(number) = self.block {
            return Ok(ReplayMode::Batch(batch::BatchMode::Block(number)));
        }
        match self.tx_hash {
            Some(tx_hash) => Ok(ReplayMode::Single(tx_hash)),
            // Unreachable through clap (the target group is required), but the
            // library API can construct `Cmd` directly.
            None => Err(ReplayError::InvalidInput(
                "'mega-evme replay' requires a TX_HASH, '--tx-file <PATH>', or '--block <N>'"
                    .to_string(),
            )),
        }
    }

    /// Replay the single transaction named by the positional argument.
    async fn run_single(&self, pctx: &mut ProviderContext, tx_hash: B256) -> Result<()> {
        let rctx = self.fetch_replay_context(&pctx.provider, tx_hash, pctx.chain_id).await?;
        // A pending transaction has no receipt to verify against; fail clearly
        // instead of replaying it and then surfacing the receipt lookup's
        // confusing "transaction is unknown to the endpoint".
        if self.verify_receipt && rctx.target_tx.block_number.is_none() {
            return Err(ReplayError::Other(
                "--verify-receipt does not support pending transactions: the comparison needs \
                 the on-chain receipt, which does not exist yet"
                    .to_string(),
            ));
        }
        let external_envs = self.apply_external_envs(pctx)?;
        self.execute_and_report(&pctx.provider, &rctx, external_envs).await
    }

    /// Replay a batch of transactions through the shared per-block driver.
    async fn run_batch(&self, pctx: &mut ProviderContext, mode: &batch::BatchMode) -> Result<()> {
        let external_envs = self.apply_external_envs(pctx)?;
        batch::run(
            &pctx.provider,
            pctx.chain_id,
            mode,
            external_envs,
            batch::ReportArgs {
                json: self.output_args.json,
                verify_receipt: self.verify_receipt,
                dump_fixture_dir: self.dump_fixture_dir.clone(),
                overwrite: self.overwrite,
            },
        )
        .await
    }

    /// Resolve the external environment and hand the capture snapshot to the
    /// cache store, which persists it on exit.
    fn apply_external_envs(&self, pctx: &mut ProviderContext) -> Result<EvmeExternalEnvs> {
        let (external_envs, env_snapshot) = self.resolve_external_envs(pctx)?;
        if let Some(snapshot) = env_snapshot {
            pctx.cache_store.set_external_env(snapshot);
        }
        Ok(external_envs)
    }

    /// Execute the replay, print the results, and (for `--dump-fixture`)
    /// finalize and write the fixture.
    ///
    /// Split out of [`Self::run`] so the caller can persist the RPC cache store
    /// regardless of which of these steps fails.
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
        // Read the verdict before `result.fixture` is moved below; the mismatch
        // is reported after every artifact has been written, so a failing
        // verification never costs the user the output it was derived from.
        let mismatched = result.verification.as_ref().is_some_and(|v| !v.matched);
        self.output_results(&result)?;
        // Write the self-validating fixture (re-executes the isolated unit through
        // state-test and cross-checks it against the replay before writing).
        if let (Some(path), Some(draft)) = (&self.dump_fixture, result.fixture) {
            // Single-file `--dump-fixture` always replaces the destination path
            // (there is no `--overwrite` gate on this form).
            super::fixture::finalize_and_write(draft, path, true)?;
            info!(path = %path.display(), "Wrote self-validating fixture");
        }
        if mismatched {
            return Err(ReplayError::VerificationMismatch { mismatched: 1, total: 1 });
        }
        Ok(())
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
            if self.is_batch() {
                self.batch_rpc_args().build_provider().await?
            } else {
                self.rpc_args.build_provider().await?
            }
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

    /// The RPC args a batch run actually uses: on-disk cache persistence is opt-in for batch.
    ///
    /// A batch scan walks linear history — its request keys are block-scoped and essentially
    /// never repeat across runs, so a shared cache file buys almost no hits, while its
    /// clean-exit persist re-reads, merges, and rewrites the whole file under a cross-process
    /// lock. That exit tail grows linearly with the file and serializes across concurrent
    /// batch processes sharing the default cache directory, so batch mode keeps the disk
    /// cache off unless the invocation says otherwise. The in-memory LRU (and its
    /// intra-run reuse across a block's transactions) is unaffected.
    ///
    /// Two flags say otherwise. `--rpc.cache-dir` names the file to use, and `--rpc.clear-cache`
    /// asks for that file to be deleted — a request that only means something while the disk
    /// cache is engaged, so forcing the cache off would silently swallow it and leave the
    /// polluted file in place for the next run. Either flag therefore opts the batch run back
    /// into the disk cache. An explicit `--rpc.no-cache-file` still wins over both, keeping the
    /// same meaning it has outside batch mode.
    fn batch_rpc_args(&self) -> RpcArgs {
        let mut args = self.rpc_args.clone();
        if args.cache_dir.is_none() && !args.clear_cache && !args.no_cache_file {
            info!(
                "Batch replay leaves the on-disk RPC cache disabled; \
                 pass --rpc.cache-dir to enable it"
            );
            args.no_cache_file = true;
        }
        args
    }

    /// Fetch the transaction, its block, and preceding transaction hashes from the provider.
    async fn fetch_replay_context<P>(
        &self,
        provider: &P,
        tx_hash: B256,
        chain_id: u64,
    ) -> Result<ReplayContext>
    where
        P: Provider<op_alloy_network::Optimism>,
    {
        info!(tx_hash = %tx_hash, "Fetching transaction");
        // The user supplied this hash and nothing the endpoint served so far
        // claims it exists, so `Ok(None)` is a definitive "unknown transaction"
        // rather than an inconsistency — unlike the block-body-derived lookups
        // further down, which the endpoint has already vouched for.
        let target_tx = provider
            .get_transaction_by_hash(tx_hash)
            .await
            .map_err(|e| ReplayError::RpcError(format!("Failed to fetch transaction: {e}")))?
            .ok_or_else(|| ReplayError::TransactionNotFound(tx_hash))?;
        debug!(block_number = ?target_tx.block_number, "Transaction found");

        // Classify the target from its `(block_number, block_hash)` pair before
        // anything else is fetched. Every shape the endpoint can return is
        // handled explicitly, so a contradictory row cannot fall through into the
        // pending arm, and a shape that can never be replayed is answered from the
        // metadata alone — no block fetch precedes the verdict, and no fetch
        // failure can mask it. A mined target keeps its inclusion hash here, which
        // is what the block fetched below is anchored against.
        let mined: Option<(u64, B256)> = match (target_tx.block_number, target_tx.block_hash) {
            (Some(number), Some(inclusion)) => Some((number, inclusion)),
            // A mined transaction without an inclusion hash is an unanchored
            // view: the block number alone cannot prove which block body the
            // target belongs to, so there is nothing to anchor the replay to.
            (Some(number), None) => {
                return Err(ReplayError::RpcError(format!(
                    "endpoint reported a mined transaction in block {number} without an \
                     inclusion hash: unanchored view"
                )))
            }
            // A hash proves inclusion; a null number denies it. That pair is
            // self-contradictory metadata, not a pending transaction.
            (None, Some(inclusion)) => {
                return Err(ReplayError::RpcError(format!(
                    "endpoint reported inclusion hash {inclusion} without a block \
                     number: contradictory metadata"
                )))
            }
            (None, None) => None,
        };
        let is_pending = mined.is_none();

        let (state_base_block, block_number) = if let Some((n, _)) = mined {
            (n - 1, n)
        } else {
            let latest = provider
                .get_block_number()
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?;
            (latest, latest)
        };
        debug!(
            state_base_block = state_base_block,
            block = block_number,
            is_pending,
            "Block numbers determined",
        );

        // A mined target forks from its parent block, which is a different block
        // than the one it is replayed in. A pending target forks from the latest
        // block, which *is* the block it is replayed in: fetching that one height
        // twice lets a reorg or a load-balanced endpoint answer the two roles
        // from different blocks, and the replay would then run a pre-state from
        // one view under a block environment from another. The pending path
        // therefore fetches once and uses the same block for both roles, so the
        // two roles cannot disagree at all.
        let parent_block = if is_pending {
            None
        } else {
            Some(
                provider
                    .get_block_by_number(state_base_block.into())
                    .await
                    .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
                    .ok_or(ReplayError::BlockNotFound(state_base_block))?,
            )
        };
        let block = provider
            .get_block_by_number(block_number.into())
            .await
            .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
            .ok_or(ReplayError::BlockNotFound(block_number))?;
        let parent_block = parent_block.unwrap_or_else(|| block.clone());

        // Parent/block linkage guard: the two blocks above were fetched by
        // number in separate calls, so across a reorg or a load-balanced
        // endpoint serving divergent views `eth_getBlockByNumber(N-1)` can
        // return a block that is not the parent of the block being replayed.
        // Forking from that state would silently execute against the wrong
        // pre-state, and the divergence would surface later as a receipt
        // mismatch rather than as the infrastructure failure it is.
        //
        // A pending transaction has no such pair: its state base *is* the
        // latest block, so one fetch fills both roles and there is no linkage,
        // no inclusion, and no membership to check.
        if let Some((_, reported)) = mined {
            let parent_hash = parent_block.hash();
            let expected_parent = block.header.parent_hash();
            if parent_hash != expected_parent {
                return Err(ReplayError::RpcError(format!(
                    "parent block hash {parent_hash} != block parent_hash {expected_parent}: the \
                     parent block describes a different chain than the block being replayed (reorg \
                     in progress, or a load-balanced endpoint serving divergent views); retry once \
                     the chain settles"
                )));
            }

            // Inclusion guard: the linkage above only proves the two fetched
            // blocks belong to one chain, not that the target belongs to them.
            // The lookup that resolved the target reported which block includes
            // it, in a separate call, so a reorg or a load-balanced endpoint can
            // answer both numbered fetches from a replacement block the target is
            // not part of. Replaying that block anyway executes the target
            // against a body it never ran in. The reported hash is the one the
            // metadata classification kept, so a mined target always has one to
            // anchor against.
            let fetched = block.hash();
            if reported != fetched {
                return Err(ReplayError::RpcError(format!(
                    "block {block_number} has hash {fetched}, but the target transaction was \
                     resolved as included in {reported}: the endpoint served divergent views of \
                     this block (reorg in progress, or a load-balanced endpoint); retry once the \
                     chain settles"
                )));
            }
        }

        // The preceding transactions are the block-body entries ahead of the
        // target, so the target's own position in that body defines the set and
        // the body must contain it. A body that does not list the target would
        // silently make every transaction of the block count as preceding and
        // execute the target after the whole block.
        let mut preceding_tx_hashes = vec![];
        if !is_pending {
            let mut found = false;
            for hash in block.transactions.hashes() {
                if hash == tx_hash {
                    found = true;
                    break;
                }
                preceding_tx_hashes.push(hash);
            }
            if !found {
                return Err(ReplayError::RpcError(format!(
                    "block {block_number} ({}) does not list target transaction {tx_hash}, which \
                     the endpoint resolved as included in it: the endpoint served divergent views \
                     of this block (reorg in progress, or a load-balanced endpoint); retry once \
                     the chain settles",
                    block.hash(),
                )));
            }
        }

        debug!(chain_id, preceding_count = preceding_tx_hashes.len(), "Replay context ready");

        Ok(ReplayContext { tx_hash, target_tx, parent_block, block, chain_id, preceding_tx_hashes })
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
        // `--override.spec` replaces the whole execution world, not just the EVM semantics: the
        // synthesized schedule drives the pre-block predeploys and the block-level limits too, so
        // the replay is a coherent what-if rather than a mix of the historical setup with forced
        // semantics.
        let chain_hardforks = get_hardfork_config(ctx.chain_id);
        let spec_override = self.resolve_spec_override()?;
        if let Some(spec_override) = spec_override {
            info!(spec_override = %spec_override, "Overriding EVM spec");
        }
        let hardforks = ReplayHardforks::resolve(&chain_hardforks, spec_override);
        let spec = hardforks.spec_id(ctx.block.header.timestamp());
        let chain_args = ChainArgs { chain_id: ctx.chain_id, spec: spec.to_string() };
        debug!(chain_id = ctx.chain_id, spec = %spec, "Chain configuration");

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
        let evm_env = EvmEnv::new(chain_args.create_cfg_env()?, block_env);

        // For `--dump-fixture`, snapshot the two inputs a fixture
        // needs before the external env is moved into the factory: the effective
        // MegaETH external environment, and the on-chain receipt gas used as the
        // fidelity anchor. They live or die together (kept in one `Option`), so the
        // fixture builder never has to assume one without the other.
        //
        // The receipt is fetched here (before the executor borrows the database) so
        // it is captured by `--rpc.capture-file`. A fixture/benchmark is only
        // meaningful if the local replay reproduces the receipt's gas and success
        // status — a mismatch means a wrong spec or hardfork config, which
        // self-validation alone cannot catch.
        let fixture_inputs = if self.dump_fixture.is_some() {
            // A pending transaction has no receipt yet, so the fidelity gate cannot
            // run; fail clearly instead of surfacing the receipt lookup's confusing
            // `TransactionNotFound`.
            if ctx.target_tx.block_number.is_none() {
                return Err(ReplayError::Other(
                    "--dump-fixture does not support pending transactions: the fidelity \
                     gate needs the on-chain receipt, which does not exist yet"
                        .to_string(),
                ));
            }
            // Sort the accessed buckets/oracle slots so the dumped fixture is
            // byte-reproducible: these come from hash-map iteration, whose order
            // is otherwise non-deterministic across runs (noisy diffs, and an
            // online dump would not byte-match an offline re-dump).
            let mut bucket_capacities = external_envs.bucket_capacities();
            bucket_capacities.sort_unstable();
            let mut oracle_storage = external_envs.oracle_storage();
            oracle_storage.sort_unstable();
            let mega_env = state_test::types::MegaEnv { bucket_capacities, oracle_storage };
            let receipt = provider
                .get_transaction_receipt(ctx.tx_hash)
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
                .ok_or(ReplayError::TransactionNotFound(ctx.tx_hash))?;
            // Anchor the receipt to the replayed block: across a reorg or a
            // load-balanced endpoint serving divergent views, the receipt can
            // describe a different inclusion than the block fetched earlier
            // (including a receipt with `blockHash: null`). Same check as
            // `--verify-receipt` so dump and verify agree on unanchored receipts.
            verify::check_inclusion(receipt.block_hash(), ctx.block.hash())
                .map_err(ReplayError::Other)?;
            // RLP-hash the receipt's logs with the same helper the state-test
            // runner uses for `logsRoot`, so the dump can check the replay's logs
            // against the chain (the rich RPC logs' `inner` is the consensus log).
            let receipt_logs: Vec<_> =
                receipt.inner.logs().iter().map(|log| log.inner.clone()).collect();
            let anchor = super::fixture::OnchainAnchor {
                gas_used: receipt.gas_used(),
                success: receipt.inner.status(),
                logs_root: state_test::utils::log_rlp_hash(&receipt_logs),
            };
            Some((mega_env, anchor))
        } else {
            None
        };

        // For `--verify-receipt`, fetch the on-chain receipt here — before the
        // executor borrows the database, and with the same call shape the
        // fixture path uses — so `--rpc.capture-file` records it and a later
        // offline run verifies without network access. It is compared against
        // the replay's own receipt once the block is finished.
        let onchain_receipt = if self.verify_receipt {
            let receipt = verify::fetch_receipt(provider, ctx.tx_hash).await?;
            // A receipt describing a different inclusion than the replayed block
            // would compare the replay against the wrong on-chain execution:
            // that is an infrastructure failure, not a verification mismatch.
            verify::check_inclusion(receipt.block_hash(), ctx.block.hash())
                .map_err(ReplayError::RpcError)?;
            Some(receipt)
        } else {
            None
        };

        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
        let block_executor_factory =
            MegaBlockExecutorFactory::new(hardforks, evm_factory, OpAlloyReceiptBuilder::default());
        // Both the per-transaction and the block-level dimensions come from the fork resolved out
        // of the schedule above, so a spec override moves all of them at once. The no-override
        // path keeps the "no fork active" failure: a block older than the chain's first hardfork
        // has no limits to execute under. A synthesized schedule always has one active.
        let block_limits = BlockLimits::from_hardfork_and_block_gas_limit(
            hardforks.hardfork(ctx.block.header.timestamp()).ok_or(ReplayError::Other(format!(
                "No `MegaHardfork` active at block timestamp: {}",
                ctx.block.header.timestamp()
            )))?,
            ctx.block.header.gas_limit(),
        );

        // The spec the target transaction will execute under (after any override),
        // captured before `evm_env` is moved into the executor.
        let executed_spec = evm_env.cfg_env.spec;

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

        block_executor.apply_pre_execution_changes().map_err(ReplayError::BlockExecutionError)?;

        // Execute preceding transactions
        info!(preceding_count = ctx.preceding_tx_hashes.len(), "Executing preceding transactions",);
        for tx_hash in &ctx.preceding_tx_hashes {
            debug!(tx_hash = %tx_hash, "Executing preceding transaction");
            // These hashes were read out of the block body this endpoint already
            // served, so `Ok(None)` contradicts an answer it gave itself: the
            // endpoint is inconsistent (reorg, or a load-balanced backend serving
            // divergent views), not definitively denying the hash. Only the
            // user-supplied target lookup keeps `TransactionNotFound`, because
            // there the null is a definitive answer about a hash the caller asked
            // about.
            let tx = provider
                .get_transaction_by_hash(*tx_hash)
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
                .ok_or(ReplayError::BlockBodyTransactionNull(*tx_hash))?;
            let outcome = block_executor
                .run_transaction(tx.as_recovered())
                .map_err(ReplayError::BlockExecutionError)?;
            trace!(tx_hash = %tx_hash, ?outcome, "Preceding transaction executed");
            block_executor
                .commit_transaction_outcome(outcome)
                .map_err(ReplayError::BlockExecutionError)?;
        }

        // Clear block hash reads accumulated by the preceding transactions so the
        // fixture gate below sees only the target transaction's BLOCKHASH reads.
        block_executor.clear_accessed_block_hashes();

        // Execute target transaction. Override-incompatibility with
        // --dump-fixture is validated up front in `run()`.
        info!("Executing target transaction");
        if self.tx_override_args.has_overrides() {
            info!(overrides = ?self.tx_override_args, "Applying transaction overrides");
        }
        let wrapped_tx = self.tx_override_args.wrap(ctx.target_tx.as_recovered())?;
        let pre_execution_nonce = block_executor
            .evm()
            .db_ref()
            .basic_ref(wrapped_tx.inner().signer())?
            .map(|acc| acc.nonce)
            .unwrap_or(0);

        block_executor.inspector_mut().fuse();
        let outcome =
            block_executor.run_transaction(wrapped_tx).map_err(ReplayError::BlockExecutionError)?;
        trace!(tx_hash = %ctx.target_tx.inner.inner.tx_hash(), ?outcome, "Target transaction executed");
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
                block_executor.inspector(),
                &result_and_state,
                block_executor.evm().db_ref(),
            )
        });

        // Build the self-validating fixture draft while the database still reflects
        // the pre-target-transaction state (preceding txs committed, target not yet).
        let fixture = match fixture_inputs {
            Some((mega_env, anchor)) => {
                // A dumped fixture cannot faithfully reproduce BLOCKHASH: the
                // state-test runner does not seed block hashes, so the isolated
                // re-execution would read default hashes instead of the ones this
                // replay observed. The access record is cleared after the preceding
                // transactions, so it holds exactly the target transaction's reads;
                // if the target read any block hash, refuse to dump rather than
                // write a fixture that self-validates against the wrong roots.
                let accessed_block_hashes = block_executor.get_accessed_block_hashes();
                if !accessed_block_hashes.is_empty() {
                    return Err(ReplayError::Other(format!(
                        "--dump-fixture does not support transactions that read block \
                         hashes (BLOCKHASH): {} block hash(es) were accessed and the \
                         fixture cannot faithfully reproduce them",
                        accessed_block_hashes.len()
                    )));
                }
                Some(super::fixture::build_draft(
                    block_executor.evm().db_ref(),
                    &evm_state,
                    ctx.chain_id,
                    executed_spec,
                    &ctx.block,
                    &ctx.target_tx,
                    super::fixture::FixtureInputs { mega_env, result: &exec_result, anchor },
                )?)
            }
            None => None,
        };

        let gas_used = block_executor
            .commit_transaction_outcome(outcome)
            .map_err(ReplayError::BlockExecutionError)?;
        let duration = start.elapsed();

        let (evm, block_result) =
            block_executor.finish().map_err(ReplayError::BlockExecutionError)?;
        let (db, _) = evm.finish();
        db.merge_transitions(BundleRetention::Reverts);
        let receipt_envelope = block_result.receipts.last().unwrap().clone();
        trace!(?receipt_envelope, "Receipt envelope obtained");

        // Block-global log index: cumulative log count of every preceding
        // receipt in this block (same data `finish()` harvested).
        let first_log_index: u64 = block_result
            .receipts
            .iter()
            .rev()
            .skip(1)
            .map(|envelope| envelope.logs().len() as u64)
            .sum();

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
            first_log_index,
        );

        let verification = onchain_receipt.as_ref().map(|onchain| {
            verify::compare(
                &verify::ReceiptFacts::from_receipt(&onchain.inner),
                &verify::ReceiptFacts::from_receipt(&receipt),
            )
        });
        if let Some(verification) = &verification {
            debug!(matched = verification.matched, "On-chain receipt verified");
        }

        Ok(ReplayOutcome {
            outcome: EvmeOutcome {
                pre_execution_nonce,
                exec_result,
                state: evm_state,
                exec_time: duration,
                trace_data,
            },
            receipt,
            fixture,
            verification,
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
            summary.verification = result.verification.as_ref().map(|verification| {
                serde_json::to_value(verification).expect("failed to serialize verification")
            });
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
            if let Some(verification) = &result.verification {
                println!();
                println!("{}", verification.verdict_line());
            }
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

/// Whether any trace option was set on the command line.
///
/// `--tracer` carries a default, so it counts as set only when it names a
/// non-default tracer.
fn has_trace_args(args: &run::TraceArgs) -> bool {
    args.trace ||
        args.trace_output_file.is_some() ||
        !matches!(args.tracer, TracerType::Opcode) ||
        args.trace_opcode_disable_memory ||
        args.trace_opcode_disable_stack ||
        args.trace_opcode_disable_storage ||
        args.trace_opcode_enable_return_data ||
        args.trace_call_only_top_call ||
        args.trace_call_with_log ||
        args.trace_prestate_diff_mode ||
        args.trace_prestate_disable_code ||
        args.trace_prestate_disable_storage
}

/// Whether any state dump option was set on the command line.
fn has_dump_args(args: &run::StateDumpArgs) -> bool {
    args.dump || args.dump_output_file.is_some()
}

/// Build a [`BlockEnv`] from the RPC block header.
///
/// Reads `excess_blob_gas` directly from the header rather than using a
/// hardcoded default, so blob-fee-sensitive opcodes (e.g. `BLOBBASEFEE`)
/// match on-chain semantics during replay.
pub(super) fn retrieve_block_env(block: &Block<Transaction>) -> Result<BlockEnv> {
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

    const TX: &str = "0x323ddc8e67dfc134284d78c65f3c1dc7ff45ba1db02eeaf62e211ae3253478ef";
    const RPC: [&str; 2] = ["--rpc.replay-file", "/tmp/envelope.json"];

    /// Parse a `replay` invocation, prefixing the shared offline RPC flags.
    fn parse(extra: &[&str]) -> std::result::Result<Cmd, clap::Error> {
        let mut argv = vec!["replay"];
        argv.extend_from_slice(&RPC);
        argv.extend_from_slice(extra);
        Cmd::try_parse_from(argv)
    }

    /// Parse a batch invocation carrying one extra flag, and return the
    /// validation error message it must be rejected with.
    fn batch_rejection(extra: &[&str]) -> String {
        let mut argv = vec!["--block", "22945844"];
        argv.extend_from_slice(extra);
        let cmd = parse(&argv).expect("flags should parse");
        cmd.validate().expect_err("batch mode must reject the flag").to_string()
    }

    /// Parse an online `replay` invocation (no offline RPC flags).
    fn parse_online(extra: &[&str]) -> Cmd {
        let mut argv = vec!["replay", "--rpc", "http://localhost:1"];
        argv.extend_from_slice(extra);
        Cmd::try_parse_from(argv).expect("online flags should parse")
    }

    #[test]
    fn test_batch_rpc_args_disables_disk_cache_by_default() {
        let cmd = parse_online(&["--block", "1"]);
        assert!(!cmd.rpc_args.no_cache_file, "the flag itself must default off");
        assert!(
            cmd.batch_rpc_args().no_cache_file,
            "a batch run without --rpc.cache-dir must not touch the disk cache",
        );
    }

    #[test]
    fn test_batch_rpc_args_explicit_cache_dir_opts_back_in() {
        let cmd = parse_online(&["--block", "1", "--rpc.cache-dir", "/tmp/evme-cache"]);
        let args = cmd.batch_rpc_args();
        assert!(!args.no_cache_file, "an explicit --rpc.cache-dir must keep persistence on");
        assert_eq!(args.cache_dir.as_deref(), Some(std::path::Path::new("/tmp/evme-cache")));
    }

    #[test]
    fn test_batch_rpc_args_explicit_clear_cache_opts_back_in() {
        let cmd = parse_online(&["--block", "1", "--rpc.clear-cache"]);
        let args = cmd.batch_rpc_args();
        assert!(
            !args.no_cache_file,
            "--rpc.clear-cache asks for the disk cache file to be deleted, which only \
             happens while the disk cache is engaged",
        );
        assert!(args.clear_cache, "the clear request itself must survive");
        assert_eq!(args.cache_dir, None, "the default cache path is the one being cleared");
    }

    #[test]
    fn test_batch_rpc_args_keeps_explicit_no_cache_file() {
        let cmd = parse_online(&["--block", "1", "--rpc.no-cache-file"]);
        assert!(cmd.batch_rpc_args().no_cache_file, "--rpc.no-cache-file must stay honored");
    }

    /// `--rpc.no-cache-file` and `--rpc.clear-cache` are not mutually exclusive at the
    /// parser level. Batch mode must not reinterpret the pair: it passes both through so
    /// the combination behaves exactly as it does in single-transaction mode.
    #[test]
    fn test_batch_rpc_args_passes_no_cache_file_with_clear_cache_through() {
        let cmd = parse_online(&["--block", "1", "--rpc.no-cache-file", "--rpc.clear-cache"]);
        let args = cmd.batch_rpc_args();
        assert!(args.no_cache_file, "--rpc.no-cache-file must stay honored alongside clear");
        assert!(args.clear_cache, "the clear flag must be passed through unmodified");
    }

    /// `--rpc.cache-dir` and `--rpc.clear-cache` together name the file to clear.
    #[test]
    fn test_batch_rpc_args_cache_dir_with_clear_cache_opts_back_in() {
        let cmd = parse_online(&[
            "--block",
            "1",
            "--rpc.cache-dir",
            "/tmp/evme-cache",
            "--rpc.clear-cache",
        ]);
        let args = cmd.batch_rpc_args();
        assert!(!args.no_cache_file, "both flags opt the batch run into the disk cache");
        assert!(args.clear_cache);
        assert_eq!(args.cache_dir.as_deref(), Some(std::path::Path::new("/tmp/evme-cache")));
    }

    #[test]
    fn test_replay_target_group_accepts_each_form() {
        assert!(matches!(
            parse(&[TX]).expect("positional").resolve_mode().expect("mode"),
            ReplayMode::Single(_)
        ));
        assert!(matches!(
            parse(&["--block", "0x15e2034"]).expect("block").resolve_mode().expect("mode"),
            ReplayMode::Batch(batch::BatchMode::Block(22_945_844)),
        ));
        // `--tx-file` reads the file, so only the parse is checked here.
        assert_eq!(
            parse(&["--tx-file", "/tmp/list.txt"]).expect("tx-file").tx_file,
            Some(PathBuf::from("/tmp/list.txt")),
        );
    }

    #[test]
    fn test_replay_target_group_is_required() {
        let err = parse(&[]).expect_err("a replay target is required");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn test_replay_target_group_is_mutually_exclusive() {
        for extra in [
            vec![TX, "--block", "1"],
            vec![TX, "--tx-file", "/tmp/list.txt"],
            vec!["--block", "1", "--tx-file", "/tmp/list.txt"],
        ] {
            let err = parse(&extra).expect_err("targets must be mutually exclusive");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "unexpected error for {extra:?}: {err}"
            );
        }
    }

    #[test]
    fn test_batch_rejects_single_transaction_only_flags() {
        for (extra, expected) in [
            (vec!["--dump-fixture", "/tmp/f.json"], "--dump-fixture"),
            (vec!["--override.gas-limit", "50000"], "transaction overrides"),
            (vec!["--override.value", "1ether"], "transaction overrides"),
            (vec!["--override.input", "0xdeadbeef"], "transaction overrides"),
            (vec!["--override.input-file", "/tmp/in.hex"], "transaction overrides"),
            (vec!["--override.spec", "Rex4"], "--override.spec"),
            (vec!["--trace"], "trace options"),
            (vec!["--trace.output", "/tmp/t.json"], "trace options"),
            (vec!["--tracer", "call"], "trace options"),
            (vec!["--trace.call.with-log"], "trace options"),
            (vec!["--trace.prestate.diff-mode"], "trace options"),
            (vec!["--dump"], "state dump options"),
            (vec!["--dump.output", "/tmp/s.json"], "state dump options"),
        ] {
            let message = batch_rejection(&extra);
            assert!(
                message.contains(expected) && message.contains("batch replay"),
                "unexpected rejection for {extra:?}: {message}"
            );
        }
    }

    /// `--verify-receipt` is a whole-corpus flag: both replay modes take it.
    #[test]
    fn test_verify_receipt_is_accepted_in_both_modes() {
        for extra in [vec![TX], vec!["--block", "1"], vec!["--tx-file", "/tmp/list.txt"]] {
            let mut argv = extra.clone();
            argv.push("--verify-receipt");
            let cmd = parse(&argv).expect("--verify-receipt should parse");
            assert!(cmd.verify_receipt, "the flag must be recorded for {extra:?}");
            cmd.validate()
                .unwrap_or_else(|e| panic!("--verify-receipt must be accepted for {extra:?}: {e}"));
        }
    }

    /// The flag defaults to off, so a replay without it does no receipt fetch.
    #[test]
    fn test_verify_receipt_defaults_to_off() {
        assert!(!parse(&[TX]).expect("parse").verify_receipt);
    }

    /// `--dump-fixture-dir` is batch-only; single-transaction mode keeps
    /// `--dump-fixture` and must reject the dir flag.
    #[test]
    fn test_dump_fixture_dir_rejected_in_single_transaction_mode() {
        let cmd = parse(&["--dump-fixture-dir", "/tmp/fixtures", TX]).expect("parse");
        let message =
            cmd.validate().expect_err("single-tx must reject --dump-fixture-dir").to_string();
        assert!(
            message.contains("--dump-fixture-dir") && message.contains("batch"),
            "unexpected rejection: {message}"
        );
    }

    /// Batch mode accepts `--dump-fixture-dir` and still rejects the single-file
    /// dump flag (use the dir form for sedimentation sweeps).
    #[test]
    fn test_dump_fixture_dir_accepted_in_batch_mode() {
        let cmd = parse(&["--block", "1", "--dump-fixture-dir", "/tmp/fixtures"]).expect("parse");
        cmd.validate().expect("--dump-fixture-dir must be accepted in batch mode");
        assert_eq!(cmd.dump_fixture_dir, Some(PathBuf::from("/tmp/fixtures")));

        let with_overwrite =
            parse(&["--tx-file", "/tmp/list.txt", "--dump-fixture-dir", "/tmp/f", "--overwrite"])
                .expect("parse");
        with_overwrite.validate().expect("--overwrite is allowed with --dump-fixture-dir");
        assert!(with_overwrite.overwrite);
    }

    /// The two dump forms are mutually exclusive even when one would otherwise
    /// be valid for the selected mode.
    #[test]
    fn test_dump_fixture_forms_are_mutually_exclusive() {
        let cmd = parse(&[
            "--block",
            "1",
            "--dump-fixture",
            "/tmp/f.json",
            "--dump-fixture-dir",
            "/tmp/fixtures",
        ])
        .expect("parse");
        // Batch validation rejects --dump-fixture first; either way both must not run.
        let message = cmd.validate().expect_err("both dump forms must be rejected").to_string();
        assert!(message.contains("--dump-fixture"), "unexpected rejection: {message}");
    }

    /// `--block 0` is rejected as input rather than replayed into an empty,
    /// silent, exit-0 run.
    #[test]
    fn test_batch_rejects_block_zero() {
        let err = parse(&["--block", "0"])
            .expect("parse")
            .validate()
            .expect_err("genesis cannot be replayed");
        let message = err.to_string();
        assert!(message.contains("--block 0"), "message={message}");
        assert!(message.contains("genesis"), "message={message}");
    }

    #[test]
    fn test_batch_accepts_the_flags_it_supports() {
        parse(&["--block", "1", "--json"]).expect("parse").validate().expect("--json is allowed");
        parse(&["--tx-file", "/tmp/list.txt"])
            .expect("parse")
            .validate()
            .expect("plain batch replay is allowed");
    }

    /// The single-transaction path keeps accepting every flag batch mode rejects.
    #[test]
    fn test_single_transaction_path_keeps_all_flags() {
        for extra in [
            vec!["--trace", "--tracer", "call"],
            vec!["--dump", "--dump.output", "/tmp/s.json"],
            vec!["--override.spec", "Rex4"],
            vec!["--override.gas-limit", "50000"],
        ] {
            let mut argv = vec![TX];
            argv.extend_from_slice(&extra);
            parse(&argv)
                .expect("parse")
                .validate()
                .unwrap_or_else(|e| panic!("single-transaction replay must accept {extra:?}: {e}"));
        }
    }

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
