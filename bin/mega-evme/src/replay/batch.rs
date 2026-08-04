//! Batch replay driver: replay many transactions inside a single process.
//!
//! The single-transaction path ([`super::cmd`]) builds a provider, forks state at
//! the parent block, and executes one block per process. Verifying a large corpus
//! that way pays the provider/cache setup once per transaction, which dominates
//! the actual EVM work. This module reuses one provider and one RPC cache for the
//! whole run, groups the requested transactions by their containing block, and
//! executes each block exactly once while recording the result of every target it
//! passes through.
//!
//! Every RPC call issued here has the same shape as the single-transaction path
//! (`eth_getTransactionByHash`, `eth_getBlockByNumber` with hash-only bodies, and
//! the state reads behind [`EvmeState::new_forked`]), so an offline envelope
//! captured by single-transaction replays serves batch runs without a miss.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Block;
use mega_evm::{
    alloy_evm::{
        block::{BlockExecutionError, BlockExecutor, BlockValidationError},
        Evm, EvmEnv,
    },
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{result::ExecutionResult, ContextTr},
        database::{states::bundle_state::BundleRetention, StateBuilder},
        DatabaseRef,
    },
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaHaltReason,
    MegaHardforks, MegaSpecId,
};
use op_alloy_rpc_types::Transaction;
use serde::Serialize;
use state_test::types::MegaEnv;
use tracing::{debug, info, warn};

use crate::{
    common::{
        op_receipt_to_tx_receipt, print_execution_summary, print_receipt, BatchFailureCounts,
        EvmeExternalEnvs, ExecutionSummary, ExitCode, OpTxReceipt,
    },
    replay::get_hardfork_config,
    ChainArgs, EvmeState,
};

use super::{
    cmd::retrieve_block_env,
    fixture,
    verify::{self, ReceiptFacts, VerificationOutcome},
    ReplayError, Result,
};

/// How a batch run reports its targets.
#[derive(Debug, Clone)]
pub(super) struct ReportArgs {
    /// Emit one NDJSON line per target instead of the human-readable summary.
    pub json: bool,
    /// Verify every target against its on-chain receipt.
    pub verify_receipt: bool,
    /// When set, dump a self-validating fixture for every successful target into
    /// this directory as `<DIR>/<tx_hash>.json`.
    pub dump_fixture_dir: Option<PathBuf>,
    /// Replace existing fixture files under [`Self::dump_fixture_dir`].
    pub overwrite: bool,
}

/// Per-target fixture dump outcome reported on the NDJSON / human result line.
///
/// Exactly one field is set: the fixture was written, expectedly skipped
/// (fidelity gate, BLOCKHASH, unsupported shape), or could not be written. A
/// write failure is reported here rather than replacing the target's result,
/// so a target that did replay keeps its result — including its receipt
/// verification verdict — and still fails the run.
#[derive(Debug, Clone, Serialize)]
struct FixtureReport {
    /// Absolute or as-written path of a successfully written fixture.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// Why the fixture was not written for this target.
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<String>,
    /// Why writing the fixture failed. Counts as an execution-class failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl FixtureReport {
    fn written(path: &Path) -> Self {
        Self { path: Some(path.display().to_string()), skipped: None, error: None }
    }

    fn skipped(reason: impl Into<String>) -> Self {
        Self { path: None, skipped: Some(reason.into()), error: None }
    }

    fn error(message: impl Into<String>) -> Self {
        Self { path: None, skipped: None, error: Some(message.into()) }
    }

    /// Whether the fixture the run was asked to write could not be written.
    const fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// One-line human summary printed under the transaction header.
    fn human_line(&self) -> String {
        if let Some(path) = &self.path {
            format!("fixture: written to {path}")
        } else if let Some(reason) = &self.skipped {
            format!("fixture: skipped ({reason})")
        } else if let Some(message) = &self.error {
            format!("fixture: FAILED ({message})")
        } else {
            "fixture: (no report)".to_string()
        }
    }
}

/// What a batch run was asked to replay.
#[derive(Debug)]
pub(super) enum BatchMode {
    /// Transaction hashes read from `--tx-file`, in file order.
    TxList(Vec<B256>),
    /// Every transaction of the block given by `--block`.
    Block(u64),
}

/// Why a target transaction produced no execution result.
///
/// Execution outcomes (success, revert, halt) are normal results and never map
/// to one of these kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchErrorKind {
    /// The transaction hash is unknown to the endpoint.
    NotFound,
    /// The transaction exists but is not mined yet (no block number).
    Pending,
    /// An RPC call failed or returned nothing.
    Rpc,
    /// The block executor rejected the transaction or the block setup failed.
    Execution,
}

impl BatchErrorKind {
    /// Wire name used in the NDJSON error line and the human-readable output.
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Pending => "pending",
            Self::Rpc => "rpc",
            Self::Execution => "execution",
        }
    }
}

/// Outcome of a single target transaction.
enum BatchEntry {
    /// The transaction executed and produced a result (success, revert, or halt).
    Executed(Box<ExecutedTx>),
    /// The transaction could not be executed.
    Failed(FailedTx),
}

impl BatchEntry {
    /// Hash of the target this entry reports on.
    const fn tx_hash(&self) -> B256 {
        match self {
            Self::Executed(tx) => tx.tx_hash,
            Self::Failed(tx) => tx.tx_hash,
        }
    }
}

/// A target transaction that ran to completion.
struct ExecutedTx {
    tx_hash: B256,
    block_number: u64,
    tx_index: u64,
    exec_result: ExecutionResult<MegaHaltReason>,
    contract_address: Option<Address>,
    exec_time: Duration,
    receipt: OpTxReceipt,
    /// On-chain receipt verdict, present iff `--verify-receipt` was given.
    verification: Option<VerificationOutcome>,
    /// Fixture dump outcome, present iff `--dump-fixture-dir` was given.
    fixture: Option<FixtureReport>,
}

/// A target transaction that hit an infrastructure failure.
struct FailedTx {
    tx_hash: B256,
    kind: BatchErrorKind,
    message: String,
}

/// Running tally of a batch run's per-target outcomes.
///
/// A batch reports each target as it goes and fails once at the end, so the
/// outcome classes are counted here rather than recovered from the emitted
/// lines.
#[derive(Debug, Default)]
struct BatchTally {
    /// Targets the run reported on, one per emitted entry.
    reported: usize,
    /// Targets that produced an execution result.
    replayed: usize,
    /// Targets compared against an on-chain receipt.
    verified: usize,
    /// Failed and mismatched targets, by class.
    counts: BatchFailureCounts,
}

impl BatchTally {
    /// Count one reported target.
    fn record(&mut self, entry: &BatchEntry) {
        match entry {
            BatchEntry::Executed(tx) => {
                self.record_executed(tx.verification.as_ref(), tx.fixture.as_ref());
            }
            // A transaction the endpoint does not know, or that is not mined
            // yet, is a definitive answer about the target rather than an
            // unanswered question, so it counts as an execution failure.
            BatchEntry::Failed(tx) => {
                self.reported += 1;
                match tx.kind {
                    BatchErrorKind::Rpc => self.counts.rpc += 1,
                    BatchErrorKind::NotFound |
                    BatchErrorKind::Pending |
                    BatchErrorKind::Execution => self.counts.execution += 1,
                }
            }
        }
    }

    /// Count the findings of one target that produced an execution result.
    ///
    /// A verdict and a fixture failure are independent findings about the same
    /// target: a replay that diverged from its receipt is counted as a mismatch
    /// whether or not its fixture could be written.
    fn record_executed(
        &mut self,
        verification: Option<&VerificationOutcome>,
        fixture: Option<&FixtureReport>,
    ) {
        self.reported += 1;
        self.replayed += 1;
        if let Some(verification) = verification {
            self.verified += 1;
            if !verification.matched {
                self.counts.mismatched += 1;
            }
        }
        // A fixture the run was asked to write and could not is a failure of
        // that target, even though its replay produced a result.
        if fixture.is_some_and(FixtureReport::is_error) {
            self.counts.execution += 1;
        }
    }

    /// Targets that failed, by any class other than a receipt mismatch.
    const fn failed(&self) -> usize {
        self.counts.execution + self.counts.rpc
    }

    /// The run's terminal error, or `None` when every target came out clean.
    ///
    /// Infrastructure failures are reported with their counts by class so the
    /// exit-code mapping resolves the precedence between them and a mismatch; a
    /// run whose only finding is divergence fails as the mismatch it is.
    /// Fixture skips never count as failures; a fixture that could not be
    /// written does, as an execution-class failure of its target.
    fn into_error(self) -> Option<ReplayError> {
        if self.failed() > 0 {
            return Some(ReplayError::BatchFailed(BatchFailureCounts {
                total: self.reported,
                ..self.counts
            }));
        }
        if self.counts.mismatched > 0 {
            return Some(ReplayError::VerificationMismatch {
                mismatched: self.counts.mismatched,
                total: self.verified,
            });
        }
        None
    }
}

/// One block's worth of work.
struct BlockJob {
    /// Number of the block holding the targets.
    number: u64,
    /// Block body, present when planning already fetched it (`--block`).
    block: Option<Block<Transaction>>,
    /// Hashes of the transactions whose results are reported.
    targets: Vec<B256>,
}

/// Fixture work for one target, held until `finish()` succeeds.
///
/// Skips and construction failures are decided against the pre-commit state and
/// carried as a final report. A successfully built draft is written only after
/// the transaction commits and the block finishes — a commit-time rejection or
/// finish failure must not leave a fixture file on disk (and must not clobber a
/// pre-existing file under `--overwrite`).
enum DeferredFixture {
    /// Already decided (skip, construction error, or refused overwrite).
    Report(FixtureReport),
    /// Draft built against pre-commit state; write after `finish()` succeeds.
    /// Boxed so the enum is not dominated by the draft's size on the skip path.
    Ready { draft: Box<fixture::FixtureDraft>, path: PathBuf },
}

/// A target that executed, awaiting the receipt harvested by `finish()`.
struct PendingTarget {
    tx_hash: B256,
    tx_index: u64,
    /// Position of this transaction among the block's committed transactions.
    commit_index: usize,
    exec_result: ExecutionResult<MegaHaltReason>,
    exec_time: Duration,
    gas_used: u64,
    pre_execution_nonce: u64,
    from: Address,
    to: Option<Address>,
    effective_gas_price: u128,
    /// Fixture dump work, present iff `--dump-fixture-dir` was given.
    fixture: Option<DeferredFixture>,
}

/// NDJSON line for a target that produced an execution result.
#[derive(Serialize)]
struct BatchResultLine<'a> {
    tx_hash: B256,
    block_number: u64,
    tx_index: u64,
    #[serde(flatten)]
    summary: &'a ExecutionSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    fixture: Option<&'a FixtureReport>,
}

/// NDJSON line for a target that produced an infrastructure error.
#[derive(Serialize)]
struct BatchErrorLine<'a> {
    tx_hash: B256,
    error: BatchErrorBody<'a>,
}

/// Error payload of a [`BatchErrorLine`].
#[derive(Serialize)]
struct BatchErrorBody<'a> {
    kind: &'static str,
    message: &'a str,
}

/// Replay every requested transaction, reporting one entry per target.
///
/// Returns an error when at least one target produced an infrastructure error
/// entry, so the process exits non-zero; execution outcomes never fail the run.
/// Fixture skips (fidelity gate, BLOCKHASH readers, unsupported tx shapes) do
/// not fail the run. The failure carries the counts by class
/// ([`ReplayError::BatchFailed`]), which decide the exit code. With
/// `--verify-receipt`, a run in which every target replayed but some diverged
/// from its on-chain receipt fails with [`ReplayError::VerificationMismatch`]
/// instead — a distinct variant, so a divergence is never confused with a
/// target that could not be replayed.
pub(super) async fn run<P>(
    provider: &P,
    chain_id: u64,
    mode: &BatchMode,
    external_envs: EvmeExternalEnvs,
    report: ReportArgs,
) -> Result<()>
where
    P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug,
{
    let start = Instant::now();
    let mut tally = BatchTally::default();
    let mut fixtures_written = 0usize;
    let mut fixtures_skipped = 0usize;
    let mut fixtures_failed = 0usize;

    if let Some(dir) = &report.dump_fixture_dir {
        std::fs::create_dir_all(dir).map_err(|e| {
            ReplayError::Other(format!(
                "failed to create --dump-fixture-dir '{}': {e}",
                dir.display()
            ))
        })?;
    }

    let jobs = match mode {
        BatchMode::Block(number) => {
            let block = fetch_block(provider, *number).await?;
            let targets: Vec<B256> = block.transactions.hashes().collect();
            info!(block = number, tx_count = targets.len(), "Batch replay of a whole block");
            vec![BlockJob { number: *number, block: Some(block), targets }]
        }
        BatchMode::TxList(hashes) => {
            let (jobs, failures) = resolve_targets(provider, hashes).await;
            info!(
                requested = hashes.len(),
                blocks = jobs.len(),
                unresolved = failures.len(),
                "Batch replay of a transaction list",
            );
            for failure in failures {
                let entry = BatchEntry::Failed(failure);
                tally.record(&entry);
                emit(&entry, report.json);
            }
            jobs
        }
    };

    for job in jobs {
        let entries = replay_block(provider, chain_id, job, external_envs.clone(), &report).await;
        for entry in entries {
            if let BatchEntry::Executed(tx) = &entry {
                match &tx.fixture {
                    Some(fixture) if fixture.path.is_some() => fixtures_written += 1,
                    Some(fixture) if fixture.is_error() => fixtures_failed += 1,
                    Some(_) => fixtures_skipped += 1,
                    None => {}
                }
            }
            tally.record(&entry);
            emit(&entry, report.json);
        }
    }

    info!(
        replayed = tally.replayed,
        failed = tally.failed(),
        elapsed = ?start.elapsed(),
        "Batch replay finished",
    );
    if report.verify_receipt {
        info!(
            verified = tally.verified,
            mismatched = tally.counts.mismatched,
            "On-chain receipt verification finished",
        );
    }
    if report.dump_fixture_dir.is_some() {
        info!(
            written = fixtures_written,
            skipped = fixtures_skipped,
            failed = fixtures_failed,
            "Fixture dump finished"
        );
    }

    tally.into_error().map_or(Ok(()), Err)
}

/// Resolve each requested hash to its containing block.
///
/// Returns the per-block jobs in ascending block order, plus the failures for
/// hashes that could not be resolved (in the order they were requested).
async fn resolve_targets<P>(provider: &P, hashes: &[B256]) -> (Vec<BlockJob>, Vec<FailedTx>)
where
    P: Provider<op_alloy_network::Optimism>,
{
    let mut grouped: BTreeMap<u64, Vec<B256>> = BTreeMap::new();
    let mut failures = Vec::new();

    for hash in hashes {
        match provider.get_transaction_by_hash(*hash).await {
            Err(e) => failures.push(FailedTx {
                tx_hash: *hash,
                kind: BatchErrorKind::Rpc,
                message: format!("Failed to fetch transaction: {e}"),
            }),
            Ok(None) => failures.push(FailedTx {
                tx_hash: *hash,
                kind: BatchErrorKind::NotFound,
                message: "Transaction not found".to_string(),
            }),
            Ok(Some(tx)) => match tx.block_number {
                Some(number) => grouped.entry(number).or_default().push(*hash),
                None => failures.push(FailedTx {
                    tx_hash: *hash,
                    kind: BatchErrorKind::Pending,
                    message: "Transaction is pending (no block number)".to_string(),
                }),
            },
        }
    }

    let jobs = grouped
        .into_iter()
        .map(|(number, targets)| BlockJob { number, block: None, targets })
        .collect();
    (jobs, failures)
}

/// Replay one block, reporting an entry for every target it was asked about.
///
/// The block is executed exactly once: every transaction runs in order, and each
/// target's result is recorded before the transaction is committed. Receipts are
/// harvested from the finished block, which is why the block's entries are only
/// produced once the block is done.
///
/// When `--dump-fixture-dir` is set, each target's fixture draft is built from
/// the pre-commit state (same moment as the single-transaction dump), gated per
/// target for fidelity and BLOCKHASH, and written only after the block
/// `finish()` succeeds — so a commit-time rejection or finish failure cannot
/// leave a fixture file on disk.
async fn replay_block<P>(
    provider: &P,
    chain_id: u64,
    job: BlockJob,
    external_envs: EvmeExternalEnvs,
    report: &ReportArgs,
) -> Vec<BatchEntry>
where
    P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug,
{
    let BlockJob { number, block, targets } = job;
    let verify_receipt = report.verify_receipt;
    let dump_dir = report.dump_fixture_dir.as_deref();
    let overwrite = report.overwrite;

    if number == 0 {
        return fail_all(&targets, BatchErrorKind::Rpc, "Block 0 has no parent block to fork from");
    }

    let block = match block {
        Some(block) => block,
        None => match fetch_block(provider, number).await {
            Ok(block) => block,
            Err(e) => {
                return fail_all(&targets, BatchErrorKind::Rpc, &e.to_string());
            }
        },
    };
    let parent_block = match fetch_block(provider, number - 1).await {
        Ok(block) => block,
        Err(e) => {
            return fail_all(&targets, BatchErrorKind::Rpc, &e.to_string());
        }
    };

    // Parent/block linkage guard: across a reorg or a load-balanced endpoint
    // serving divergent views, `eth_getBlockByNumber(N-1)` can return a block
    // that is not the parent of the block being replayed. Forking from that
    // state would silently execute against the wrong pre-state.
    let parent_hash = parent_block.hash();
    let expected_parent = block.header.parent_hash();
    if parent_hash != expected_parent {
        let message = format!(
            "parent block hash {parent_hash} != block parent_hash {expected_parent}: the parent \
             block describes a different chain than the block being replayed (reorg in progress, \
             or a load-balanced endpoint serving divergent views); retry once the chain settles"
        );
        return fail_all(&targets, BatchErrorKind::Rpc, &message);
    }

    // Fetch the on-chain receipts before the block runs. Needed for
    // `--verify-receipt` (mismatch vs unverified) and for `--dump-fixture-dir`
    // (fidelity gate). A receipt that cannot be fetched, or that describes a
    // different inclusion than this block, is recorded here and interpreted by
    // each feature below.
    let need_receipts = verify_receipt || dump_dir.is_some();
    let onchain_receipts = if need_receipts {
        fetch_target_receipts(provider, &targets, block.hash()).await
    } else {
        BTreeMap::new()
    };

    // Sorted once so every fixture of this block is byte-reproducible for the
    // same megaEnv (hash-map iteration order is otherwise non-deterministic).
    let mega_env = dump_dir.map(|_| {
        let mut bucket_capacities = external_envs.bucket_capacities();
        bucket_capacities.sort_unstable();
        let mut oracle_storage = external_envs.oracle_storage();
        oracle_storage.sort_unstable();
        MegaEnv { bucket_capacities, oracle_storage }
    });

    let hardforks = get_hardfork_config(chain_id);
    let timestamp = block.header.timestamp();
    let spec = hardforks.spec_id(timestamp);
    let chain_args = ChainArgs { chain_id, spec: spec.to_string() };
    debug!(block = number, chain_id, spec = %spec, "Block configuration");

    let cfg_env = match chain_args.create_cfg_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            return fail_all(&targets, BatchErrorKind::Execution, &e.to_string());
        }
    };
    let block_env = match retrieve_block_env(&block) {
        Ok(env) => env,
        Err(e) => {
            return fail_all(&targets, BatchErrorKind::Execution, &e.to_string());
        }
    };
    let executed_spec = cfg_env.spec;
    let evm_env = EvmEnv::new(cfg_env, block_env);

    let Some(hardfork) = hardforks.hardfork(timestamp) else {
        let message = format!("No `MegaHardfork` active at block timestamp: {timestamp}");
        return fail_all(&targets, BatchErrorKind::Execution, &message);
    };
    let block_limits =
        BlockLimits::from_hardfork_and_block_gas_limit(hardfork, block.header.gas_limit());
    let block_ctx = MegaBlockExecutionCtx::new(
        parent_block.hash(),
        block.header.parent_beacon_block_root(),
        block.header.extra_data().clone(),
        block_limits,
    );

    info!(block = number, fork_block = parent_block.header.number(), "Forking state for block");
    let mut database = match EvmeState::new_forked(
        provider.clone(),
        Some(parent_block.header.number()),
        Default::default(),
        Default::default(),
    )
    .await
    {
        Ok(database) => database,
        Err(e) => {
            return fail_all(&targets, BatchErrorKind::Rpc, &e.to_string());
        }
    };

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let block_executor_factory =
        MegaBlockExecutorFactory::new(&hardforks, evm_factory, OpAlloyReceiptBuilder::default());
    let mut state = StateBuilder::new().with_database(&mut database).with_bundle_update().build();
    let mut block_executor = block_executor_factory.create_executor(&mut state, block_ctx, evm_env);

    if let Err(e) = block_executor.apply_pre_execution_changes() {
        let error = ReplayError::BlockExecutionError(e);
        return fail_all(&targets, classify(&error), &error.to_string());
    }

    let target_set: HashSet<B256> = targets.iter().copied().collect();
    let tx_hashes: Vec<B256> = block.transactions.hashes().collect();
    let mut pending: Vec<PendingTarget> = Vec::new();
    let mut committed = 0usize;

    // Run the block's transactions in order. Any failure aborts the block: the
    // executor state no longer matches the chain, so the remaining targets
    // cannot be replayed faithfully.
    let loop_result: Result<()> = async {
        for (tx_index, tx_hash) in tx_hashes.iter().enumerate() {
            // Isolate BLOCKHASH reads per transaction so a fixture dump sees only
            // the target's own accesses (mirrors the single-tx clear after
            // preceding transactions).
            block_executor.clear_accessed_block_hashes();

            let tx = provider
                .get_transaction_by_hash(*tx_hash)
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
                .ok_or(ReplayError::TransactionNotFound(*tx_hash))?;

            let is_target = target_set.contains(tx_hash);
            let start = Instant::now();
            let pre_execution_nonce = if is_target {
                block_executor
                    .evm()
                    .db_ref()
                    .basic_ref(tx.inner.inner.signer())?
                    .map(|acc| acc.nonce)
                    .unwrap_or(0)
            } else {
                0
            };

            let outcome = block_executor
                .run_transaction(tx.as_recovered())
                .map_err(ReplayError::BlockExecutionError)?;

            // Fixture draft must be built before commit: the pre-state closure
            // is the database after preceding txs, with the target's result
            // state still uncommitted — same moment as the single-tx dump.
            // The draft is only written after `finish()` succeeds (see harvest).
            let fixture = if is_target {
                if let (Some(dir), Some(mega_env)) = (dump_dir, mega_env.as_ref()) {
                    let accessed_block_hashes = block_executor.get_accessed_block_hashes();
                    Some(prepare_target_fixture(
                        block_executor.evm().db_ref(),
                        DumpFixtureArgs {
                            accessed_block_hash_count: accessed_block_hashes.len(),
                            exec_result: &outcome.inner.result,
                            evm_state: &outcome.inner.state,
                            chain_id,
                            executed_spec,
                            block: &block,
                            target_tx: &tx,
                            mega_env: mega_env.clone(),
                            onchain: onchain_receipts.get(tx_hash),
                            dir,
                            overwrite,
                        },
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            // Record the target's result before committing, mirroring the
            // single-transaction path.
            let exec_result = is_target.then(|| outcome.inner.result.clone());
            let gas_used = block_executor
                .commit_transaction_outcome(outcome)
                .map_err(ReplayError::BlockExecutionError)?;
            let commit_index = committed;
            committed += 1;

            if let Some(exec_result) = exec_result {
                pending.push(PendingTarget {
                    tx_hash: *tx_hash,
                    tx_index: tx_index as u64,
                    commit_index,
                    exec_result,
                    exec_time: start.elapsed(),
                    gas_used,
                    pre_execution_nonce,
                    from: tx.inner.inner.signer(),
                    to: tx.inner.inner.to(),
                    effective_gas_price: tx.inner.effective_gas_price.unwrap_or(0),
                    fixture,
                });
            }
        }
        Ok(())
    }
    .await;

    // Finish the block even when it aborted midway: targets that already ran
    // still have a receipt worth reporting.
    let mut entries = Vec::with_capacity(targets.len());
    match block_executor.finish() {
        Ok((evm, block_result)) => {
            let (db, _) = evm.finish();
            db.merge_transitions(BundleRetention::Reverts);
            let receipts = block_result.receipts;
            // Receipts are pushed one per committed transaction; index from the
            // end so any receipt produced before the first transaction (now or
            // later) cannot shift the mapping.
            let offset = receipts.len().saturating_sub(committed);
            let block_hash = block.hash();
            // A fixture that could not be written stays on the target's own
            // result line below: the target did replay, so its receipt and its
            // verification verdict are still what the run was asked for. The
            // failed dump fails the run through the tally, not by replacing the
            // result with an error entry.
            //
            // Finalize+write runs only here, after finish succeeded: a
            // commit-time rejection never reaches pending, and a finish failure
            // drops ready drafts unwritten (see the Err arm).
            for target in pending {
                let Some(envelope) = receipts.get(offset + target.commit_index) else {
                    entries.push(failure(
                        target.tx_hash,
                        BatchErrorKind::Execution,
                        format!("No receipt produced for transaction index {}", target.tx_index),
                    ));
                    continue;
                };
                let contract_address = (target.to.is_none() && envelope.is_success())
                    .then(|| target.from.create(target.pre_execution_nonce));
                // Block-global log index: cumulative log count of all committed
                // receipts that precede this target in the block.
                let first_log_index: u64 = receipts[offset..offset + target.commit_index]
                    .iter()
                    .map(|r| r.logs().len() as u64)
                    .sum();
                let receipt = op_receipt_to_tx_receipt(
                    envelope,
                    number,
                    timestamp,
                    target.from,
                    target.to,
                    contract_address,
                    target.effective_gas_price,
                    target.gas_used,
                    Some(target.tx_hash),
                    Some(block_hash),
                    target.tx_index,
                    first_log_index,
                );
                let verification = if verify_receipt {
                    match onchain_receipts.get(&target.tx_hash) {
                        Some(Ok(onchain)) => {
                            Some(verify::compare(onchain, &ReceiptFacts::from_receipt(&receipt)))
                        }
                        // Without an on-chain receipt there is nothing to compare
                        // against: report the target as unverified.
                        unverified => {
                            let message = match unverified {
                                Some(Err(message)) => message.clone(),
                                _ => "No on-chain receipt was fetched for this transaction"
                                    .to_string(),
                            };
                            entries.push(failure(target.tx_hash, BatchErrorKind::Rpc, message));
                            continue;
                        }
                    }
                } else {
                    None
                };
                let fixture = target.fixture.map(materialize_deferred_fixture);
                entries.push(BatchEntry::Executed(Box::new(ExecutedTx {
                    tx_hash: target.tx_hash,
                    block_number: number,
                    tx_index: target.tx_index,
                    exec_result: target.exec_result,
                    contract_address,
                    exec_time: target.exec_time,
                    receipt,
                    verification,
                    fixture,
                })));
            }
        }
        // The block itself failed to finish, so no target of it has a receipt
        // and no deferred fixture is written or replaced.
        Err(e) => {
            let error = ReplayError::BlockExecutionError(e);
            let kind = classify(&error);
            let message = error.to_string();
            for target in pending {
                entries.push(failure(target.tx_hash, kind, message.clone()));
            }
        }
    }

    // Any target that produced no entry either sat behind the abort or is not
    // part of this block at all. They are appended in block transaction-index
    // order, keeping the run's ascending (block, index) order; a target the
    // block does not contain has no index and keeps its input position.
    let reported: HashSet<B256> = entries.iter().map(BatchEntry::tx_hash).collect();
    let block_txs: HashSet<B256> = tx_hashes.iter().copied().collect();
    let unreported = tx_hashes
        .iter()
        .filter(|hash| target_set.contains(*hash))
        .chain(targets.iter().filter(|hash| !block_txs.contains(*hash)))
        .filter(|hash| !reported.contains(*hash));

    match &loop_result {
        Ok(()) => {
            let message = format!("Transaction is not part of block {number}");
            for tx_hash in unreported {
                entries.push(failure(*tx_hash, BatchErrorKind::NotFound, message.clone()));
            }
        }
        Err(e) => {
            warn!(block = number, error = %e, "Aborted block replay; skipping its remaining targets");
            let aborting = aborting_tx_hash(e);
            for tx_hash in unreported {
                if aborting == Some(*tx_hash) {
                    // The abort is this target's own answer.
                    entries.push(failure(*tx_hash, classify(e), e.to_string()));
                } else {
                    // The abort belongs to another transaction of the block, so
                    // nothing was established about this target: it went
                    // unanswered rather than being unknown or invalid.
                    entries.push(failure(
                        *tx_hash,
                        swept_kind(e),
                        format!("Block replay aborted before this transaction: {e}"),
                    ));
                }
            }
        }
    }

    entries
}

/// Inputs for [`prepare_target_fixture`], grouped so the dump path stays a single
/// call site without a long positional argument list.
struct DumpFixtureArgs<'a> {
    accessed_block_hash_count: usize,
    exec_result: &'a ExecutionResult<MegaHaltReason>,
    evm_state: &'a mega_evm::revm::state::EvmState,
    chain_id: u64,
    executed_spec: MegaSpecId,
    block: &'a Block<Transaction>,
    target_tx: &'a Transaction,
    mega_env: MegaEnv,
    onchain: Option<&'a std::result::Result<ReceiptFacts, String>>,
    dir: &'a Path,
    overwrite: bool,
}

/// Prepare a fixture for one successfully executed target against pre-commit state.
///
/// Expected skips (missing receipt, fidelity mismatch, BLOCKHASH, unsupported
/// transaction shapes) become a final [`FixtureReport`] and never fail the run.
/// Database and other construction failures become a fixture error (execution-class).
/// A successfully built draft is carried as [`DeferredFixture::Ready`] and only
/// written by [`materialize_deferred_fixture`] after `finish()` succeeds.
///
/// `db` must reflect the pre-target-commit state (preceding txs committed, target
/// not yet), matching the single-transaction dump.
fn prepare_target_fixture<DB>(db: &DB, args: DumpFixtureArgs<'_>) -> DeferredFixture
where
    DB: DatabaseRef,
    DB::Error: core::fmt::Display,
{
    let DumpFixtureArgs {
        accessed_block_hash_count,
        exec_result,
        evm_state,
        chain_id,
        executed_spec,
        block,
        target_tx,
        mega_env,
        onchain,
        dir,
        overwrite,
    } = args;

    // Fidelity gate needs the on-chain receipt; without it the dump is skipped,
    // not failed — the envelope may simply lack receipts (expected in sweeps
    // over captures that never fetched them).
    let facts = match onchain {
        Some(Ok(facts)) => facts,
        Some(Err(message)) => {
            return DeferredFixture::Report(FixtureReport::skipped(format!(
                "fidelity-gate-unavailable: {message}"
            )));
        }
        None => {
            return DeferredFixture::Report(FixtureReport::skipped(
                "fidelity-gate-unavailable: no on-chain receipt was fetched for this transaction",
            ));
        }
    };

    if accessed_block_hash_count > 0 {
        return DeferredFixture::Report(FixtureReport::skipped(format!(
            "transaction reads block hashes (BLOCKHASH): {accessed_block_hash_count} block \
             hash(es) were accessed and the fixture cannot faithfully reproduce them"
        )));
    }

    let anchor = fixture::anchor_from_receipt_facts(facts);
    if let Err(reason) = fixture::check_fidelity(exec_result, &anchor, chain_id) {
        return DeferredFixture::Report(FixtureReport::skipped(format!(
            "fidelity gate failed: {reason}"
        )));
    }

    let draft = match fixture::build_draft(
        db,
        evm_state,
        chain_id,
        executed_spec,
        block,
        target_tx,
        fixture::FixtureInputs { mega_env, result: exec_result, anchor },
    ) {
        Ok(draft) => draft,
        Err(e) => return DeferredFixture::Report(fixture_report_from_build_err(e)),
    };

    let tx_hash = target_tx.inner.inner.tx_hash();
    let path = dir.join(format!("{tx_hash:#x}.json"));
    // Refuse overwrite without the flag before carrying a ready draft, so the
    // harvest path never has to re-check and a finish failure cannot be confused
    // with an overwrite refusal.
    if path.exists() && !overwrite {
        return DeferredFixture::Report(FixtureReport::error(format!(
            "fixture already exists at {} (pass --overwrite to replace)",
            path.display()
        )));
    }

    DeferredFixture::Ready { draft: Box::new(draft), path }
}

/// Finalize a deferred fixture after the block `finish()` succeeded.
///
/// Ready drafts are self-validated and written here. Pre-decided reports pass
/// through unchanged. On finish failure the caller drops the deferred value
/// without calling this, so no file is written or replaced.
fn materialize_deferred_fixture(deferred: DeferredFixture) -> FixtureReport {
    match deferred {
        DeferredFixture::Report(report) => report,
        DeferredFixture::Ready { draft, path } => {
            match fixture::finalize_and_write(*draft, &path) {
                Ok(()) => {
                    info!(path = %path.display(), "Wrote self-validating fixture");
                    FixtureReport::written(&path)
                }
                Err(e) => FixtureReport::error(format!("fixture write failed: {e}")),
            }
        }
    }
}

/// Classify a [`fixture::build_draft`] error as a skip (unsupported shape) or a
/// fixture construction error (database / other failures).
///
/// Unsupported shapes are expected in whole-block sweeps and must not fail the
/// run. Endpoint/DB failures during construction mean the requested artifact
/// could not be produced and fail the run as an execution-class fixture error.
fn fixture_report_from_build_err(err: ReplayError) -> FixtureReport {
    let message = err.to_string();
    if is_unsupported_fixture_shape(&message) {
        FixtureReport::skipped(message)
    } else {
        FixtureReport::error(format!("fixture construction failed: {message}"))
    }
}

/// Whether a `build_draft` error is an expected unsupported shape (skip) rather
/// than a construction failure.
fn is_unsupported_fixture_shape(message: &str) -> bool {
    message.contains("does not support deposit")
        || message.contains("does not support EIP-7702")
        || message.contains("has no fixture mapping")
        || message.contains("reports no gas price")
        // Fidelity is normally gated before `build_draft`; keep the classification
        // if a fidelity check inside the builder ever surfaces here.
        || message.contains("does not reproduce on-chain execution")
}

/// Fetch the on-chain receipt of every target of a block.
///
/// Each target maps either to the consensus facts its receipt reports, or to the
/// message explaining why it could not be verified (the endpoint failed the
/// call or pruned the receipt, or the receipt describes a different inclusion
/// than the block being replayed).
async fn fetch_target_receipts<P>(
    provider: &P,
    targets: &[B256],
    block_hash: B256,
) -> BTreeMap<B256, std::result::Result<ReceiptFacts, String>>
where
    P: Provider<op_alloy_network::Optimism>,
{
    let mut receipts = BTreeMap::new();
    for tx_hash in targets {
        let fetched = match verify::fetch_receipt(provider, *tx_hash).await {
            Ok(receipt) => match verify::check_inclusion(receipt.block_hash(), block_hash) {
                Ok(()) => Ok(ReceiptFacts::from_receipt(&receipt.inner)),
                Err(message) => Err(message),
            },
            // The reported entry already carries the `rpc` kind, so the error's
            // own "RPC error" prefix would only repeat it.
            Err(ReplayError::RpcError(message)) => Err(message),
            Err(e) => Err(e.to_string()),
        };
        if let Err(message) = &fetched {
            warn!(tx_hash = %tx_hash, %message, "Could not fetch the on-chain receipt");
        }
        receipts.insert(*tx_hash, fetched);
    }
    receipts
}

/// Fetch a block by number, using the same call shape as the single-transaction path.
async fn fetch_block<P>(provider: &P, number: u64) -> Result<Block<Transaction>>
where
    P: Provider<op_alloy_network::Optimism>,
{
    provider
        .get_block_by_number(number.into())
        .await
        .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
        .ok_or(ReplayError::BlockNotFound(number))
}

/// Map an error raised while replaying a block onto the kind reported for the
/// target the error is about.
fn classify(err: &ReplayError) -> BatchErrorKind {
    match err {
        ReplayError::TransactionNotFound(_) => BatchErrorKind::NotFound,
        ReplayError::RpcError(_) | ReplayError::RpcTransportError(_) => BatchErrorKind::Rpc,
        // A block error the EVM raised because a state read failed is that
        // read's failure: the same classification the run-level exit code uses.
        ReplayError::BlockExecutionError(_)
            if ExitCode::from_evme_error(err) == ExitCode::RpcFailure =>
        {
            BatchErrorKind::Rpc
        }
        _ => BatchErrorKind::Execution,
    }
}

/// The kind reported for a target swept up by an abort caused elsewhere.
///
/// The abort says nothing about this target: whatever class caused the block to
/// stop (unknown hash, RPC failure, executor/setup error on another
/// transaction), a non-aborting swept target is unanswered (`rpc`). Only the
/// transaction that caused the abort keeps its own classified kind.
fn swept_kind(_err: &ReplayError) -> BatchErrorKind {
    BatchErrorKind::Rpc
}

/// The transaction an aborting error is about, when it names one.
fn aborting_tx_hash(err: &ReplayError) -> Option<B256> {
    match err {
        ReplayError::TransactionNotFound(hash) => Some(*hash),
        ReplayError::BlockExecutionError(err) => block_error_tx_hash(err),
        _ => None,
    }
}

/// The transaction a block execution error names, when it carries one.
fn block_error_tx_hash(err: &BlockExecutionError) -> Option<B256> {
    if let Some(validation) = err.as_validation() {
        return match validation {
            BlockValidationError::InvalidTx { hash, .. } |
            BlockValidationError::EVM { hash, .. } => Some(*hash),
            _ => None,
        };
    }
    err.as_internal()?.as_evm().map(|(hash, _)| *hash)
}

/// Build a failure entry.
fn failure(tx_hash: B256, kind: BatchErrorKind, message: String) -> BatchEntry {
    BatchEntry::Failed(FailedTx { tx_hash, kind, message })
}

/// Report the same failure for every target of a block that never started.
fn fail_all(targets: &[B256], kind: BatchErrorKind, message: &str) -> Vec<BatchEntry> {
    targets.iter().map(|hash| failure(*hash, kind, message.to_string())).collect()
}

/// Write one entry to stdout: a compact NDJSON line, or the human-readable
/// summary used by the single-transaction path.
fn emit(entry: &BatchEntry, json: bool) {
    if json {
        let line = match entry {
            BatchEntry::Executed(tx) => {
                let mut summary =
                    ExecutionSummary::from_result(&tx.exec_result, tx.contract_address);
                summary.receipt =
                    Some(serde_json::to_value(&tx.receipt).expect("failed to serialize receipt"));
                summary.verification = tx.verification.as_ref().map(|verification| {
                    serde_json::to_value(verification).expect("failed to serialize verification")
                });
                serde_json::to_string(&BatchResultLine {
                    tx_hash: tx.tx_hash,
                    block_number: tx.block_number,
                    tx_index: tx.tx_index,
                    summary: &summary,
                    fixture: tx.fixture.as_ref(),
                })
            }
            BatchEntry::Failed(tx) => serde_json::to_string(&BatchErrorLine {
                tx_hash: tx.tx_hash,
                error: BatchErrorBody { kind: tx.kind.as_str(), message: &tx.message },
            }),
        };
        println!("{}", line.expect("failed to serialize output"));
        return;
    }

    match entry {
        BatchEntry::Executed(tx) => {
            println!();
            println!(
                "=== Transaction {} (block {}, index {}) ===",
                tx.tx_hash, tx.block_number, tx.tx_index
            );
            print_execution_summary(&tx.exec_result, tx.contract_address, tx.exec_time);
            print_receipt(&tx.receipt);
            if let Some(verification) = &tx.verification {
                println!();
                println!("{}", verification.verdict_line());
            }
            if let Some(fixture) = &tx.fixture {
                println!();
                println!("{}", fixture.human_line());
            }
        }
        BatchEntry::Failed(tx) => {
            println!();
            println!("=== Transaction {} ===", tx.tx_hash);
            println!("Error ({}): {}", tx.kind.as_str(), tx.message);
        }
    }
}

/// Parse the newline-separated transaction hash list behind `--tx-file`.
///
/// Blank lines and `#`-prefixed comment lines are ignored. Duplicates are
/// dropped, keeping the first occurrence.
pub(super) fn parse_tx_hash_list(contents: &str) -> Result<Vec<B256>> {
    let mut hashes = Vec::new();
    let mut seen = HashSet::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_number = index + 1;
        let hash = B256::from_str(line).map_err(|e| {
            ReplayError::InvalidInput(format!(
                "invalid transaction hash on line {line_number}: '{line}' ({e})"
            ))
        })?;
        if seen.insert(hash) {
            hashes.push(hash);
        } else {
            warn!(
                tx_hash = %hash,
                line = line_number,
                "Duplicate transaction hash in --tx-file; replaying it once",
            );
        }
    }

    Ok(hashes)
}

/// `clap` value parser for `--block`, accepting decimal or `0x`-prefixed hex.
pub(super) fn parse_block_number(value: &str) -> std::result::Result<u64, String> {
    let trimmed = value.trim();
    match trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16)
            .map_err(|e| format!("invalid hex block number '{value}': {e}")),
        None => trimmed.parse::<u64>().map_err(|e| format!("invalid block number '{value}': {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ExitCode;

    const HASH_A: &str = "0xde3d56dc739484166b8af1bea757bf7e3e9a4b9a0fb62d722703345570dfc1d6";
    const HASH_B: &str = "0x323ddc8e67dfc134284d78c65f3c1dc7ff45ba1db02eeaf62e211ae3253478ef";

    /// A verification verdict as a run would have reported it.
    fn verdict(matched: bool) -> VerificationOutcome {
        VerificationOutcome { matched, diff: None }
    }

    /// Build a tally from the outcomes a run would have reported: `failures`
    /// error entries, plus `replayed` verified result lines of which
    /// `mismatched` diverged from their receipt.
    fn tally(failures: &[BatchErrorKind], replayed: usize, mismatched: usize) -> BatchTally {
        let mut tally = BatchTally::default();
        for kind in failures {
            tally.record(&failure(B256::ZERO, *kind, String::new()));
        }
        for index in 0..replayed {
            tally.record_executed(Some(&verdict(index >= mismatched)), None);
        }
        tally
    }

    /// The exit code a batch run with these outcomes ends with.
    fn exit_code(failures: &[BatchErrorKind], replayed: usize, mismatched: usize) -> ExitCode {
        tally(failures, replayed, mismatched)
            .into_error()
            .map_or(ExitCode::Success, |err| ExitCode::from_evme_error(&err))
    }

    /// A clean run has nothing to report and exits 0.
    #[test]
    fn test_batch_tally_clean_run_has_no_error() {
        assert!(tally(&[], 3, 0).into_error().is_none());
        assert_eq!(exit_code(&[], 3, 0), ExitCode::Success);
    }

    /// Mixed failures are ranked by class: an execution failure outranks the
    /// rest, an RPC failure outranks a mismatch.
    #[test]
    fn test_batch_tally_failure_precedence() {
        use BatchErrorKind::{Execution, NotFound, Pending, Rpc};

        assert_eq!(exit_code(&[Execution, Rpc], 1, 1), ExitCode::ExecutionError);
        assert_eq!(exit_code(&[Rpc, Rpc], 1, 1), ExitCode::RpcFailure);
        assert_eq!(exit_code(&[], 2, 1), ExitCode::VerificationMismatch);
        // A definitive answer about a target is an execution-class failure.
        assert_eq!(exit_code(&[NotFound], 1, 0), ExitCode::ExecutionError);
        assert_eq!(exit_code(&[Pending], 1, 0), ExitCode::ExecutionError);
    }

    /// The aggregate error carries the counts by class, not a formatted string
    /// the exit mapping would have to parse.
    #[test]
    fn test_batch_tally_aggregate_carries_counts() {
        use BatchErrorKind::{Execution, NotFound, Rpc};

        let err = tally(&[Execution, NotFound, Rpc], 2, 1).into_error().expect("run failed");
        let ReplayError::BatchFailed(counts) = err else {
            panic!("infrastructure failures must aggregate: {err:?}");
        };
        assert_eq!(counts, BatchFailureCounts { execution: 2, rpc: 1, mismatched: 1, total: 5 });
        assert!(
            counts.to_string().contains("3 of 5 target transaction(s) failed"),
            "unexpected message: {counts}"
        );
    }

    /// A target whose fixture could not be written keeps its result line and
    /// its verdict, and still fails the run as an execution-class failure —
    /// including when that same target diverged from its on-chain receipt.
    #[test]
    fn test_batch_tally_counts_a_fixture_failure_and_its_mismatch() {
        let mut tally = BatchTally::default();
        tally.record_executed(Some(&verdict(false)), Some(&FixtureReport::error("disk full")));

        assert_eq!(tally.replayed, 1, "the target replayed");
        assert_eq!(tally.verified, 1, "the target was verified");
        assert_eq!(tally.counts.mismatched, 1, "its divergence is counted");
        assert_eq!(tally.counts.execution, 1, "its failed fixture is counted");

        let err = tally.into_error().expect("run failed");
        let ReplayError::BatchFailed(counts) = err else {
            panic!("a failed fixture must fail the run: {err:?}");
        };
        assert_eq!(counts, BatchFailureCounts { execution: 1, rpc: 0, mismatched: 1, total: 1 });
        assert_eq!(ExitCode::from_batch_failures(&counts), ExitCode::ExecutionError);
    }

    /// A written or skipped fixture is not a failure.
    #[test]
    fn test_batch_tally_ignores_written_and_skipped_fixtures() {
        let mut tally = BatchTally::default();
        tally.record_executed(None, Some(&FixtureReport::written(Path::new("/tmp/tx.json"))));
        tally.record_executed(None, Some(&FixtureReport::skipped("fidelity gate failed")));

        assert_eq!(tally.counts.execution, 0);
        assert!(tally.into_error().is_none(), "fixture skips never fail the run");
    }

    /// Every non-aborting swept target is unanswered (`rpc`), even when the
    /// abort itself is an execution-class failure of another transaction.
    #[test]
    fn test_swept_kind_always_rpc_regardless_of_abort_class() {
        // Unknown hash: already unanswered for the cause, and for swept peers.
        assert_eq!(swept_kind(&ReplayError::TransactionNotFound(B256::ZERO)), BatchErrorKind::Rpc);
        // Transport/RPC failure.
        assert_eq!(swept_kind(&ReplayError::RpcError("endpoint down".into())), BatchErrorKind::Rpc);
        // Execution-class aborts (other, setup, internal) must not blame swept targets.
        assert_eq!(
            swept_kind(&ReplayError::Other("executor setup failed".into())),
            BatchErrorKind::Rpc
        );
        assert_eq!(
            swept_kind(&ReplayError::InvalidInput("bad hardfork schedule".into())),
            BatchErrorKind::Rpc
        );
        // classify itself still distinguishes execution for the aborting target.
        assert_eq!(
            classify(&ReplayError::Other("executor setup failed".into())),
            BatchErrorKind::Execution
        );
    }

    /// A run whose only finding is divergence fails as the mismatch it is.
    #[test]
    fn test_batch_tally_mismatch_only_reports_the_verification_error() {
        let err = tally(&[], 4, 2).into_error().expect("run failed");
        assert!(
            matches!(err, ReplayError::VerificationMismatch { mismatched: 2, total: 4 }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_parse_tx_hash_list_skips_blanks_and_comments() {
        let contents =
            format!("# leading comment\n\n{HASH_A}\n   \n  # indented comment\n\t{HASH_B}  \n\n");

        let hashes = parse_tx_hash_list(&contents).expect("should parse");

        assert_eq!(hashes, vec![B256::from_str(HASH_A).unwrap(), B256::from_str(HASH_B).unwrap()]);
    }

    #[test]
    fn test_parse_tx_hash_list_deduplicates_preserving_order() {
        let contents = format!("{HASH_B}\n{HASH_A}\n{HASH_B}\n");

        let hashes = parse_tx_hash_list(&contents).expect("should parse");

        assert_eq!(hashes, vec![B256::from_str(HASH_B).unwrap(), B256::from_str(HASH_A).unwrap()]);
    }

    #[test]
    fn test_parse_tx_hash_list_reports_offending_line_number() {
        let contents = format!("# comment\n\n{HASH_A}\nnot-a-hash\n");

        let err = parse_tx_hash_list(&contents).expect_err("should reject the invalid hash");

        let message = err.to_string();
        assert!(message.contains("line 4"), "error should name the line, got: {message}");
        assert!(message.contains("not-a-hash"), "error should quote the line, got: {message}");
    }

    #[test]
    fn test_parse_tx_hash_list_accepts_empty_input() {
        let hashes = parse_tx_hash_list("# only a comment\n\n").expect("should parse");
        assert!(hashes.is_empty());
    }

    #[test]
    fn test_parse_block_number_decimal() {
        assert_eq!(parse_block_number("22945844"), Ok(22_945_844));
        assert_eq!(parse_block_number("  22945844 "), Ok(22_945_844));
        assert_eq!(parse_block_number("0"), Ok(0));
    }

    #[test]
    fn test_parse_block_number_hex() {
        assert_eq!(parse_block_number("0x15e2034"), Ok(22_945_844));
        assert_eq!(parse_block_number("0X15E2034"), Ok(22_945_844));
    }

    #[test]
    fn test_parse_block_number_rejects_garbage() {
        assert!(parse_block_number("").is_err());
        assert!(parse_block_number("0x").is_err());
        assert!(parse_block_number("0xzz").is_err());
        assert!(parse_block_number("-1").is_err());
        assert!(parse_block_number("12.5").is_err());
    }

    /// Unsupported shapes remain skips; database/construction failures become
    /// fixture errors so the run exits non-zero.
    #[test]
    fn test_fixture_build_err_classifies_skips_vs_construction_errors() {
        let deposit = fixture_report_from_build_err(ReplayError::Other(
            "--dump-fixture does not support deposit transactions".into(),
        ));
        assert!(deposit.skipped.is_some(), "deposit is a skip: {deposit:?}");
        assert!(deposit.error.is_none());

        let eip7702 = fixture_report_from_build_err(ReplayError::Other(
            "--dump-fixture does not support EIP-7702 (set-code) transactions: the \
             fixture builder does not serialize the authorization list"
                .into(),
        ));
        assert!(eip7702.skipped.is_some(), "EIP-7702 is a skip: {eip7702:?}");

        let unknown_spec = fixture_report_from_build_err(ReplayError::Other(
            "--dump-fixture: spec Rex99 has no fixture mapping".into(),
        ));
        assert!(unknown_spec.skipped.is_some(), "unknown spec is a skip: {unknown_spec:?}");

        let pre_state = fixture_report_from_build_err(ReplayError::Other(
            "pre-state read for 0x00000000000000000000000000000000000000aa: database unavailable"
                .into(),
        ));
        assert!(
            pre_state.error.as_ref().is_some_and(|m| m.contains("construction failed")),
            "DB pre-state failure is a fixture error: {pre_state:?}"
        );
        assert!(pre_state.skipped.is_none());

        let code_fetch = fixture_report_from_build_err(ReplayError::Other(
            "code fetch for 0x1111: endpoint timeout".into(),
        ));
        assert!(
            code_fetch.error.as_ref().is_some_and(|m| m.contains("construction failed")),
            "code fetch failure is a fixture error: {code_fetch:?}"
        );
        assert!(code_fetch.skipped.is_none());
    }

    /// A pre-decided fixture report is never rewritten by materialization, so a
    /// finish failure that drops a Ready draft (without calling materialize)
    /// cannot leave a file and a Report never touches the filesystem.
    #[test]
    fn test_materialize_deferred_fixture_passes_reports_through() {
        let skipped = materialize_deferred_fixture(DeferredFixture::Report(
            FixtureReport::skipped("fidelity-gate-unavailable: no receipt"),
        ));
        assert_eq!(skipped.skipped.as_deref(), Some("fidelity-gate-unavailable: no receipt"));
        assert!(skipped.path.is_none());
        assert!(skipped.error.is_none());

        let err = materialize_deferred_fixture(DeferredFixture::Report(FixtureReport::error(
            "fixture construction failed: code fetch failed",
        )));
        assert!(err.error.as_ref().is_some_and(|m| m.contains("construction failed")));
        assert!(err.path.is_none());
    }
}
