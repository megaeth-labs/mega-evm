//! The execution-spec blockchain tests, imported through Satin's block executor.
//!
//! A blockchain test is a chain: a pre-state, a genesis block, and blocks a node imports one after
//! the other, some of which it must refuse. [`run`] imports every test the fixtures under a set
//! of paths define for Osaka through [`MegaBlockExecutor`](mega_evm::MegaBlockExecutor) — many
//! transactions to a block, many blocks to a test, the pre-block system calls before every block
//! — on `MegaEvm` in equivalence mode's neutral configuration, and judges each block:
//!
//! - a block the fixture expects to be valid must be accepted, and must produce the header's gas
//!   used, logs bloom and receipts root, and leave its state root, the accounts a Satin block adds
//!   to the state taken out ([`SATIN_ACCOUNTS`]);
//! - a block the fixture expects to be invalid for a `TransactionException` must be refused, for
//!   one of the exceptions it names ([`exceptions`](crate::exceptions)), and leaves the chain at
//!   the previous block;
//! - once every block is imported, the chain's head must be the fixture's last block and its state
//!   the fixture's post-state.
//!
//! Each test is passed, skipped for a [reason](SkipReason) decided from its content before
//! anything runs, or failed at its first block that does not match. A failure is explained only by
//! a [deviation](crate::deviations) the state-test registry already has, which lists the test with
//! what Satin produces for the block it fails at; any other failure is unattributed, and
//! [`Report::gate`] fails on one.
//!
//! The devnet release's blockchain tests are not run: the revm fork's own runner skips them too.

mod chain;
mod fixture;
pub mod skips;

use std::{
    collections::BTreeMap,
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

pub use chain::{chain_spec, registry_config, Added, SatinAccount, FORK, SATIN_ACCOUNTS};
pub use fixture::NETWORK;
use mega_evm::{alloy_consensus::Header, revm::primitives::B256};
use serde::Serialize;
pub use skips::SkipReason;

use self::{
    chain::{BlockOutput, Chain},
    fixture::{decode_block, Network, Suite, Test},
};
use crate::{
    deviations::{BlockchainEntry, Deviation},
    exceptions::{check_names, Mismatch},
};

/// How a run executes its fixtures.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Worker threads; one runs the files in order.
    pub threads: usize,
    /// Print one JSON line per test to standard error.
    pub json_outcome: bool,
    /// The registry failures are attributed to:
    /// [`deviations::DEVIATIONS`](crate::deviations::DEVIATIONS) for the gate.
    pub deviations: &'static [Deviation],
}

/// Which test a result is about.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct TestId {
    /// The fixture file, as it was found under the paths the run was given.
    pub path: String,
    /// The test's name within the file.
    pub name: String,
}

impl fmt::Display for TestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} :: {}", self.path, self.name)
    }
}

/// How a test failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// The executor refused a block the fixture expects to be valid.
    UnexpectedException,
    /// The executor accepted a block the fixture expects to be invalid.
    MissingException,
    /// The executor refused a block for another reason than the ones the fixture names.
    WrongException,
    /// The executor refused a block with an error no execution-spec exception names.
    UnnamedException,
    /// An accepted block used other gas than its header says.
    GasUsedMismatch,
    /// An accepted block's logs bloom is not its header's.
    LogsBloomMismatch,
    /// An accepted block's receipts root is not its header's.
    ReceiptsRootMismatch,
    /// An accepted block's state root is not its header's.
    StateRootMismatch,
    /// Once every block is imported, the chain's head is not the fixture's last block, or its
    /// state is not the fixture's post-state.
    PostStateMismatch,
    /// The fixture could not be read, parsed or decoded.
    Fixture,
    /// Running the test panicked.
    Panic,
}

impl FailureKind {
    /// Every kind, in the order a block's comparisons are made.
    pub const ALL: [Self; 11] = [
        Self::UnexpectedException,
        Self::MissingException,
        Self::WrongException,
        Self::UnnamedException,
        Self::GasUsedMismatch,
        Self::LogsBloomMismatch,
        Self::ReceiptsRootMismatch,
        Self::StateRootMismatch,
        Self::PostStateMismatch,
        Self::Fixture,
        Self::Panic,
    ];

    /// The kind's name, as the summary prints it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::UnexpectedException => "unexpected-exception",
            Self::MissingException => "missing-exception",
            Self::WrongException => "wrong-exception",
            Self::UnnamedException => "unnamed-exception",
            Self::GasUsedMismatch => "gas-used-mismatch",
            Self::LogsBloomMismatch => "logs-bloom-mismatch",
            Self::ReceiptsRootMismatch => "receipts-root-mismatch",
            Self::StateRootMismatch => "state-root-mismatch",
            Self::PostStateMismatch => "post-state-mismatch",
            Self::Fixture => "fixture",
            Self::Panic => "panic",
        }
    }
}

/// What Satin produced for an accepted block that does not match its header: the block, and the
/// gas used, receipts root and state root it produced. The receipts root commits to every
/// receipt's bloom, so the logs bloom adds nothing to say which outcome this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Produced {
    /// The block's index among the test's blocks.
    pub block: usize,
    /// The gas the block used.
    pub gas_used: u64,
    /// The root of the block's receipts.
    pub receipts_root: B256,
    /// The root of the state the block left, the accounts Satin added taken out.
    pub state_root: B256,
}

impl fmt::Display for Produced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "block {}: gas used {}, receipts root {}, state root {}",
            self.block, self.gas_used, self.receipts_root, self.state_root
        )
    }
}

/// A failed test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// How it failed.
    pub kind: FailureKind,
    /// The index of the block it failed at, when it failed at one.
    pub block: Option<usize>,
    /// What Satin produced for the block, when the block was accepted and does not match its
    /// header.
    pub produced: Option<Produced>,
    /// What was expected and what happened.
    pub detail: String,
    /// The deviation that explains it, if one does.
    pub deviation: Option<&'static str>,
}

impl Failure {
    fn new(kind: FailureKind, block: Option<usize>, detail: impl Into<String>) -> Self {
        Self { kind, block, produced: None, detail: detail.into(), deviation: None }
    }
}

/// What happened to a test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    /// It was imported and matched its fixture.
    Passed,
    /// It was not executed.
    Skipped {
        /// Why not.
        reason: SkipReason,
    },
    /// It was imported, or failed to be, and did not match its fixture.
    Failed(Failure),
}

/// The blocks of a test the runner imported and judged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Blocks {
    /// Blocks the executor accepted and that matched their headers.
    pub accepted: usize,
    /// Blocks the executor refused for an exception the fixture names.
    pub refused: usize,
}

/// A test and what happened to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TestResult {
    /// The test.
    pub id: TestId,
    /// What happened to it.
    pub outcome: Outcome,
    /// The blocks it imported as the fixture expects, up to a failure.
    pub blocks: Blocks,
}

/// Every test a run was given, and what happened to each.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The registry failures were attributed to.
    #[serde(skip)]
    pub deviations: &'static [Deviation],
    /// The fixture files the run found.
    pub files: usize,
    /// Every test, ordered by file and name.
    pub results: Vec<TestResult>,
    /// The fixture files that could not be read or parsed, in path order, and why.
    pub file_failures: Vec<(String, Failure)>,
}

/// The counts of a [`Report`].
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// The fixture files the run found.
    pub files: usize,
    /// The tests the files define for Osaka: executed and skipped.
    pub defined: usize,
    /// The tests that executed.
    pub executed: usize,
    /// The tests that executed and matched their fixture.
    pub passed: usize,
    /// The tests that were not executed, by reason.
    pub skipped: BTreeMap<SkipReason, usize>,
    /// The tests that failed, by kind.
    pub failed: BTreeMap<FailureKind, usize>,
    /// The failed tests a deviation explains, by deviation.
    pub deviated: BTreeMap<&'static str, usize>,
    /// The failed tests no deviation explains.
    pub unattributed: usize,
    /// The tests a deviation lists that did not fail as listed, by deviation.
    pub unreproduced: BTreeMap<&'static str, usize>,
    /// The fixture files that could not be read or parsed.
    pub file_failures: usize,
    /// The blocks the executed tests imported as their fixtures expect: accepted, and refused.
    pub blocks: Blocks,
}

impl Summary {
    /// The tests that were not executed.
    pub fn skipped_total(&self) -> usize {
        self.skipped.values().sum()
    }

    /// The tests that failed.
    pub fn failed_total(&self) -> usize {
        self.failed.values().sum()
    }
}

impl Report {
    /// Counts the report.
    pub fn summary(&self) -> Summary {
        let mut summary = Summary {
            files: self.files,
            file_failures: self.file_failures.len(),
            ..Default::default()
        };
        for result in &self.results {
            summary.defined += 1;
            summary.blocks.accepted += result.blocks.accepted;
            summary.blocks.refused += result.blocks.refused;
            match &result.outcome {
                Outcome::Passed => {
                    summary.executed += 1;
                    summary.passed += 1;
                }
                Outcome::Skipped { reason } => *summary.skipped.entry(*reason).or_default() += 1,
                Outcome::Failed(failure) => {
                    summary.executed += 1;
                    *summary.failed.entry(failure.kind).or_default() += 1;
                    match failure.deviation {
                        Some(id) => *summary.deviated.entry(id).or_default() += 1,
                        None => summary.unattributed += 1,
                    }
                }
            }
        }
        for unreproduced in self.unreproduced() {
            *summary.unreproduced.entry(unreproduced.deviation.id).or_default() += 1;
        }
        summary
    }

    /// The failed tests no deviation explains.
    pub fn unattributed(&self) -> impl Iterator<Item = (&TestId, &Failure)> {
        self.results.iter().filter_map(|result| match &result.outcome {
            Outcome::Failed(failure) if failure.deviation.is_none() => Some((&result.id, failure)),
            _ => None,
        })
    }

    /// The tests the registry lists that did not fail as listed: exactly one result of the run
    /// is the test's, and its deviation explains it, which it does only for what it lists.
    pub fn unreproduced(&self) -> impl Iterator<Item = Unreproduced<'_>> {
        self.deviations.iter().flat_map(move |deviation| {
            deviation.blockchain_entries.iter().filter_map(move |entry| {
                let seen: Vec<_> =
                    self.results.iter().filter(|result| entry.is(&result.id)).collect();
                let reproduced = matches!(
                    seen.as_slice(),
                    [TestResult { outcome: Outcome::Failed(failure), .. }]
                        if failure.deviation == Some(deviation.id)
                );
                (!reproduced).then_some(Unreproduced { deviation, entry, seen })
            })
        })
    }

    /// What the gate finds wrong with this run: a fixture file that could not be read, an
    /// unattributed failure, and, for each count given, a count that differs from it. Empty when
    /// the gate passes.
    ///
    /// `expected_skipped` pins every class: a class it does not name is pinned at zero. With
    /// `check_deviations`, every test a registered deviation lists must fail exactly as listed,
    /// and each deviation explains as many failures as it lists.
    pub fn gate(
        &self,
        expected_executed: Option<usize>,
        expected_skipped: Option<&BTreeMap<SkipReason, usize>>,
        check_deviations: bool,
    ) -> Vec<String> {
        let summary = self.summary();
        let mut problems = Vec::new();
        if summary.file_failures > 0 {
            problems.push(format!("{} fixture files could not be read", summary.file_failures));
        }
        if summary.unattributed > 0 {
            problems.push(format!("{} failed tests no deviation explains", summary.unattributed));
        }
        if let Some(expected) = expected_executed.filter(|&n| n != summary.executed) {
            problems.push(format!("{} tests executed, {expected} pinned", summary.executed));
        }
        if let Some(expected) = expected_skipped {
            for reason in SkipReason::ALL {
                let skipped = summary.skipped.get(&reason).copied().unwrap_or(0);
                let pinned = expected.get(&reason).copied().unwrap_or(0);
                if skipped != pinned {
                    problems.push(format!(
                        "{skipped} tests skipped for {}, {pinned} pinned",
                        reason.name()
                    ));
                }
            }
        }
        if check_deviations {
            for deviation in self.deviations {
                let listed = deviation.blockchain_entries.len();
                let unreproduced = summary.unreproduced.get(deviation.id).copied().unwrap_or(0);
                if unreproduced > 0 {
                    problems.push(format!(
                        "deviation {}: {unreproduced} of the {listed} blockchain tests it lists \
                         did not fail as listed",
                        deviation.id
                    ));
                }
                let explained = summary.deviated.get(deviation.id).copied().unwrap_or(0);
                if explained != listed {
                    problems.push(format!(
                        "deviation {} explains {explained} failed blockchain tests, {listed} listed",
                        deviation.id
                    ));
                }
            }
        }
        problems
    }
}

/// A test a deviation lists that did not fail as listed.
#[derive(Debug)]
pub struct Unreproduced<'a> {
    /// The deviation.
    pub deviation: &'static Deviation,
    /// The test it lists.
    pub entry: &'static BlockchainEntry,
    /// The results the run has for the test: none when the run did not execute it.
    pub seen: Vec<&'a TestResult>,
}

/// Imports every Osaka test `files` define, and reports each.
pub fn run(files: &[PathBuf], config: Config) -> Report {
    let next = AtomicUsize::new(0);
    let results = Mutex::new((Vec::new(), Vec::new()));
    let threads = config.threads.clamp(1, files.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = files.get(index) else { break };
                let file = run_file(path, config);
                let mut results = results.lock().expect("no worker panics holding the lock");
                match file {
                    Ok(tests) => results.0.extend(tests),
                    Err(failure) => results.1.push((path.to_string_lossy().into_owned(), failure)),
                }
            });
        }
    });
    let (mut results, mut file_failures) =
        results.into_inner().expect("no worker panics holding the lock");
    results.sort_by(|a, b| a.id.cmp(&b.id));
    file_failures.sort_by(|a, b| a.0.cmp(&b.0));
    Report { deviations: config.deviations, files: files.len(), results, file_failures }
}

/// Every Osaka test the file at `path` defines, and what happened to each; or why the file could
/// not be read.
pub fn run_file(path: &Path, config: Config) -> Result<Vec<TestResult>, Failure> {
    let path_str = path.to_string_lossy().into_owned();
    let json = std::fs::read_to_string(path)
        .map_err(|error| Failure::new(FailureKind::Fixture, None, format!("read: {error}")))?;
    let suite = serde_json::from_str::<Suite<'_>>(&json)
        .map_err(|error| Failure::new(FailureKind::Fixture, None, format!("parse: {error}")))?;

    let mut results = Vec::new();
    for (name, raw) in &suite.0 {
        let network = serde_json::from_str::<Network>(raw.get()).map_err(|error| {
            Failure::new(FailureKind::Fixture, None, format!("{name}: network: {error}"))
        })?;
        if network.network != NETWORK {
            continue;
        }
        let id = TestId { path: path_str.clone(), name: name.clone() };
        let (outcome, blocks) = match serde_json::from_str::<Test>(raw.get()) {
            Ok(test) => catch_unwind(AssertUnwindSafe(|| run_test(&path_str, name, &test)))
                .unwrap_or_else(|panic| {
                    let failure = Failure::new(FailureKind::Panic, None, panic_message(&panic));
                    (Outcome::Failed(failure), Blocks::default())
                }),
            Err(error) => {
                let failure = Failure::new(FailureKind::Fixture, None, format!("parse: {error}"));
                (Outcome::Failed(failure), Blocks::default())
            }
        };
        let outcome = attribute(config.deviations, &id, outcome);
        if config.json_outcome {
            print_outcome(&id, &outcome);
        }
        results.push(TestResult { id, outcome, blocks });
    }
    Ok(results)
}

/// Names the deviation that explains a failure of the test `id`.
fn attribute(registry: &'static [Deviation], id: &TestId, outcome: Outcome) -> Outcome {
    match outcome {
        Outcome::Failed(mut failure) => {
            failure.deviation = registry
                .iter()
                .find(|deviation| deviation.explains_blockchain(id, failure.produced))
                .map(|deviation| deviation.id);
            Outcome::Failed(failure)
        }
        outcome => outcome,
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".into())
}

fn print_outcome(id: &TestId, outcome: &Outcome) {
    let line = serde_json::json!({ "path": id.path, "test": id.name, "result": outcome });
    eprintln!("{line}");
}

/// Imports the test named `name`, in the fixture file at `path`, and judges it against its
/// fixture, unless it is skipped.
fn run_test(path: &str, name: &str, test: &Test) -> (Outcome, Blocks) {
    if let Some(reason) = skips::skip_test(path, name, test) {
        return (Outcome::Skipped { reason }, Blocks::default());
    }
    let mut blocks = Blocks::default();
    let outcome = match import_test(test, &mut blocks) {
        Ok(()) => Outcome::Passed,
        Err(failure) => Outcome::Failed(failure),
    };
    (outcome, blocks)
}

/// Holds what the executor produced for the accepted block at `index` to its header: the gas
/// used, the logs bloom, the receipts root and the state root. A mismatch is the first of them
/// that differs, and its detail names every one that does.
fn compare(index: usize, header: &Header, output: &BlockOutput) -> Result<(), Failure> {
    let mismatches = [
        (
            FailureKind::GasUsedMismatch,
            output.gas_used != header.gas_used,
            format!("gas used {}, header {}", output.gas_used, header.gas_used),
        ),
        (
            FailureKind::LogsBloomMismatch,
            output.logs_bloom != header.logs_bloom,
            format!("logs bloom {}, header {}", output.logs_bloom, header.logs_bloom),
        ),
        (
            FailureKind::ReceiptsRootMismatch,
            output.receipts_root != header.receipts_root,
            format!("receipts root {}, header {}", output.receipts_root, header.receipts_root),
        ),
        (
            FailureKind::StateRootMismatch,
            output.state_root != header.state_root,
            format!("state root {}, header {}", output.state_root, header.state_root),
        ),
    ];
    let failed: Vec<_> = mismatches.iter().filter(|(_, differs, _)| *differs).collect();
    let Some((kind, _, _)) = failed.first() else { return Ok(()) };
    let details: Vec<_> = failed.iter().map(|(_, _, detail)| detail.as_str()).collect();
    Err(Failure {
        kind: *kind,
        block: Some(index),
        produced: Some(Produced {
            block: index,
            gas_used: output.gas_used,
            receipts_root: output.receipts_root,
            state_root: output.state_root,
        }),
        detail: format!("block {index}: {}", details.join("; ")),
        deviation: None,
    })
}

/// Imports every block of `test`, counting in `blocks` those imported as the fixture expects, and
/// checks where the chain ends.
fn import_test(test: &Test, blocks: &mut Blocks) -> Result<(), Failure> {
    let fixture = |detail: String| Failure::new(FailureKind::Fixture, None, detail);
    let chain_id = u64::try_from(test.config.chainid)
        .map_err(|_| fixture(format!("chain id {} does not fit a u64", test.config.chainid)))?;
    let fraction = test
        .config
        .blob_schedule
        .get(NETWORK)
        .ok_or_else(|| fixture(format!("no {NETWORK} blob schedule")))?
        .base_fee_update_fraction;
    let fraction = u64::try_from(fraction)
        .map_err(|_| fixture(format!("blob base fee update fraction {fraction} does not fit")))?;
    let genesis =
        decode_block(&test.genesis_rlp).map_err(|error| fixture(format!("genesis: {error}")))?;
    let genesis_id = (genesis.header.number, genesis.header.hash_slow());
    let mut chain = Chain::new(&test.pre, genesis_id, chain_id, fraction)
        .map_err(|error| fixture(format!("pre-state: {error}")))?;
    // The pre-state is the genesis block's: nothing has run yet, so nothing Satin adds is there.
    let root = chain.state_root();
    if root != genesis.header.state_root {
        return Err(fixture(format!(
            "the pre-state's root is {root}, the genesis block's {}",
            genesis.header.state_root
        )));
    }

    for (index, block) in test.blocks.iter().enumerate() {
        let decoded = decode_block(&block.rlp).map_err(|error| {
            Failure::new(FailureKind::Fixture, Some(index), format!("block {index}: {error}"))
        })?;
        let header = &decoded.header;
        let imported = chain.import(&decoded);
        match (&block.expect_exception, imported) {
            (None, Ok(output)) => {
                compare(index, header, &output)?;
                blocks.accepted += 1;
            }
            (None, Err(refusal)) => {
                return Err(Failure::new(
                    FailureKind::UnexpectedException,
                    Some(index),
                    format!("block {index} refused: {}", refusal.detail),
                ))
            }
            (Some(expected), Ok(_)) => {
                return Err(Failure::new(
                    FailureKind::MissingException,
                    Some(index),
                    format!("block {index}: expected {expected}, the executor accepted it"),
                ))
            }
            (Some(expected), Err(refusal)) => match check_names(expected, refusal.names) {
                Ok(()) => blocks.refused += 1,
                Err(Mismatch::Wrong { got }) => {
                    return Err(Failure::new(
                        FailureKind::WrongException,
                        Some(index),
                        format!(
                            "block {index}: expected {expected}, refused as {got:?}: {}",
                            refusal.detail
                        ),
                    ))
                }
                Err(Mismatch::Unnamed) => {
                    return Err(Failure::new(
                        FailureKind::UnnamedException,
                        Some(index),
                        format!("block {index}: expected {expected}, refused: {}", refusal.detail),
                    ))
                }
            },
        }
    }

    // The chain ends at the fixture's last valid block, holding the fixture's post-state.
    let end = |detail: String| Failure::new(FailureKind::PostStateMismatch, None, detail);
    if chain.head != test.lastblockhash {
        return Err(end(format!(
            "the chain ends at block {}, the fixture's last block is {}",
            chain.head, test.lastblockhash
        )));
    }
    let post_root = chain::fixture_state_root(&test.post_state)
        .map_err(|error| fixture(format!("post-state: {error}")))?;
    let root = chain.state_root();
    if root != post_root {
        return Err(end(format!(
            "the chain ends with state root {root}, the fixture's post-state has {post_root}"
        )));
    }
    Ok(())
}
