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
//! A [deviation](crate::deviations) the state-test registry already has may list a test, block by
//! block: an accepted block whose outcome is not its header's passes only when the test's entry
//! lists that block with exactly the outcome Satin produces — its gas used, logs bloom, receipts
//! root and state root — and the chain is imported on from Satin's own state, so every later block
//! is held to its header or to its own listed outcome in turn. The chain's head is always
//! compared; its post-state is compared when the chain ends on a state its last header
//! describes, and otherwise only when the entry says why it is not.
//!
//! Each test is passed, deviated (every block matching its header or its listed outcome, and the
//! chain ending as listed), skipped for a [reason](SkipReason) decided from its content before
//! anything runs, or failed; [`Report::gate`] fails on a failed test. The summary counts the
//! blocks checked against Ethereum apart from the blocks matched to a deviation.
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
use mega_evm::{
    alloy_consensus::Header,
    revm::primitives::{keccak256, B256},
};
use serde::Serialize;
pub use skips::SkipReason;

use self::{
    chain::{BlockOutput, Chain},
    fixture::{decode_block, Network, Suite, Test},
};
use crate::{
    deviations::{self, BlockchainEntry, Cause, ChainEnd, Deviation},
    exceptions::{check_names, Mismatch},
};

/// How a run executes its fixtures.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Worker threads; one runs the files in order.
    pub threads: usize,
    /// Print one JSON line per test to standard error.
    pub json_outcome: bool,
    /// The registry whose entries explain blocks that differ:
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
    /// The test is listed by a deviation, and does not deviate as listed: a listed block matched
    /// its header or produced another outcome, a block names a cause that does not hold, or the
    /// chain does not end as listed.
    NotAsListed,
    /// The fixture could not be read, parsed or decoded.
    Fixture,
    /// Running the test panicked.
    Panic,
}

impl FailureKind {
    /// Every kind, in the order a block's comparisons are made.
    pub const ALL: [Self; 12] = [
        Self::UnexpectedException,
        Self::MissingException,
        Self::WrongException,
        Self::UnnamedException,
        Self::GasUsedMismatch,
        Self::LogsBloomMismatch,
        Self::ReceiptsRootMismatch,
        Self::StateRootMismatch,
        Self::PostStateMismatch,
        Self::NotAsListed,
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
            Self::NotAsListed => "not-as-listed",
            Self::Fixture => "fixture",
            Self::Panic => "panic",
        }
    }
}

/// What Satin produces for an accepted block: the block, and the gas used, logs bloom, receipts
/// root and state root it produced — every field a block is held to.
///
/// The bloom is kept as its keccak hash, which pins its 256 bytes in 32: the receipts root commits
/// to each receipt's bloom, not to the block's, which the runner aggregates on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Produced {
    /// The block's index among the test's blocks.
    pub block: usize,
    /// The gas the block used.
    pub gas_used: u64,
    /// The keccak hash of the block's logs bloom.
    pub logs_bloom_hash: B256,
    /// The root of the block's receipts.
    pub receipts_root: B256,
    /// The root of the state the block left, the accounts Satin added taken out.
    pub state_root: B256,
}

impl fmt::Display for Produced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "block {}: gas used {}, logs bloom hash {}, receipts root {}, state root {}",
            self.block, self.gas_used, self.logs_bloom_hash, self.receipts_root, self.state_root
        )
    }
}

/// An accepted block whose outcome is not its header's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Differing {
    /// What Satin produced for it.
    pub produced: Produced,
    /// Whether its state root is the only field that differs from its header.
    pub state_root_only: bool,
    /// Whether the test's entry lists the block with exactly this outcome, and its cause holds.
    pub listed: bool,
}

/// A failed test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// How it failed.
    pub kind: FailureKind,
    /// The index of the block it failed at, when it failed at one.
    pub block: Option<usize>,
    /// What was expected and what happened.
    pub detail: String,
    /// Every accepted block the test imported whose outcome is not its header's, listed or not:
    /// what an entry for the test lists.
    pub differing: Vec<Differing>,
}

impl Failure {
    fn new(kind: FailureKind, block: Option<usize>, detail: impl Into<String>) -> Self {
        Self { kind, block, detail: detail.into(), differing: Vec::new() }
    }
}

/// What happened to a test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    /// It was imported, and every block and the chain's end matched its fixture.
    Passed,
    /// It was imported, every block matched its header or the outcome its deviation lists for it,
    /// and the chain ended as listed.
    Deviated {
        /// The deviation that lists it.
        deviation: &'static str,
    },
    /// It was not executed.
    Skipped {
        /// Why not.
        reason: SkipReason,
    },
    /// It was imported, or failed to be, and did not match its fixture or its listed outcome.
    Failed(Failure),
}

/// The blocks of a test the runner imported, by how each was judged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Blocks {
    /// Blocks the executor accepted that matched their headers: checked against Ethereum.
    pub matched: usize,
    /// Blocks the executor refused for an exception the fixture names: checked against Ethereum.
    pub refused: usize,
    /// Blocks the executor accepted whose outcome is not their header's and is exactly the one a
    /// deviation lists: matched to a deviation, not to Ethereum.
    pub deviated: usize,
}

impl Blocks {
    /// The blocks checked against Ethereum: matched or refused as the fixture expects.
    pub const fn checked(&self) -> usize {
        self.matched + self.refused
    }
}

/// A test and what happened to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TestResult {
    /// The test.
    pub id: TestId,
    /// What happened to it.
    pub outcome: Outcome,
    /// The blocks it imported, up to a failure that stopped it.
    pub blocks: Blocks,
}

/// Every test a run was given, and what happened to each.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The registry the run held differing blocks to.
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
    /// The tests that executed: passed, deviated or failed.
    pub executed: usize,
    /// The tests that executed and matched their fixture in every block and at the chain's end.
    pub passed: usize,
    /// The tests that were not executed, by reason.
    pub skipped: BTreeMap<SkipReason, usize>,
    /// The tests that failed, by kind.
    pub failed: BTreeMap<FailureKind, usize>,
    /// The tests a deviation lists that deviated exactly as listed, by deviation.
    pub deviated: BTreeMap<&'static str, usize>,
    /// The failed tests: no listed outcome explains them.
    pub unattributed: usize,
    /// The tests a deviation lists that did not deviate as listed, by deviation.
    pub unreproduced: BTreeMap<&'static str, usize>,
    /// The fixture files that could not be read or parsed.
    pub file_failures: usize,
    /// The blocks the executed tests imported: checked against Ethereum (matched or refused) and
    /// matched to a deviation, counted apart.
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

    /// The tests that deviated as listed.
    pub fn deviated_total(&self) -> usize {
        self.deviated.values().sum()
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
            summary.blocks.matched += result.blocks.matched;
            summary.blocks.refused += result.blocks.refused;
            summary.blocks.deviated += result.blocks.deviated;
            match &result.outcome {
                Outcome::Passed => {
                    summary.executed += 1;
                    summary.passed += 1;
                }
                Outcome::Deviated { deviation } => {
                    summary.executed += 1;
                    *summary.deviated.entry(deviation).or_default() += 1;
                }
                Outcome::Skipped { reason } => *summary.skipped.entry(*reason).or_default() += 1,
                Outcome::Failed(failure) => {
                    summary.executed += 1;
                    summary.unattributed += 1;
                    *summary.failed.entry(failure.kind).or_default() += 1;
                }
            }
        }
        for unreproduced in self.unreproduced() {
            *summary.unreproduced.entry(unreproduced.deviation.id).or_default() += 1;
        }
        summary
    }

    /// The failed tests: no listed outcome explains them.
    pub fn unattributed(&self) -> impl Iterator<Item = (&TestId, &Failure)> {
        self.results.iter().filter_map(|result| match &result.outcome {
            Outcome::Failed(failure) => Some((&result.id, failure)),
            _ => None,
        })
    }

    /// The tests the registry lists that did not deviate as listed: a test is reproduced when
    /// exactly one result of the run is the test's and it deviated under the deviation that lists
    /// it.
    pub fn unreproduced(&self) -> impl Iterator<Item = Unreproduced<'_>> {
        self.deviations.iter().flat_map(move |deviation| {
            deviation.blockchain_entries.iter().filter_map(move |entry| {
                let seen: Vec<_> =
                    self.results.iter().filter(|result| entry.is(&result.id)).collect();
                let reproduced = matches!(
                    seen.as_slice(),
                    [TestResult { outcome: Outcome::Deviated { deviation: id }, .. }]
                        if *id == deviation.id
                );
                (!reproduced).then_some(Unreproduced { deviation, entry, seen })
            })
        })
    }

    /// What the gate finds wrong with this run: a fixture file that could not be read, a failed
    /// test, and, for each count given, a count that differs from it. Empty when the gate passes.
    ///
    /// `expected_skipped` pins every class: a class it does not name is pinned at zero. With
    /// `check_deviations`, every test a registered deviation lists must deviate exactly as listed.
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
            problems.push(format!(
                "{} blockchain tests failed: no listed outcome explains them",
                summary.unattributed
            ));
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
                         did not deviate as listed",
                        deviation.id
                    ));
                }
            }
        }
        problems
    }
}

/// A test a deviation lists that did not deviate as listed.
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
            Ok(test) => catch_unwind(AssertUnwindSafe(|| {
                run_test(&id, &test, deviations::blockchain_entry(config.deviations, &id))
            }))
            .unwrap_or_else(|panic| {
                let failure = Failure::new(FailureKind::Panic, None, panic_message(&panic));
                (Outcome::Failed(failure), Blocks::default())
            }),
            Err(error) => {
                let failure = Failure::new(FailureKind::Fixture, None, format!("parse: {error}"));
                (Outcome::Failed(failure), Blocks::default())
            }
        };
        if config.json_outcome {
            print_outcome(&id, &outcome);
        }
        results.push(TestResult { id, outcome, blocks });
    }
    Ok(results)
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

/// Imports the test `id` and judges it against its fixture and the entry `listed`, the deviation
/// that lists it and its entry for it, unless it is skipped.
fn run_test(
    id: &TestId,
    test: &Test,
    listed: Option<(&'static Deviation, &'static BlockchainEntry)>,
) -> (Outcome, Blocks) {
    if let Some(reason) = skips::skip_test(&id.path, &id.name, test) {
        return (Outcome::Skipped { reason }, Blocks::default());
    }
    let mut blocks = Blocks::default();
    let outcome = match import_test(test, listed.map(|(_, entry)| entry), &mut blocks) {
        Ok(()) => match listed {
            Some((deviation, _)) => Outcome::Deviated { deviation: deviation.id },
            None => Outcome::Passed,
        },
        Err(failure) => Outcome::Failed(failure),
    };
    (outcome, blocks)
}

/// The fields of an accepted block's outcome that differ from its header.
#[derive(Clone, Copy, Debug)]
struct Differences {
    gas_used: bool,
    logs_bloom: bool,
    receipts_root: bool,
    state_root: bool,
}

impl Differences {
    fn of(header: &Header, output: &BlockOutput) -> Self {
        Self {
            gas_used: output.gas_used != header.gas_used,
            logs_bloom: output.logs_bloom != header.logs_bloom,
            receipts_root: output.receipts_root != header.receipts_root,
            state_root: output.state_root != header.state_root,
        }
    }

    const fn any(self) -> bool {
        self.gas_used || self.logs_bloom || self.receipts_root || self.state_root
    }

    /// Whether the block ran as on Ethereum and left another state: only its state root differs.
    const fn state_root_only(self) -> bool {
        self.state_root && !self.gas_used && !self.logs_bloom && !self.receipts_root
    }

    /// The first field that differs, in the order a block is compared, as a failure.
    const fn kind(self) -> FailureKind {
        if self.gas_used {
            FailureKind::GasUsedMismatch
        } else if self.logs_bloom {
            FailureKind::LogsBloomMismatch
        } else if self.receipts_root {
            FailureKind::ReceiptsRootMismatch
        } else {
            FailureKind::StateRootMismatch
        }
    }

    /// Every field that differs, with what the executor produced and what the header says.
    fn detail(self, header: &Header, output: &BlockOutput) -> String {
        let mut details = Vec::new();
        if self.gas_used {
            details.push(format!("gas used {}, header {}", output.gas_used, header.gas_used));
        }
        if self.logs_bloom {
            details.push(format!("logs bloom {}, header {}", output.logs_bloom, header.logs_bloom));
        }
        if self.receipts_root {
            details.push(format!(
                "receipts root {}, header {}",
                output.receipts_root, header.receipts_root
            ));
        }
        if self.state_root {
            details.push(format!("state root {}, header {}", output.state_root, header.state_root));
        }
        details.join("; ")
    }
}

/// What Satin produced for the accepted block at `index`.
fn produced(index: usize, output: &BlockOutput) -> Produced {
    Produced {
        block: index,
        gas_used: output.gas_used,
        logs_bloom_hash: keccak256(output.logs_bloom),
        receipts_root: output.receipts_root,
        state_root: output.state_root,
    }
}

/// Whether the cause `entry` gives for its block at `index`, which differs from its header in
/// `differences`, holds: the deviation's rule may act in any block, and a block whose state an
/// earlier listed block left must otherwise run as on Ethereum.
fn cause_holds(
    cause: Cause,
    index: usize,
    differences: Differences,
    entry: &BlockchainEntry,
) -> bool {
    match cause {
        Cause::Rule => true,
        Cause::StateLeftBy(earlier) => {
            earlier < index && entry.block(earlier).is_some() && differences.state_root_only()
        }
    }
}

/// `failure`, carrying the blocks that differed before it.
fn with_differing(mut failure: Failure, differing: &[Differing]) -> Failure {
    failure.differing = differing.to_vec();
    failure
}

/// Imports every block of `test`, counting in `blocks` how each was judged, and checks where the
/// chain ends.
///
/// An accepted block that differs from its header passes only when `entry` lists it with exactly
/// the outcome Satin produced and a cause that holds; the chain is imported on from Satin's own
/// state either way, so a failure names every block that differs, not only the first. A refusal
/// the fixture does not expect, or a block it expects refused that is accepted, stops the test.
fn import_test(
    test: &Test,
    entry: Option<&'static BlockchainEntry>,
    blocks: &mut Blocks,
) -> Result<(), Failure> {
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

    let mut differing = Vec::new();
    let mut unlisted = None;
    // Whether the chain holds the state its last accepted block's header describes.
    let mut on_fixture_state = true;
    for (index, block) in test.blocks.iter().enumerate() {
        let decoded = decode_block(&block.rlp).map_err(|error| {
            let failure =
                Failure::new(FailureKind::Fixture, Some(index), format!("block {index}: {error}"));
            with_differing(failure, &differing)
        })?;
        let header = &decoded.header;
        match (&block.expect_exception, chain.import(&decoded)) {
            (None, Ok(output)) => {
                let differences = Differences::of(header, &output);
                on_fixture_state = !differences.state_root;
                if !differences.any() {
                    blocks.matched += 1;
                    continue;
                }
                let produced = produced(index, &output);
                let listed_block = entry.and_then(|entry| Some((entry, entry.block(index)?)));
                let listed = listed_block.is_some_and(|(entry, listed)| {
                    listed.produced == produced &&
                        cause_holds(listed.cause, index, differences, entry)
                });
                differing.push(Differing {
                    produced,
                    state_root_only: differences.state_root_only(),
                    listed,
                });
                if listed {
                    blocks.deviated += 1;
                } else if unlisted.is_none() {
                    let note = if listed_block.is_some() {
                        "; its deviation lists another outcome or a cause that does not hold"
                    } else {
                        ""
                    };
                    let detail =
                        format!("block {index}: {}{note}", differences.detail(header, &output));
                    unlisted = Some(Failure::new(differences.kind(), Some(index), detail));
                }
            }
            (None, Err(refusal)) => {
                let failure = Failure::new(
                    FailureKind::UnexpectedException,
                    Some(index),
                    format!("block {index} refused: {}", refusal.detail),
                );
                return Err(with_differing(failure, &differing));
            }
            (Some(expected), Ok(_)) => {
                let failure = Failure::new(
                    FailureKind::MissingException,
                    Some(index),
                    format!("block {index}: expected {expected}, the executor accepted it"),
                );
                return Err(with_differing(failure, &differing));
            }
            (Some(expected), Err(refusal)) => {
                let failure = match check_names(expected, refusal.names) {
                    Ok(()) => {
                        blocks.refused += 1;
                        continue;
                    }
                    Err(Mismatch::Wrong { got }) => Failure::new(
                        FailureKind::WrongException,
                        Some(index),
                        format!(
                            "block {index}: expected {expected}, refused as {got:?}: {}",
                            refusal.detail
                        ),
                    ),
                    Err(Mismatch::Unnamed) => Failure::new(
                        FailureKind::UnnamedException,
                        Some(index),
                        format!("block {index}: expected {expected}, refused: {}", refusal.detail),
                    ),
                };
                return Err(with_differing(failure, &differing));
            }
        }
    }
    if let Some(failure) = unlisted {
        return Err(with_differing(failure, &differing));
    }

    // Every block the entry lists deviated as listed.
    let not_as_listed = |block, detail: String| {
        with_differing(Failure::new(FailureKind::NotAsListed, block, detail), &differing)
    };
    for listed in entry.map_or(&[][..], |entry| entry.blocks) {
        let index = listed.produced.block;
        if !differing.iter().any(|block| block.listed && block.produced.block == index) {
            return Err(not_as_listed(
                Some(index),
                format!("block {index} is listed, and the run did not produce its listed outcome"),
            ));
        }
    }

    // The chain ends at the fixture's last valid block, holding the fixture's post-state, unless
    // its entry says why that state is not the fixture's.
    if chain.head != test.lastblockhash {
        let detail = format!(
            "the chain ends at block {}, the fixture's last block is {}",
            chain.head, test.lastblockhash
        );
        return Err(with_differing(
            Failure::new(FailureKind::PostStateMismatch, None, detail),
            &differing,
        ));
    }
    match (on_fixture_state, entry.map(|entry| entry.end)) {
        (true, None | Some(ChainEnd::FixtureState)) => {
            let post_root = chain::fixture_state_root(&test.post_state)
                .map_err(|error| fixture(format!("post-state: {error}")))?;
            let root = chain.state_root();
            if root != post_root {
                let detail = format!(
                    "the chain ends with state root {root}, the fixture's post-state has \
                     {post_root}"
                );
                return Err(with_differing(
                    Failure::new(FailureKind::PostStateMismatch, None, detail),
                    &differing,
                ));
            }
        }
        (false, Some(ChainEnd::DeviatedState(_))) => {}
        (true, Some(ChainEnd::DeviatedState(_))) => {
            return Err(not_as_listed(
                None,
                "the entry says the chain ends on a state a listed block left, and it ends on \
                 the state its last header describes"
                    .into(),
            ))
        }
        (false, None | Some(ChainEnd::FixtureState)) => {
            return Err(not_as_listed(
                None,
                "the chain ends on a state a listed block left, and its entry gives no reason \
                 the post-state is not compared"
                    .into(),
            ))
        }
    }
    Ok(())
}
