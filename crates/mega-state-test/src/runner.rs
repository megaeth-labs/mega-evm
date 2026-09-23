//! Finding, executing and judging state tests.
//!
//! [`run`] executes every entry the fixtures under a set of paths define for one [`Fork`], in one
//! [`Mode`], and reports each: passed, skipped (with the [reason](SkipReason) the reference
//! runner shares) or failed (with the [kind](FailureKind) of failure and, in equivalence mode,
//! the [deviation](crate::deviations::Deviation) that explains it, if one does).
//! [`Report::summary`] counts them, and [`Summary::gate`] is what the gate checks.
//!
//! A test is judged the way the reference runner judges it — the post-state root and the logs
//! hash for a transaction that executes, the exception for one that must not — and more strictly
//! where that runner is lenient:
//!
//! - an expected exception must be the one the fixture names ([`exceptions`]), not any error, and
//!   the state it leaves must be the fixture's post-state;
//! - an expected output must be produced, not merely not contradicted;
//! - a fixture value that does not fit its width is a failure, not a value clamped to fit.
//!
//! Base fees go to Optimism's base-fee vault on Satin, where Ethereum burns them. The runner takes
//! the vault back out of the post-state before computing its root, but only the account the fee
//! routing created: one the fixture's pre-state does not hold, holding exactly the base fee of the
//! gas the transaction used and nothing else. Any other vault stays, and the root it moves is a
//! failure.

use std::{
    collections::BTreeMap,
    fmt,
    io::stderr,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

use mega_evm::{
    alloy_op_evm::OpTx,
    op_revm::{constants::BASE_FEE_RECIPIENT, OpHaltReason, OpTransaction, OpTransactionError},
    revm::{
        context::{
            result::{EVMError, ExecutionResult},
            CfgEnv,
        },
        database::{EmptyDB, State},
        inspector::{inspectors::TracerEip3155, InspectCommitEvm},
        primitives::{Bytes, KECCAK_EMPTY, U256},
        ExecuteCommitEvm,
    },
};
use serde::Serialize;
use walkdir::WalkDir;

use crate::{
    deviations,
    exceptions::{self, Mismatch},
    mode::MAX_BLOBS_PER_TX,
    roots::{logs_hash, state_root},
    skips::{skip_file, SkipReason},
    types::{Test, TestSuite, TestUnit},
    Fork, Mode,
};

/// How a run executes its fixtures.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The configuration each test runs under.
    pub mode: Mode,
    /// The fork whose entries run; every other entry is not part of the run.
    pub fork: Fork,
    /// Worker threads; one runs the files in order.
    pub threads: usize,
    /// Print one JSON line per test to standard error.
    pub json_outcome: bool,
    /// Run each test under an EIP-3155 tracer writing to standard error.
    pub trace: bool,
}

/// Which test a result is about.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct TestId {
    /// The fixture file, as it was found under the paths the run was given.
    pub path: String,
    /// The test's name within the file.
    pub name: String,
    /// The position of the entry within the fork's entries for the test.
    pub entry: usize,
    /// The entry's data, gas and value indices.
    pub indexes: (usize, usize, usize),
}

/// How a test failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// The transaction was rejected, and the fixture expects it to execute.
    UnexpectedException,
    /// The transaction executed, and the fixture expects it to be rejected.
    MissingException,
    /// The transaction was rejected for another reason than the one the fixture names.
    WrongException,
    /// The transaction was rejected with an error no execution-spec exception names.
    UnnamedException,
    /// The transaction's output is not the one the fixture expects.
    OutputMismatch,
    /// The hash of the logs is not the fixture's.
    LogsMismatch,
    /// The post-state root is not the fixture's.
    StateRootMismatch,
    /// The fixture could not be read, parsed or turned into a transaction.
    Fixture,
    /// Executing the test panicked.
    Panic,
}

impl FailureKind {
    /// The kind's name, as the summary prints it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::UnexpectedException => "unexpected-exception",
            Self::MissingException => "missing-exception",
            Self::WrongException => "wrong-exception",
            Self::UnnamedException => "unnamed-exception",
            Self::OutputMismatch => "output-mismatch",
            Self::LogsMismatch => "logs-mismatch",
            Self::StateRootMismatch => "state-root-mismatch",
            Self::Fixture => "fixture",
            Self::Panic => "panic",
        }
    }
}

/// A failed test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// How it failed.
    pub kind: FailureKind,
    /// What was expected and what happened.
    pub detail: String,
    /// The deviation that explains it, in equivalence mode, if one does.
    pub deviation: Option<&'static str>,
}

impl Failure {
    fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into(), deviation: None }
    }
}

/// What happened to a test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    /// It executed and matched its fixture.
    Passed,
    /// It was not executed.
    Skipped {
        /// Why not.
        reason: SkipReason,
    },
    /// It executed, or failed to, and did not match its fixture.
    Failed(Failure),
}

/// A test and what happened to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TestResult {
    /// The test.
    pub id: TestId,
    /// What happened to it.
    pub outcome: Outcome,
}

/// Every test a run was given, and what happened to each.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The configuration the tests ran under.
    pub mode: Mode,
    /// The fork whose entries ran.
    pub fork: Fork,
    /// The fixture files the run found.
    pub files: usize,
    /// Every test, ordered by file, name and entry.
    pub results: Vec<TestResult>,
    /// The fixture files that could not be read or parsed, in path order, and why.
    pub file_failures: Vec<(String, Failure)>,
}

/// The counts of a [`Report`].
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// The fixture files the run found.
    pub files: usize,
    /// The entries the files define for the fork: executed and skipped.
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
    /// The fixture files that could not be read or parsed. No deviation explains one, and the
    /// entries such a file defines are counted nowhere else.
    pub file_failures: usize,
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

    /// The tests a deviation explains.
    pub fn deviated_total(&self) -> usize {
        self.deviated.values().sum()
    }

    /// What equivalence mode's gate finds wrong with this run: an unattributed failure, and, for
    /// each count given, a count that differs from it. Empty when the gate passes.
    ///
    /// With `check_deviations`, every registered deviation's count must equal its pin for `fork`
    /// — a deviation that explains fewer failures than it pins is as much a change as one that
    /// explains more.
    pub fn gate(
        &self,
        fork: Fork,
        expected_executed: Option<usize>,
        expected_skipped: Option<usize>,
        check_deviations: bool,
    ) -> Vec<String> {
        let mut problems = Vec::new();
        if self.file_failures > 0 {
            problems.push(format!("{} fixture files could not be read", self.file_failures));
        }
        if self.unattributed > 0 {
            problems.push(format!("{} failed tests no deviation explains", self.unattributed));
        }
        if let Some(expected) = expected_executed.filter(|&n| n != self.executed) {
            problems.push(format!("{} tests executed, {expected} pinned", self.executed));
        }
        let skipped = self.skipped_total();
        if let Some(expected) = expected_skipped.filter(|&n| n != skipped) {
            problems.push(format!("{skipped} tests skipped, {expected} pinned"));
        }
        if check_deviations {
            for deviation in deviations::DEVIATIONS {
                let got = self.deviated.get(deviation.id).copied().unwrap_or(0);
                let pinned = deviation.pinned(fork);
                if got != pinned {
                    problems.push(format!(
                        "deviation {} explains {got} failed tests on {fork}, {pinned} pinned",
                        deviation.id
                    ));
                }
            }
        }
        problems
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
        summary
    }

    /// The failed tests no deviation explains.
    pub fn unattributed(&self) -> impl Iterator<Item = (&TestId, &Failure)> {
        self.results.iter().filter_map(|result| match &result.outcome {
            Outcome::Failed(failure) if failure.deviation.is_none() => Some((&result.id, failure)),
            _ => None,
        })
    }
}

impl fmt::Display for TestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (d, g, v) = self.indexes;
        write!(f, "{} :: {} [d={d} g={g} v={v}]", self.path, self.name)
    }
}

/// Every JSON file under `path`, or `path` itself when it is a file, in path order.
pub fn find_json_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut files: Vec<_> = WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    files
}

/// Executes every entry `files` define for `config.fork`, in `config.mode`, and reports each.
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
    Report { mode: config.mode, fork: config.fork, files: files.len(), results, file_failures }
}

/// Every entry the file at `path` defines for `config.fork`, and what happened to each; or why
/// the file could not be read.
pub fn run_file(path: &Path, config: Config) -> Result<Vec<TestResult>, Failure> {
    let path_str = path.to_string_lossy().into_owned();
    let json = std::fs::read_to_string(path)
        .map_err(|error| Failure::new(FailureKind::Fixture, format!("read: {error}")))?;
    let suite: TestSuite = serde_json::from_str(&json)
        .map_err(|error| Failure::new(FailureKind::Fixture, format!("parse: {error}")))?;
    let skip = skip_file(&path_str);

    let mut results = Vec::new();
    for (name, unit) in &suite.0 {
        for (spec, tests) in &unit.post {
            if !config.fork.is(spec) {
                continue;
            }
            for (entry, test) in tests.iter().enumerate() {
                let id = TestId {
                    path: path_str.clone(),
                    name: name.clone(),
                    entry,
                    indexes: (test.indexes.data, test.indexes.gas, test.indexes.value),
                };
                let outcome = match skip {
                    Some(reason) => Outcome::Skipped { reason },
                    None => catch_unwind(AssertUnwindSafe(|| run_test(config, unit, test)))
                        .unwrap_or_else(|panic| {
                            Outcome::Failed(Failure::new(FailureKind::Panic, panic_message(&panic)))
                        }),
                };
                let outcome = attribute(config, &path_str, outcome);
                if config.json_outcome {
                    print_outcome(config, &id, &outcome);
                }
                results.push(TestResult { id, outcome });
            }
        }
    }
    Ok(results)
}

/// Names the deviation that explains a failure, in equivalence mode.
fn attribute(config: Config, path: &str, outcome: Outcome) -> Outcome {
    match outcome {
        Outcome::Failed(mut failure) if config.mode == Mode::Equivalence => {
            failure.deviation = deviations::attribute(config.fork, path, failure.kind)
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

fn print_outcome(config: Config, id: &TestId, outcome: &Outcome) {
    let line = serde_json::json!({
        "mode": config.mode,
        "fork": config.fork,
        "path": id.path,
        "test": id.name,
        "d": id.indexes.0,
        "g": id.indexes.1,
        "v": id.indexes.2,
        "result": outcome,
    });
    eprintln!("{line}");
}

/// The chain id `unit` runs on: its own, or mainnet's when it names none, as the reference runner
/// has it. One that does not fit a `u64` is a fixture error rather than a value clamped to fit.
fn chain_id(unit: &TestUnit) -> Result<u64, Failure> {
    let id = unit.env.current_chain_id.unwrap_or(U256::ONE);
    id.try_into().map_err(|_| {
        Failure::new(FailureKind::Fixture, format!("currentChainID {id} does not fit a u64"))
    })
}

/// Executes one entry and judges it against its fixture.
fn run_test(config: Config, unit: &TestUnit, test: &Test) -> Outcome {
    match execute_and_check(config, unit, test) {
        Ok(None) => Outcome::Passed,
        Ok(Some(reason)) => Outcome::Skipped { reason },
        Err(failure) => Outcome::Failed(failure),
    }
}

fn execute_and_check(
    config: Config,
    unit: &TestUnit,
    test: &Test,
) -> Result<Option<SkipReason>, Failure> {
    let chain_id = chain_id(unit)?;

    // The block is built the way the reference runner builds it, from the fork's Ethereum
    // configuration: the blob base fee fraction and the default `prevrandao` come from there.
    let mut eth_cfg = CfgEnv::new();
    eth_cfg.chain_id = chain_id;
    eth_cfg.set_spec_and_mainnet_gas_params(config.fork.spec_id());
    eth_cfg.set_max_blobs_per_tx(MAX_BLOBS_PER_TX);
    let block = unit.block_env(&mut eth_cfg);

    let tx = match test.tx_env(unit) {
        Ok(tx) => tx,
        Err(_) if test.expect_exception.is_some() => {
            return Ok(Some(SkipReason::UnbuildableInvalidTransaction))
        }
        Err(error) => {
            return Err(Failure::new(FailureKind::Fixture, format!("transaction: {error}")))
        }
    };
    let tx =
        OpTx(OpTransaction { base: tx, enveloped_tx: Some(Bytes::new()), ..Default::default() });

    let basefee = block.basefee;
    let mut state =
        State::builder().with_cached_prestate(unit.state()).with_bundle_update().build();
    let evm = config.mode.evm(config.fork, &mut state, block, chain_id);
    let result = if config.trace {
        let mut evm = evm.with_inspector(TracerEip3155::buffered(stderr()).without_summary());
        evm.inspect_tx_commit(tx)
    } else {
        let mut evm = evm;
        evm.transact_commit(tx)
    };
    if let Ok(result) = &result {
        undo_fee_vault_credit(unit, &mut state, basefee, result.tx_gas_used());
    }
    check(test, unit.out.as_ref(), &result, &state)?;
    Ok(None)
}

/// Takes back out of `state` the base-fee vault account Satin's fee routing created, where
/// Ethereum burns the fee: the fixture's pre-state has no vault, and the vault holds the base fee
/// of the `gas_used` the transaction reports and nothing else.
///
/// The L1 and operator fee vaults are credited nothing here, so they are touched and empty, and
/// EIP-161 has already removed them; one that is not stays for the root to show.
fn undo_fee_vault_credit(unit: &TestUnit, state: &mut State<EmptyDB>, basefee: u64, gas_used: u64) {
    if unit.pre.contains_key(&BASE_FEE_RECIPIENT) {
        return;
    }
    let Some(cached) = state.cache.accounts.get(&BASE_FEE_RECIPIENT) else { return };
    let fee = U256::from(basefee) * U256::from(gas_used);
    let routed_fee_only = cached.account.as_ref().is_none_or(|account| {
        account.info.balance == fee &&
            account.info.nonce == 0 &&
            account.info.code_hash == KECCAK_EMPTY &&
            account.storage.is_empty()
    });
    if routed_fee_only {
        state.cache.accounts.remove(&BASE_FEE_RECIPIENT);
    }
}

/// Judges an executed entry against its fixture.
///
/// A rejected transaction must be rejected for the reason the fixture names and must leave the
/// fixture's post-state, which is its pre-state; an executed one must produce the fixture's
/// output, logs and post-state.
fn check<DBError: fmt::Debug>(
    test: &Test,
    expected_output: Option<&Bytes>,
    result: &Result<ExecutionResult<OpHaltReason>, EVMError<DBError, OpTransactionError>>,
    state: &State<EmptyDB>,
) -> Result<(), Failure> {
    match (&test.expect_exception, result) {
        (Some(expected), Err(error)) => match exceptions::check(expected, error) {
            Ok(()) => {}
            Err(Mismatch::Wrong { got }) => {
                return Err(Failure::new(
                    FailureKind::WrongException,
                    format!("expected {expected}, rejected as {got:?}: {error:?}"),
                ))
            }
            Err(Mismatch::Unnamed) => {
                return Err(Failure::new(
                    FailureKind::UnnamedException,
                    format!("expected {expected}, rejected with {error:?}"),
                ))
            }
        },
        (Some(expected), Ok(result)) => {
            return Err(Failure::new(
                FailureKind::MissingException,
                format!("expected {expected}, executed: {}", describe(result)),
            ))
        }
        (None, Err(error)) => {
            return Err(Failure::new(FailureKind::UnexpectedException, format!("{error:?}")))
        }
        (None, Ok(result)) => check_output(expected_output, result)?,
    }
    let logs: &[_] = result.as_ref().map(ExecutionResult::logs).unwrap_or_default();
    let logs_hash = logs_hash(logs);
    if logs_hash != test.logs {
        return Err(Failure::new(
            FailureKind::LogsMismatch,
            format!("logs hash {logs_hash}, expected {}", test.logs),
        ));
    }
    let root = state_root(state.cache.trie_account());
    if root != test.hash {
        let executed = result.as_ref().map(|r| format!("; {}", describe(r))).unwrap_or_default();
        return Err(Failure::new(
            FailureKind::StateRootMismatch,
            format!("state root {root}, expected {}{executed}", test.hash),
        ));
    }
    Ok(())
}

/// An expected output must be produced: a halt, which produces none, does not satisfy one.
fn check_output(
    expected: Option<&Bytes>,
    result: &ExecutionResult<OpHaltReason>,
) -> Result<(), Failure> {
    let Some(expected) = expected else { return Ok(()) };
    if result.output() != Some(expected) {
        return Err(Failure::new(
            FailureKind::OutputMismatch,
            format!("output {:?}, expected {expected}", result.output()),
        ));
    }
    Ok(())
}

fn describe(result: &ExecutionResult<OpHaltReason>) -> String {
    let status = match result {
        ExecutionResult::Success { reason, .. } => format!("success ({reason:?})"),
        ExecutionResult::Revert { .. } => "revert".into(),
        ExecutionResult::Halt { reason, .. } => format!("halt ({reason:?})"),
    };
    format!("{status}, gas used {}", result.tx_gas_used())
}
