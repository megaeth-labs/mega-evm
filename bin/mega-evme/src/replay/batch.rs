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
//! Every RPC call issued by a plain batch replay has the same shape as the
//! single-transaction path (`eth_getTransactionByHash`, `eth_getBlockByNumber`
//! with hash-only bodies, and the state reads behind [`EvmeState::new_forked`]),
//! so an offline envelope captured by single-transaction replays serves batch
//! runs without a miss. `--verify-receipt` and `--dump-fixture-dir` are the
//! exception: both fetch `eth_getTransactionReceipt` for *every* target of a
//! block, including the non-targets a single-transaction capture never asked
//! about. Offline, those come back as `rpc` entries and the run exits `3`.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};

use alloy_consensus::{transaction::Recovered, BlockHeader};
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Block;
use mega_evm::{
    alloy_evm::EvmEnv,
    revm::{context::result::ExecutionResult, inspector::NoOpInspector, DatabaseRef},
    BlockLimits, MegaBlockExecutionCtx, MegaHaltReason, MegaHardforks, MegaSpecId, MegaTxEnvelope,
};
use op_alloy_rpc_types::Transaction;
use serde::Serialize;
use state_test::types::MegaEnv;
use tracing::{debug, info, warn};

use crate::{
    common::{
        print_execution_summary, print_receipt, BatchExitFloor, BatchFailureCounts,
        EvmeExternalEnvs, ExecutionSummary, ExitCode, OpTxReceipt,
    },
    replay::get_hardfork_config,
    ChainArgs,
};

use super::{
    cmd::retrieve_block_env,
    coherence::{self, Incoherence, MembershipClaim, TargetPlacement},
    fixture, kernel,
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
/// (fidelity mismatch, BLOCKHASH, unsupported shape), or could not be written.
/// A write failure — and an unanswered receipt question for the fidelity gate —
/// is reported here rather than replacing the target's result, so a target that
/// did replay keeps its result — including its receipt verification verdict —
/// and still fails the run.
#[derive(Debug, Clone, Serialize)]
struct FixtureReport {
    /// Absolute or as-written path of a successfully written fixture.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// Why the fixture was not written for this target.
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<String>,
    /// Why writing the fixture failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Batch tally class for [`Self::error`]. Construction and write failures
    /// are execution-class; an unanswered on-chain receipt (transport, pruned,
    /// divergent inclusion, missing from the offline envelope) is rpc-class; a
    /// draft discarded because the block aborted inherits the abort's class so
    /// a transient RPC abort does not become exit 1.
    /// Not serialized: the wire shape stays `path` / `skipped` / `error`.
    #[serde(skip)]
    error_kind: BatchErrorKind,
}

impl FixtureReport {
    fn written(path: &Path) -> Self {
        Self {
            path: Some(path.display().to_string()),
            skipped: None,
            error: None,
            error_kind: BatchErrorKind::Execution,
        }
    }

    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            path: None,
            skipped: Some(reason.into()),
            error: None,
            error_kind: BatchErrorKind::Execution,
        }
    }

    /// Construction or write failure of this target's fixture (execution-class).
    fn error(message: impl Into<String>) -> Self {
        Self {
            path: None,
            skipped: None,
            error: Some(message.into()),
            error_kind: BatchErrorKind::Execution,
        }
    }

    /// Fidelity gate could not run because the on-chain receipt question went
    /// unanswered (transport, null, reorg, or offline envelope missing it).
    ///
    /// Distinct from a genuine skip (BLOCKHASH, unsupported shape, fidelity
    /// mismatch): the dump was requested and the receipt call failed, so the
    /// run exits non-zero as rpc-class.
    fn rpc_error(message: impl Into<String>) -> Self {
        Self {
            path: None,
            skipped: None,
            error: Some(message.into()),
            error_kind: BatchErrorKind::Rpc,
        }
    }

    /// Fixture discarded because the block aborted after the draft was built.
    ///
    /// The target keeps its execution result; only the fixture field fails, and
    /// the failure class matches the abort so the run exit reflects the cause.
    fn abort_error(message: impl Into<String>, kind: BatchErrorKind) -> Self {
        Self { path: None, skipped: None, error: Some(message.into()), error_kind: kind }
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
///
/// Per-target counters ([`Self::counts`], [`Self::reported`]) stay strictly
/// about emitted target entries. A non-target abort's class is carried only as
/// [`Self::exit_floor`] so the human "N of M" totals stay truthful while the
/// run exit still reflects the root cause.
#[derive(Debug, Default)]
struct BatchTally {
    /// Targets the run reported on, one per emitted entry.
    reported: usize,
    /// Targets that produced an execution result.
    replayed: usize,
    /// Targets compared against an on-chain receipt.
    verified: usize,
    /// Failed and mismatched targets, by class (reported targets only).
    counts: BatchFailureCounts,
    /// Run-level exit floor from a non-target abort not carried by any target.
    exit_floor: BatchExitFloor,
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
    /// whether or not its fixture could be written. An unanswered receipt
    /// question (verification unavailable, or dump-dir fidelity gate starved of
    /// a receipt) is rpc-class and does not count as verified.
    ///
    /// When both `--verify-receipt` and `--dump-fixture-dir` fail on the same
    /// unanswered receipt, both result fields stay on the line but the shared
    /// rpc failure is counted once.
    fn record_executed(
        &mut self,
        verification: Option<&VerificationOutcome>,
        fixture: Option<&FixtureReport>,
    ) {
        self.reported += 1;
        self.replayed += 1;
        let mut receipt_rpc_counted = false;
        if let Some(verification) = verification {
            if verification.is_unavailable() {
                // Compared path never ran: the receipt question went unanswered.
                self.counts.rpc += 1;
                receipt_rpc_counted = true;
            } else {
                self.verified += 1;
                if !verification.matched {
                    self.counts.mismatched += 1;
                }
            }
        }
        // A fixture the run was asked to write and could not is a failure of
        // that target, even though its replay produced a result. Construction
        // and write failures are execution-class; an unanswered receipt for the
        // fidelity gate is rpc-class; an abort-inherited discard uses the
        // abort's class (see [`FixtureReport::abort_error`]).
        if let Some(fixture) = fixture.filter(|f| f.is_error()) {
            match fixture.error_kind {
                BatchErrorKind::Rpc => {
                    // Same missing receipt as verification.error: one target,
                    // one rpc count. Independent fixture rpc failures (none
                    // today share the gate without verification) still count.
                    if !receipt_rpc_counted {
                        self.counts.rpc += 1;
                    }
                }
                BatchErrorKind::NotFound | BatchErrorKind::Pending | BatchErrorKind::Execution => {
                    self.counts.execution += 1
                }
            }
        }
    }

    /// Record a mid-block abort whose root-cause class is not already carried by
    /// a per-target failure entry.
    ///
    /// Swept targets always stay `rpc` ("unanswered"). When the aborting
    /// transaction is not itself a reported target, that class would otherwise
    /// be lost and a deterministic executor abort would exit 3. The abort is
    /// recorded as an exit floor only: it does not emit an NDJSON line, does
    /// not increment `reported`, and does not inflate the per-target counters.
    fn record_uncounted_abort(&mut self, kind: BatchErrorKind) {
        let floor = match kind {
            BatchErrorKind::Rpc => BatchExitFloor::Rpc,
            BatchErrorKind::NotFound | BatchErrorKind::Pending | BatchErrorKind::Execution => {
                BatchExitFloor::Execution
            }
        };
        // Multiple blocks can each contribute a floor; keep the more severe.
        self.exit_floor = match (self.exit_floor, floor) {
            (BatchExitFloor::Execution, _) | (_, BatchExitFloor::Execution) => {
                BatchExitFloor::Execution
            }
            (BatchExitFloor::Rpc, _) | (_, BatchExitFloor::Rpc) => BatchExitFloor::Rpc,
            (BatchExitFloor::None, BatchExitFloor::None) => BatchExitFloor::None,
        };
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
    ///
    /// A non-target abort floor alone also fails the run (with empty target
    /// failure counters) so the exit still reflects the root cause.
    fn into_error(self) -> Option<ReplayError> {
        if self.failed() > 0 || self.exit_floor != BatchExitFloor::None {
            return Some(ReplayError::BatchFailed(BatchFailureCounts {
                total: self.reported,
                exit_floor: self.exit_floor,
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

/// One target of a [`BlockJob`], carrying the inclusion hash it resolved with.
///
/// Per-target inclusion (rather than a job-level first-seen anchor) keeps
/// outcomes order-independent: two same-height targets that report different
/// hashes each validate against the fetched block on their own.
struct JobTarget {
    hash: B256,
    /// Inclusion block hash from `eth_getTransactionByHash` (`--tx-file`).
    /// `None` for `--block` targets, which come from the body itself.
    inclusion_hash: Option<B256>,
}

/// One block's worth of work.
struct BlockJob {
    /// Number of the block holding the targets.
    number: u64,
    /// Block body, present when planning already fetched it (`--block`).
    block: Option<Block<Transaction>>,
    /// Targets whose results are reported for this block.
    targets: Vec<JobTarget>,
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
    /// `overwrite` is enforced at materialization via noclobber persist — the
    /// prep-time existence check is only a fast path.
    Ready { draft: Box<fixture::FixtureDraft>, path: PathBuf, overwrite: bool },
}

/// Fixture dump destination and the environment every fixture of a block is
/// built against, present iff `--dump-fixture-dir` was given.
struct FixtureDumpEnv<'a> {
    /// Directory the fixture files are written into.
    dir: &'a Path,
    /// Replace an existing file at the target path.
    overwrite: bool,
    /// `MegaETH` external environment recorded into every fixture of this block.
    mega_env: MegaEnv,
}

/// The batch driver's participation in the kernel's block run.
///
/// A fixture draft has to be built while the database still reflects the state
/// the target started from, which is only true between its execution and its
/// commit. The draft is carried through the block's `finish()` and published by
/// the driver afterwards, so a block that never finishes leaves no file behind.
///
/// The other lifecycle points are the ones a plain replay does not need: a batch
/// target executes exactly as it was mined, and nothing inspects it.
struct FixtureDraftHook<'a> {
    /// Where and against what to dump, or `None` when no dump was requested.
    dump: Option<FixtureDumpEnv<'a>>,
    /// Chain the fixture declares.
    chain_id: u64,
    /// Spec the target actually executed under.
    executed_spec: MegaSpecId,
    /// Block the targets belong to.
    block: &'a Block<Transaction>,
    /// On-chain receipts prefetched for this block's targets, keyed by hash.
    onchain_receipts: &'a BTreeMap<B256, std::result::Result<ReceiptFacts, String>>,
}

impl kernel::TargetLifecycle for FixtureDraftHook<'_> {
    /// Nothing observes a batch replay step by step: the run reports receipts,
    /// not traces.
    type Inspector = NoOpInspector;
    const INSPECT: bool = false;
    /// A batch target is replayed as the chain mined it — overrides belong to
    /// the single-transaction path, which refuses them together with a dump.
    type Tx<'tx> = Recovered<&'tx MegaTxEnvelope>;
    type Draft = Option<DeferredFixture>;

    /// Replay the target exactly as the block body served it.
    fn before_target<'tx>(
        &mut self,
        tx: &'tx Transaction,
        _inspector: &mut NoOpInspector,
    ) -> Result<Self::Tx<'tx>> {
        Ok(tx.as_recovered())
    }

    /// Prepare this target's fixture against the pre-commit state.
    ///
    /// Every way a dump can fail for one target is folded into its own report
    /// and the run keeps going: a batch reports per target, so one target's
    /// unwritable fixture must not stop the block. The kernel's failure channel
    /// is therefore unused here.
    fn on_target_executed<DB>(
        &mut self,
        target: kernel::TargetExecution<'_, DB>,
        _inspector: &NoOpInspector,
    ) -> Result<Self::Draft>
    where
        DB: DatabaseRef,
        DB::Error: core::fmt::Display,
    {
        let Some(dump) = self.dump.as_ref() else { return Ok(None) };
        Ok(Some(prepare_target_fixture(
            target.db,
            DumpFixtureArgs {
                accessed_block_hash_count: target.accessed_block_hash_count,
                exec_result: &target.result_and_state.result,
                evm_state: &target.result_and_state.state,
                chain_id: self.chain_id,
                executed_spec: self.executed_spec,
                block: self.block,
                target_tx: target.tx,
                mega_env: dump.mega_env.clone(),
                onchain: self.onchain_receipts.get(&target.tx_hash),
                dir: dump.dir,
                overwrite: dump.overwrite,
            },
        )))
    }
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
    P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug + 'static,
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
            if targets.is_empty() {
                // Nothing failed, so this is a clean exit — but a silent one is
                // indistinguishable from a run that produced no output for a bad
                // reason, so say why stdout is empty. No job is queued: with no
                // targets to report, forking the parent state would buy nothing.
                eprintln!("Block {number} contains no transactions; nothing to replay");
                vec![]
            } else {
                vec![BlockJob {
                    number: *number,
                    block: Some(block),
                    // Whole-block mode takes its targets from the body, so there
                    // is no separate inclusion claim to reconcile later.
                    targets: targets
                        .into_iter()
                        .map(|hash| JobTarget { hash, inclusion_hash: None })
                        .collect(),
                }]
            }
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
        let outcome = replay_block(provider, chain_id, job, external_envs.clone(), &report).await;
        for entry in outcome.entries {
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
        // Root-cause class of a non-target abort is not on any per-target line.
        if let Some(kind) = outcome.uncounted_abort {
            tally.record_uncounted_abort(kind);
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
///
/// Grouping is by block number only. Each target keeps the inclusion hash its
/// own lookup reported; agreement with the fetched block is checked later in
/// [`replay_block`], so two same-height targets that disagree with each other
/// still get independent outcomes instead of a first-seen race.
async fn resolve_targets<P>(provider: &P, hashes: &[B256]) -> (Vec<BlockJob>, Vec<FailedTx>)
where
    P: Provider<op_alloy_network::Optimism>,
{
    let mut grouped: BTreeMap<u64, Vec<JobTarget>> = BTreeMap::new();
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
            // Authenticate before reading anything out of the answer: a pending
            // verdict leaves planning immediately and would never reach the
            // authenticated lookup inside the execution kernel, so an endpoint
            // serving another (pending) transaction under this hash could
            // otherwise stamp the target as definitively pending — and capture
            // mode would persist that answer for offline reuse.
            //
            // Every (block_number, block_hash) shape the endpoint can return is
            // then classified by the shared judgment, so a contradictory row
            // cannot fall through into the pending arm. An unanchored or
            // contradictory row leaves the target unanswered (`rpc`); a
            // genuinely pending one is a definitive answer about it, which
            // batch mode reports as its own class.
            Ok(Some(tx)) => match verify::authenticate_transaction(&tx, *hash) {
                Err(message) => {
                    failures.push(FailedTx { tx_hash: *hash, kind: BatchErrorKind::Rpc, message })
                }
                Ok(()) => match coherence::classify_placement(tx.block_number, tx.block_hash) {
                    Ok(TargetPlacement::Mined { number, inclusion_hash }) => {
                        grouped
                            .entry(number)
                            .or_default()
                            .push(JobTarget { hash: *hash, inclusion_hash: Some(inclusion_hash) });
                    }
                    Ok(TargetPlacement::Pending) => failures.push(FailedTx {
                        tx_hash: *hash,
                        kind: BatchErrorKind::Pending,
                        message: "Transaction is pending (no block number)".to_string(),
                    }),
                    Err(incoherence) => failures.push(incoherent_endpoint(*hash, &incoherence)),
                },
            },
        }
    }

    let jobs = grouped
        .into_iter()
        .map(|(number, targets)| BlockJob { number, block: None, targets })
        .collect();
    (jobs, failures)
}

/// Outcome of replaying one block's targets.
struct BlockReplayOutcome {
    /// One entry per target of the job (executed or failed).
    entries: Vec<BatchEntry>,
    /// Root-cause class of a mid-block abort that no reported entry carries.
    ///
    /// Present when the aborting transaction is not a target: swept targets stay
    /// `rpc`, and this class is tallied so the run exit reflects the abort.
    uncounted_abort: Option<BatchErrorKind>,
}

impl BlockReplayOutcome {
    /// Order entries into documented stream order before returning.
    ///
    /// Pre-execution inclusion/membership failures are collected before the
    /// execute loop, while canonical results are appended after `finish()`.
    /// Without a final reorder, a later same-block target's inclusion failure
    /// would precede an earlier target's execution result.
    fn ordered(
        entries: Vec<BatchEntry>,
        job_targets: &[JobTarget],
        block_tx_order: Option<&[B256]>,
        uncounted_abort: Option<BatchErrorKind>,
    ) -> Self {
        Self { entries: order_block_entries(entries, job_targets, block_tx_order), uncounted_abort }
    }
}

/// Order a block's entries: targets present in the body by ascending transaction
/// index, then targets the block cannot place (inclusion/membership failures)
/// last, in job input order.
fn order_block_entries(
    entries: Vec<BatchEntry>,
    job_targets: &[JobTarget],
    block_tx_order: Option<&[B256]>,
) -> Vec<BatchEntry> {
    if entries.len() <= 1 {
        return entries;
    }
    let mut by_hash: HashMap<B256, BatchEntry> = HashMap::with_capacity(entries.len());
    for entry in entries {
        by_hash.insert(entry.tx_hash(), entry);
    }
    let mut ordered = Vec::with_capacity(by_hash.len());
    if let Some(block_txs) = block_tx_order {
        for hash in block_txs {
            if let Some(entry) = by_hash.remove(hash) {
                ordered.push(entry);
            }
        }
    }
    // Residual targets (absent from the body, or no body order available) keep
    // the job's input order — the documented absent-last placement.
    for target in job_targets {
        if let Some(entry) = by_hash.remove(&target.hash) {
            ordered.push(entry);
        }
    }
    // Defensive: anything not listed on the job (should not happen).
    ordered.extend(by_hash.into_values());
    ordered
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
) -> BlockReplayOutcome
where
    P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug + 'static,
{
    let BlockJob { number, block, targets: job_targets } = job;
    let verify_receipt = report.verify_receipt;
    let dump_dir = report.dump_fixture_dir.as_deref();
    let overwrite = report.overwrite;
    let target_hashes = || job_targets.iter().map(|t| t.hash);

    // Distinct from `--block 0` (invalid request, exit 1): an endpoint that
    // resolves a hash into block 0 is contradictory endpoint data — the
    // same unanswered class as unanchored / contradictory metadata.
    if let Err(incoherence) = coherence::require_forkable_block(number) {
        return BlockReplayOutcome::ordered(
            fail_all(target_hashes(), BatchErrorKind::Rpc, &incoherence.to_string()),
            &job_targets,
            None,
            None,
        );
    }

    let block = match block {
        Some(block) => block,
        None => match fetch_block(provider, number).await {
            Ok(block) => block,
            Err(e) => {
                return BlockReplayOutcome::ordered(
                    fail_all(target_hashes(), BatchErrorKind::Rpc, &e.to_string()),
                    &job_targets,
                    None,
                    None,
                );
            }
        },
    };
    // Body order for the documented ascending `(block, tx_index)` stream.
    let block_tx_order: Vec<B256> = block.transactions.hashes().collect();

    // Per-target inclusion and membership guards. `--tx-file` resolved each
    // target through `eth_getTransactionByHash`, which reported the block it
    // belongs to. Agreement is checked against the fetched body, not against
    // a first-seen peer, so two same-height targets that report different
    // hashes get independent outcomes. A target whose reported hash matches
    // the body but is missing from it is an endpoint self-contradiction (`rpc`),
    // not a definitive "unknown hash".
    //
    // When none of the job's targets appear in the body, every target already
    // has its definitive answer here — skip parent fetch, state forking, and
    // the execute loop entirely. Otherwise `last_target_index` would be `None`
    // and the foreign block would be walked for nothing.
    //
    // Pre-execution failures are buffered into `entries` and reordered with
    // executed results at return time so a later-index inclusion failure cannot
    // precede an earlier target's result line.
    let fetched = block.hash();
    let body_txs: HashSet<B256> = block_tx_order.iter().copied().collect();
    let mut entries = Vec::with_capacity(job_targets.len());
    let mut active: Vec<B256> = Vec::new();
    for target in &job_targets {
        // A target carrying an inclusion claim must anchor to the fetched block
        // before its membership is worth asking about. A target queued without
        // one (`--block` takes its targets from the body) only has to still be
        // listed — that arm is defensive, and a residual absence is an
        // unanswered view of this height, not a definitive not-found.
        let claim = match target.inclusion_hash {
            Some(reported) => {
                if let Err(incoherence) =
                    coherence::require_inclusion_anchor(number, fetched, reported)
                {
                    entries
                        .push(BatchEntry::Failed(incoherent_endpoint(target.hash, &incoherence)));
                    continue;
                }
                MembershipClaim::ResolvedInclusion
            }
            None => MembershipClaim::QueuedAgainstBlock,
        };
        if let Err(incoherence) = coherence::require_body_membership(
            number,
            fetched,
            target.hash,
            body_txs.contains(&target.hash),
            claim,
        ) {
            entries.push(BatchEntry::Failed(incoherent_endpoint(target.hash, &incoherence)));
            continue;
        }
        active.push(target.hash);
    }
    // Every target either failed an inclusion/membership check or was a
    // `--block` target already taken from the body. Nothing left to execute.
    if active.is_empty() {
        return BlockReplayOutcome::ordered(entries, &job_targets, Some(&block_tx_order), None);
    }
    let targets = active;

    // Both guards below check the *headers* the endpoint served. The state
    // reads behind the fork are still addressed by block number, so an endpoint
    // that serves headers and state from different backends can still hand back
    // state for a different block at this height. Anchoring state reads to the
    // validated hash would need the fork to take a block hash rather than a
    // number, and would change every cached RPC key (alloy hashes the block id
    // into the cache key), invalidating every committed offline capture.
    //
    // Parent/block linkage guard: across a reorg or a load-balanced endpoint
    // serving divergent views, `eth_getBlockByNumber(N-1)` can return a block
    // that is not the parent of the block being replayed. Forking from that
    // state would silently execute against the wrong pre-state.
    let parent_block = match fetch_block(provider, number - 1).await {
        Ok(block) => block,
        Err(e) => {
            return BlockReplayOutcome::ordered(
                fail_remaining(&targets, entries, BatchErrorKind::Rpc, &e.to_string()),
                &job_targets,
                Some(&block_tx_order),
                None,
            );
        }
    };
    if let Err(incoherence) =
        coherence::require_parent_linkage(parent_block.hash(), block.header.parent_hash())
    {
        return BlockReplayOutcome::ordered(
            fail_remaining(&targets, entries, BatchErrorKind::Rpc, &incoherence.to_string()),
            &job_targets,
            Some(&block_tx_order),
            None,
        );
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
    let fixture_dump = dump_dir.map(|dir| {
        let mut bucket_capacities = external_envs.bucket_capacities();
        bucket_capacities.sort_unstable();
        let mut oracle_storage = external_envs.oracle_storage();
        oracle_storage.sort_unstable();
        FixtureDumpEnv { dir, overwrite, mega_env: MegaEnv { bucket_capacities, oracle_storage } }
    });

    let hardforks = get_hardfork_config(chain_id);
    let timestamp = block.header.timestamp();
    let spec = hardforks.spec_id(timestamp);
    let chain_args = ChainArgs { chain_id, spec: spec.to_string() };
    debug!(block = number, chain_id, spec = %spec, "Block configuration");

    let cfg_env = match chain_args.create_cfg_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            return BlockReplayOutcome::ordered(
                fail_remaining(&targets, entries, BatchErrorKind::Execution, &e.to_string()),
                &job_targets,
                Some(&block_tx_order),
                None,
            );
        }
    };
    let block_env = match retrieve_block_env(&block) {
        Ok(env) => env,
        Err(e) => {
            return BlockReplayOutcome::ordered(
                fail_remaining(&targets, entries, BatchErrorKind::Execution, &e.to_string()),
                &job_targets,
                Some(&block_tx_order),
                None,
            );
        }
    };
    let executed_spec = cfg_env.spec;
    let evm_env = EvmEnv::new(cfg_env, block_env);

    let Some(hardfork) = hardforks.hardfork(timestamp) else {
        let message = format!("No `MegaHardfork` active at block timestamp: {timestamp}");
        return BlockReplayOutcome::ordered(
            fail_remaining(&targets, entries, BatchErrorKind::Execution, &message),
            &job_targets,
            Some(&block_tx_order),
            None,
        );
    };
    let block_limits =
        BlockLimits::from_hardfork_and_block_gas_limit(hardfork, block.header.gas_limit());
    let block_ctx = MegaBlockExecutionCtx::new(
        parent_block.hash(),
        block.header.parent_beacon_block_root(),
        block.header.extra_data().clone(),
        block_limits,
    );

    let target_set: HashSet<B256> = targets.iter().copied().collect();
    // Prefer the already-collected body order so stream ordering and the kernel
    // walk the same sequence.
    let tx_hashes = block_tx_order.clone();

    let mut hook = FixtureDraftHook {
        dump: fixture_dump,
        chain_id,
        executed_spec,
        block: &block,
        onchain_receipts: &onchain_receipts,
    };
    let run = kernel::execute_until_targets(
        provider,
        kernel::MinedBlockRun {
            hardforks: &hardforks,
            external_envs,
            block_ctx,
            evm_env,
            inspector: NoOpInspector,
            fork_block: parent_block.header.number(),
            identity: kernel::BlockIdentity { number, timestamp, hash: block.hash() },
            tx_hashes: &tx_hashes,
            targets: &target_set,
        },
        &mut hook,
    )
    .await;

    // A block that never started reports the same failure for every target it
    // was asked about, classified by what stopped it: a failed fork is an
    // unanswered endpoint question, while rejected pre-execution changes are
    // the executor's own verdict on the block.
    let kernel::BlockRun { loop_outcome, finish } = match run {
        Ok(run) => run,
        Err(setup) => {
            let (kind, message) = match setup {
                kernel::SetupError::Fork(e) => (BatchErrorKind::Rpc, e.to_string()),
                kernel::SetupError::PreExecution(e) => (classify(&e), e.to_string()),
            };
            return BlockReplayOutcome::ordered(
                fail_remaining(&targets, entries, kind, &message),
                &job_targets,
                Some(&block_tx_order),
                None,
            );
        }
    };

    // `entries` already holds any inclusion/membership failure recorded before
    // the block started; the harvested targets are appended to it here.
    match finish {
        kernel::FinishOutcome::Harvested(harvest) => {
            for harvested in harvest {
                let target = match harvested {
                    kernel::TargetHarvest::Receipt(target) => target,
                    kernel::TargetHarvest::MissingReceipt { tx_hash, tx_index } => {
                        entries.push(failure(
                            tx_hash,
                            BatchErrorKind::Execution,
                            format!("No receipt produced for transaction index {tx_index}"),
                        ));
                        continue;
                    }
                };
                // Keep the execution result even when the receipt question went
                // unanswered: the target did replay, so its summary, local
                // receipt, and timing stay on the result line. The verification
                // field carries the failure; the tally counts it as rpc.
                let verification = if verify_receipt {
                    match onchain_receipts.get(&target.tx_hash) {
                        Some(Ok(onchain)) => Some(verify::compare(
                            onchain,
                            &ReceiptFacts::from_receipt(&target.receipt),
                        )),
                        Some(Err(message)) => {
                            Some(VerificationOutcome::unavailable(message.clone()))
                        }
                        None => Some(VerificationOutcome::unavailable(
                            "No on-chain receipt was fetched for this transaction",
                        )),
                    }
                } else {
                    None
                };
                // A fixture that could not be written stays on the target's own
                // result line: the target did replay, so its receipt and its
                // verification verdict are still what the run was asked for. The
                // failed dump fails the run through the tally, not by replacing
                // the result with an error entry.
                //
                // Materialize only when the block loop completed cleanly: a
                // mid-block abort after this target built a Ready draft must
                // not publish (or clobber) a fixture for a block that failed.
                // That is the kernel's rule, not this driver's discipline — a
                // draft is only redeemable against the run's clean-run proof,
                // and an aborted run has none to hand out. Keep the execution
                // result; only the fixture field fails, and it inherits the
                // abort's class so a transient RPC abort exits 3.
                let fixture = match &loop_outcome {
                    kernel::LoopOutcome::Completed(clean) => {
                        target.draft.redeem(clean).map(materialize_deferred_fixture)
                    }
                    kernel::LoopOutcome::Aborted { error, .. } => {
                        target.draft.peek().as_ref().map(|draft| discarded_fixture(draft, error))
                    }
                };
                entries.push(BatchEntry::Executed(Box::new(ExecutedTx {
                    tx_hash: target.tx_hash,
                    block_number: number,
                    tx_index: target.tx_index,
                    exec_result: target.exec_result,
                    contract_address: target.contract_address,
                    exec_time: target.exec_time,
                    receipt: target.receipt,
                    verification,
                    fixture,
                })));
            }
        }
        // The block itself failed to finish, so no target of it has a receipt
        // and no deferred fixture is written or replaced.
        kernel::FinishOutcome::Failed { error, executed } => {
            let kind = classify(&error);
            let message = error.to_string();
            for tx_hash in executed {
                entries.push(failure(tx_hash, kind, message.clone()));
            }
        }
    }

    // Any active target that produced no entry sat behind an abort (or is a
    // residual not-in-body case for `--block`, which has no inclusion claim).
    // They are appended in block transaction-index order, keeping the run's
    // ascending (block, index) order; a target the block does not contain has
    // no index and keeps its input position among the active set.
    let reported: HashSet<B256> = entries.iter().map(BatchEntry::tx_hash).collect();
    let block_txs: HashSet<B256> = tx_hashes.iter().copied().collect();
    let unreported = tx_hashes
        .iter()
        .filter(|hash| target_set.contains(*hash))
        .chain(targets.iter().filter(|hash| !block_txs.contains(*hash)))
        .filter(|hash| !reported.contains(*hash));

    let mut uncounted_abort = None;
    match &loop_outcome {
        kernel::LoopOutcome::Completed(_) => {
            // Active targets are already filtered for inclusion agreement; a
            // remaining absence from the body is still an endpoint
            // inconsistency (the target was queued against this block), not a
            // definitive not-found.
            for tx_hash in unreported {
                let incoherence = Incoherence::AbsentFromBody {
                    number,
                    block_hash: fetched,
                    tx_hash: *tx_hash,
                    claim: MembershipClaim::ResolvedInclusion,
                };
                entries.push(BatchEntry::Failed(incoherent_endpoint(*tx_hash, &incoherence)));
            }
        }
        kernel::LoopOutcome::Aborted { error: e, tx_hash: aborting } => {
            warn!(block = number, error = %e, "Aborted block replay; skipping its remaining targets");
            let root_kind = classify(e);
            let mut root_on_target = false;
            for tx_hash in unreported {
                if aborting == tx_hash {
                    // The abort is this target's own answer.
                    root_on_target = true;
                    entries.push(failure(*tx_hash, root_kind, e.to_string()));
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
            // When the aborter is not a reported target, no failure entry carries
            // the abort's own class. Tallied separately so the run exit reflects
            // the root cause (e.g. exit 1 for a deterministic non-target abort).
            // Fixture abort-errors on executed targets may also carry the class;
            // double-counting the same class still yields the correct exit.
            if !root_on_target {
                // If finish failed for pending targets that already include the
                // aborter as a Failed entry, the class is already counted.
                let already_counted = entries.iter().any(|entry| match entry {
                    BatchEntry::Failed(tx) => tx.tx_hash == *aborting && tx.kind == root_kind,
                    BatchEntry::Executed(_) => false,
                });
                // Abort-inherited fixture failures on executed targets already
                // contribute the abort class to the tally.
                let fixture_carries_class = entries.iter().any(|entry| match entry {
                    BatchEntry::Executed(tx) => tx
                        .fixture
                        .as_ref()
                        .is_some_and(|f| f.is_error() && f.error_kind == root_kind),
                    BatchEntry::Failed(_) => false,
                });
                if !already_counted && !fixture_carries_class {
                    uncounted_abort = Some(root_kind);
                }
            }
        }
    }

    BlockReplayOutcome::ordered(entries, &job_targets, Some(&block_tx_order), uncounted_abort)
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
/// Genuine skips (fidelity mismatch, BLOCKHASH, unsupported transaction shapes)
/// become a final [`FixtureReport::skipped`] and never fail the run.
/// An unanswered on-chain receipt (transport, pruned/null, divergent inclusion,
/// or offline envelope lacking it) becomes a rpc-class fixture error so the run
/// exits 3 while the target keeps its execution result line.
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

    // Fidelity gate needs the on-chain receipt. When the receipt question went
    // unanswered the dump fails as rpc (not a fidelity-gate skip): the run was
    // asked to write a fixture and could not obtain the receipt it needs. Genuine
    // gate skips (BLOCKHASH, unsupported shape, fidelity mismatch) stay skips.
    let facts = match onchain {
        Some(Ok(facts)) => facts,
        Some(Err(message)) => {
            return DeferredFixture::Report(FixtureReport::rpc_error(message.clone()));
        }
        None => {
            return DeferredFixture::Report(FixtureReport::rpc_error(
                "no on-chain receipt was fetched for this transaction",
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
    // Fast-path courtesy: refuse overwrite before carrying a ready draft so the
    // harvest path never confuses a finish failure with an overwrite refusal.
    // Correctness against a concurrent creator is still enforced at materialize
    // time via noclobber persist.
    if path.exists() && !overwrite {
        return DeferredFixture::Report(FixtureReport::error(format!(
            "fixture already exists at {} (pass --overwrite to replace)",
            path.display()
        )));
    }

    DeferredFixture::Ready { draft: Box::new(draft), path, overwrite }
}

/// Finalize a deferred fixture after the block `finish()` succeeded.
///
/// Ready drafts are self-validated and written here. Pre-decided reports pass
/// through unchanged. On finish failure the caller drops the deferred value
/// without calling this, so no file is written or replaced.
fn materialize_deferred_fixture(deferred: DeferredFixture) -> FixtureReport {
    match deferred {
        DeferredFixture::Report(report) => report,
        DeferredFixture::Ready { draft, path, overwrite } => {
            match fixture::finalize_and_write(*draft, &path, overwrite) {
                Ok(()) => {
                    info!(path = %path.display(), "Wrote self-validating fixture");
                    FixtureReport::written(&path)
                }
                Err(e) => {
                    let message = e.to_string();
                    // Noclobber / prep-time refusal already carry the full
                    // "already exists … --overwrite" text; do not wrap them.
                    if message.contains("already exists") {
                        FixtureReport::error(message)
                    } else {
                        FixtureReport::error(format!("fixture write failed: {message}"))
                    }
                }
            }
        }
    }
}

/// Report a deferred fixture the block aborted on before it could be written.
///
/// The kernel hands an aborted run's drafts back for reading only, which is
/// exactly what this needs: a pre-decided report is what it always was, and a
/// draft that was ready to write becomes a failure naming the file that was not
/// created, classified as the abort that prevented it.
fn discarded_fixture(deferred: &DeferredFixture, abort: &ReplayError) -> FixtureReport {
    match deferred {
        DeferredFixture::Report(report) => report.clone(),
        DeferredFixture::Ready { path, .. } => FixtureReport::abort_error(
            format!(
                "fixture not written: block aborted before a clean finish \
                 (draft for {} was discarded): {abort}",
                path.display()
            ),
            classify(abort),
        ),
    }
}

/// Classify a [`fixture::build_draft`] error as a skip (unsupported shape) or a
/// fixture construction error (database / other failures).
///
/// Unsupported shapes are expected in whole-block sweeps and must not fail the
/// run. Endpoint/DB failures during construction mean the requested artifact
/// could not be produced and fail the run as an execution-class fixture error.
/// The builder decides which is which at the point it knows, so rewording any of
/// its messages cannot silently reclassify a sweep.
fn fixture_report_from_build_err(err: fixture::FixtureBuildError) -> FixtureReport {
    match err {
        fixture::FixtureBuildError::Unsupported(reason) => FixtureReport::skipped(reason),
        fixture::FixtureBuildError::Construction(err) => {
            FixtureReport::error(format!("fixture construction failed: {err}"))
        }
    }
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
///
/// Every block this driver executes against — the replayed block and the parent
/// it forks from — is fetched here, so the served header is authenticated here
/// too: a header that does not hash to the hash it was served under, or that
/// claims a height other than the one asked for, is rejected before any guard
/// consumes that hash and before the block environment is built from its fields.
/// The checks are local, so they issue no additional request.
async fn fetch_block<P>(provider: &P, number: u64) -> Result<Block<Transaction>>
where
    P: Provider<op_alloy_network::Optimism>,
{
    let block = provider
        .get_block_by_number(number.into())
        .await
        .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
        .ok_or(ReplayError::BlockNotFound(number))?;
    coherence::authenticate_block_header(&block.header, number)
        .map_err(|incoherence| ReplayError::RpcError(incoherence.to_string()))?;
    Ok(block)
}

/// Map an error raised while replaying a block onto the kind reported for the
/// target the error is about.
///
/// This is a per-target reporting classification, not an exit-code decision:
/// the kinds are tallied across the run and [`ExitCode`] resolves the process
/// exit from those tallies, staying the single authority on what each class
/// exits with. Nothing here re-derives that judgment. The one arm where the
/// class is not visible in the variant — a block error whose real cause is a
/// failed state read, buried in the executor's error or stringified into a
/// validation message — asks [`ExitCode::from_evme_error`] rather than
/// re-implementing its unwrapping, so the two can never drift apart. Classify a
/// new error variant the same way: by delegating, never by copying the
/// authority's rules.
fn classify(err: &ReplayError) -> BatchErrorKind {
    match err {
        ReplayError::TransactionNotFound(_) => BatchErrorKind::NotFound,
        ReplayError::RpcError(_) |
        ReplayError::RpcTransportError(_) |
        ReplayError::BlockBodyTransactionNull(_) |
        ReplayError::BlockBodyTransactionFetch { .. } => BatchErrorKind::Rpc,
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

/// The kind reported for a target swept up by an abort caused elsewhere:
/// always `rpc`, whatever class the abort itself had.
///
/// The abort says nothing about this target: whatever class caused the block to
/// stop (unknown hash, RPC failure, executor/setup error on another
/// transaction), a non-aborting swept target is unanswered (`rpc`). Only the
/// transaction that caused the abort keeps its own classified kind (when it is
/// a reported target); otherwise the run tallies the abort class separately so
/// the exit code still reflects the root cause.
///
/// The abort's cause is therefore a parameter this function deliberately does
/// not read — hence the underscore. It stays in the signature so the decision is
/// stated where the cause is in hand: a change that wants to classify swept
/// targets by cause has to argue against the rule above rather than silently
/// add a parameter.
fn swept_kind(_abort_cause: &ReplayError) -> BatchErrorKind {
    BatchErrorKind::Rpc
}

/// Adapt a shared coherence verdict to this target's failure record.
///
/// An incoherent answer is the endpoint contradicting itself (a reorg in
/// progress, or a load-balanced endpoint serving divergent views), so nothing
/// definitive was established about the target: it stays unanswered (`rpc`),
/// with the verdict's own wording as its message.
fn incoherent_endpoint(tx_hash: B256, incoherence: &Incoherence) -> FailedTx {
    FailedTx { tx_hash, kind: BatchErrorKind::Rpc, message: incoherence.to_string() }
}

/// Build a failure entry.
fn failure(tx_hash: B256, kind: BatchErrorKind, message: String) -> BatchEntry {
    BatchEntry::Failed(FailedTx { tx_hash, kind, message })
}

/// Report the same failure for every target of a block that never started.
fn fail_all(
    targets: impl IntoIterator<Item = B256>,
    kind: BatchErrorKind,
    message: &str,
) -> Vec<BatchEntry> {
    targets.into_iter().map(|hash| failure(hash, kind, message.to_string())).collect()
}

/// Append the same failure for every remaining target, keeping any entries
/// already recorded (for example inclusion mismatches decided earlier).
fn fail_remaining(
    targets: &[B256],
    mut entries: Vec<BatchEntry>,
    kind: BatchErrorKind,
    message: &str,
) -> Vec<BatchEntry> {
    let reported: HashSet<B256> = entries.iter().map(BatchEntry::tx_hash).collect();
    for hash in targets {
        if !reported.contains(hash) {
            entries.push(failure(*hash, kind, message.to_string()));
        }
    }
    entries
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
        VerificationOutcome::compared(matched, None)
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
        assert_eq!(
            counts,
            BatchFailureCounts {
                execution: 2,
                rpc: 1,
                mismatched: 1,
                total: 5,
                ..Default::default()
            }
        );
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
        assert_eq!(
            counts,
            BatchFailureCounts {
                execution: 1,
                rpc: 0,
                mismatched: 1,
                total: 1,
                ..Default::default()
            }
        );
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
    ///
    /// The abort's own class is tallied separately when the aborter is not a
    /// target (`record_uncounted_abort`); swept entries stay `rpc`.
    #[test]
    fn test_swept_kind_always_rpc_regardless_of_abort_class() {
        // Unknown hash: already unanswered for the cause, and for swept peers.
        assert_eq!(swept_kind(&ReplayError::TransactionNotFound(B256::ZERO)), BatchErrorKind::Rpc);
        // Transport/RPC failure.
        assert_eq!(swept_kind(&ReplayError::RpcError("endpoint down".into())), BatchErrorKind::Rpc);
        // Block-body null is rpc-class for the aborting target and for sweeps.
        assert_eq!(
            swept_kind(&ReplayError::BlockBodyTransactionNull(B256::ZERO)),
            BatchErrorKind::Rpc
        );
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

    /// A block-body hash resolving to null is an RPC inconsistency, and its
    /// message names the vanished transaction the abort is attributed to.
    #[test]
    fn test_block_body_transaction_null_classifies_as_rpc_and_names_the_hash() {
        let hash = B256::repeat_byte(0xab);
        let err = ReplayError::BlockBodyTransactionNull(hash);
        assert_eq!(classify(&err), BatchErrorKind::Rpc);
        let message = err.to_string();
        assert!(
            message.contains(&hash.to_string()) || message.contains(&format!("{hash:#x}")),
            "unexpected message: {message}"
        );
        // Contrasts with the user-supplied unknown-hash definitive answer.
        assert_eq!(classify(&ReplayError::TransactionNotFound(hash)), BatchErrorKind::NotFound);
    }

    /// A block-body fetch failure (transport / cache miss) is rpc-class and
    /// names the hash, matching the null-answer pattern.
    #[test]
    fn test_block_body_transaction_fetch_classifies_as_rpc_and_names_the_hash() {
        let hash = B256::repeat_byte(0xcd);
        let err = ReplayError::BlockBodyTransactionFetch {
            tx_hash: hash,
            message: "cache miss in offline replay file".into(),
        };
        assert_eq!(classify(&err), BatchErrorKind::Rpc);
        let message = err.to_string();
        assert!(message.contains(&hash.to_string()) || message.contains(&format!("{hash:#x}")));
        assert!(message.contains("fetching it failed"), "unexpected message: {message}");
        assert!(message.contains("cache miss"), "unexpected message: {message}");
    }

    /// An uncounted non-target abort floors the exit class without a synthetic
    /// reported entry and without inflating per-target failure totals.
    #[test]
    fn test_batch_tally_uncounted_abort_drives_exit_class() {
        let mut tally = BatchTally::default();
        // Two targets swept as unanswered behind a non-target execution abort.
        tally.record(&failure(B256::repeat_byte(0x01), BatchErrorKind::Rpc, "swept".into()));
        tally.record(&failure(B256::repeat_byte(0x02), BatchErrorKind::Rpc, "swept".into()));
        tally.record_uncounted_abort(BatchErrorKind::Execution);

        assert_eq!(tally.reported, 2, "uncounted abort is not a reported target");
        assert_eq!(tally.counts.rpc, 2, "target counters stay per-target");
        assert_eq!(tally.counts.execution, 0, "abort must not inflate execution count");
        assert_eq!(tally.exit_floor, BatchExitFloor::Execution);
        let err = tally.into_error().expect("run failed");
        let ReplayError::BatchFailed(counts) = err else {
            panic!("expected batch failure: {err:?}");
        };
        assert_eq!(
            counts.to_string(),
            "2 of 2 target transaction(s) failed (0 execution, 2 rpc)",
            "aggregate message must stay truthful about targets"
        );
        assert_eq!(ExitCode::from_batch_failures(&counts), ExitCode::ExecutionError);
    }

    /// One unanswered receipt with both `--verify-receipt` and
    /// `--dump-fixture-dir` counts as a single rpc failure, while both result
    /// fields remain present on the executed entry.
    #[test]
    fn test_batch_tally_shared_receipt_failure_counted_once() {
        let mut tally = BatchTally::default();
        tally.record_executed(
            Some(&VerificationOutcome::unavailable("receipt pruned")),
            Some(&FixtureReport::rpc_error("no on-chain receipt was fetched for this transaction")),
        );

        assert_eq!(tally.reported, 1);
        assert_eq!(tally.replayed, 1);
        assert_eq!(tally.verified, 0);
        assert_eq!(tally.counts.rpc, 1, "shared receipt failure is one rpc count");
        assert_eq!(tally.counts.execution, 0);
        let err = tally.into_error().expect("run failed");
        let ReplayError::BatchFailed(counts) = err else {
            panic!("expected batch failure: {err:?}");
        };
        assert_eq!(counts.to_string(), "1 of 1 target transaction(s) failed (0 execution, 1 rpc)");
        assert_eq!(ExitCode::from_batch_failures(&counts), ExitCode::RpcFailure);
    }

    /// Independent findings on the same target still both count: a receipt
    /// mismatch plus a fixture write failure is not a shared root cause.
    #[test]
    fn test_batch_tally_mismatch_and_fixture_error_are_independent() {
        let mut tally = BatchTally::default();
        tally.record_executed(Some(&verdict(false)), Some(&FixtureReport::error("disk full")));

        assert_eq!(tally.counts.mismatched, 1);
        assert_eq!(tally.counts.execution, 1);
        assert_eq!(tally.counts.rpc, 0);
    }

    /// Documented stream order: body-index first, then absent targets last in
    /// job input order — independent of the order entries were collected.
    #[test]
    fn test_order_block_entries_body_index_before_absent_last() {
        let early = B256::repeat_byte(0x11);
        let mid = B256::repeat_byte(0x22);
        let late_absent = B256::repeat_byte(0x33);
        let job_targets = vec![
            JobTarget { hash: late_absent, inclusion_hash: Some(B256::ZERO) },
            JobTarget { hash: early, inclusion_hash: Some(B256::ZERO) },
            JobTarget { hash: mid, inclusion_hash: Some(B256::ZERO) },
        ];
        // Collected in the buggy pre-execution-first order: absent then results.
        let entries = vec![
            failure(late_absent, BatchErrorKind::Rpc, "inclusion".into()),
            failure(mid, BatchErrorKind::Rpc, "swept".into()),
            failure(early, BatchErrorKind::Rpc, "swept".into()),
        ];
        let body = [early, mid, B256::repeat_byte(0x99)];
        let ordered = order_block_entries(entries, &job_targets, Some(&body));
        let hashes: Vec<B256> = ordered.iter().map(BatchEntry::tx_hash).collect();
        assert_eq!(
            hashes,
            vec![early, mid, late_absent],
            "body order first, absent last: {hashes:?}"
        );
    }

    /// A fixture discarded after a transport abort counts as rpc, not execution,
    /// so the run exit matches the abort class.
    #[test]
    fn test_batch_tally_abort_inherited_fixture_error_is_rpc_class() {
        let mut tally = BatchTally::default();
        tally.record_executed(
            None,
            Some(&FixtureReport::abort_error(
                "fixture not written: block aborted: transport",
                BatchErrorKind::Rpc,
            )),
        );

        assert_eq!(tally.replayed, 1);
        assert_eq!(tally.counts.rpc, 1);
        assert_eq!(tally.counts.execution, 0);
        let err = tally.into_error().expect("run failed");
        let ReplayError::BatchFailed(counts) = err else {
            panic!("expected batch failure: {err:?}");
        };
        assert_eq!(ExitCode::from_batch_failures(&counts), ExitCode::RpcFailure);
    }

    /// An unanswered receipt for `--verify-receipt` keeps the target as
    /// replayed, counts as rpc (not mismatched), and is not "verified".
    #[test]
    fn test_batch_tally_verification_unavailable_is_rpc_and_still_replayed() {
        let mut tally = BatchTally::default();
        tally.record_executed(Some(&VerificationOutcome::unavailable("receipt pruned")), None);

        assert_eq!(tally.replayed, 1, "the target still replayed");
        assert_eq!(tally.verified, 0, "no comparison ran");
        assert_eq!(tally.counts.mismatched, 0, "unverified is not a mismatch");
        assert_eq!(tally.counts.rpc, 1);
        let err = tally.into_error().expect("run failed");
        let ReplayError::BatchFailed(counts) = err else {
            panic!("expected batch failure: {err:?}");
        };
        assert_eq!(ExitCode::from_batch_failures(&counts), ExitCode::RpcFailure);
    }

    /// An unanswered receipt for `--dump-fixture-dir` is a rpc-class fixture
    /// error, not a skip that exits 0.
    #[test]
    fn test_batch_tally_fixture_receipt_unavailable_is_rpc_class() {
        let mut tally = BatchTally::default();
        tally.record_executed(
            None,
            Some(&FixtureReport::rpc_error("no on-chain receipt was fetched for this transaction")),
        );

        assert_eq!(tally.replayed, 1);
        assert_eq!(tally.counts.rpc, 1);
        assert_eq!(tally.counts.execution, 0);
        let err = tally.into_error().expect("run failed");
        let ReplayError::BatchFailed(counts) = err else {
            panic!("expected batch failure: {err:?}");
        };
        assert_eq!(ExitCode::from_batch_failures(&counts), ExitCode::RpcFailure);
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

    /// Unsupported shapes remain skips; construction failures become fixture
    /// errors so the run exits non-zero.
    ///
    /// Which builder rejection lands in which variant is decided inside
    /// `build_draft` and pinned by the integration tests that drive the real
    /// builder (deposit skip, injected pre-state failure); this test only pins
    /// the variant-to-report mapping, which no rewording can move.
    #[test]
    fn test_fixture_build_err_classifies_skips_vs_construction_errors() {
        let unsupported = fixture_report_from_build_err(fixture::FixtureBuildError::Unsupported(
            "--dump-fixture does not support deposit transactions".into(),
        ));
        assert!(unsupported.skipped.is_some(), "unsupported shape is a skip: {unsupported:?}");
        assert!(unsupported.error.is_none());
        assert_eq!(
            unsupported.skipped.as_deref(),
            Some("--dump-fixture does not support deposit transactions"),
            "the builder's reason is reported verbatim"
        );

        let construction = fixture_report_from_build_err(fixture::FixtureBuildError::Construction(
            ReplayError::Other(
                "pre-state read for 0x00000000000000000000000000000000000000aa: \
                     database unavailable"
                    .into(),
            ),
        ));
        assert!(
            construction.error.as_ref().is_some_and(|m| m.contains("construction failed")),
            "construction failure is a fixture error: {construction:?}"
        );
        assert!(construction.skipped.is_none());
    }

    /// A pre-decided fixture report is never rewritten by materialization, so a
    /// finish failure that drops a Ready draft (without calling materialize)
    /// cannot leave a file and a Report never touches the filesystem.
    #[test]
    fn test_materialize_deferred_fixture_passes_reports_through() {
        let skipped = materialize_deferred_fixture(DeferredFixture::Report(
            FixtureReport::skipped("fidelity gate failed: gas_used"),
        ));
        assert_eq!(skipped.skipped.as_deref(), Some("fidelity gate failed: gas_used"));
        assert!(skipped.path.is_none());
        assert!(skipped.error.is_none());

        let err = materialize_deferred_fixture(DeferredFixture::Report(FixtureReport::error(
            "fixture construction failed: code fetch failed",
        )));
        assert!(err.error.as_ref().is_some_and(|m| m.contains("construction failed")));
        assert!(err.path.is_none());

        let rpc = materialize_deferred_fixture(DeferredFixture::Report(FixtureReport::rpc_error(
            "no on-chain receipt was fetched for this transaction",
        )));
        assert!(rpc.is_error());
        assert_eq!(rpc.error_kind, BatchErrorKind::Rpc);
        assert!(rpc.skipped.is_none());
    }
}
