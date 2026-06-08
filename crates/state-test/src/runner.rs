#![allow(missing_docs)]

use crate::{
    types::{
        tx_env_at, SpecName, Test, TestError as TxBuildError, TestSuite, TestUnit, TxPartIndices,
    },
    utils::{compute_test_roots, TestValidationResult},
};
use alloy_primitives::{address, U256};
use indicatif::{ProgressBar, ProgressDrawTarget};
use mega_evm::{
    revm::{
        context::{block::BlockEnv, cfg::CfgEnv, tx::TxEnv},
        context_interface::{
            result::{EVMError, ExecutionResult},
            Cfg,
        },
        database,
        database::State,
        database_interface::EmptyDB,
        inspector::{inspectors::TracerEip3155, InspectCommitEvm},
        primitives::{hardfork::SpecId, Bytes, B256},
        ExecuteCommitEvm,
    },
    AHashBucketHasher, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};
use serde_json::json;
use std::{
    convert::Infallible,
    fmt::Debug,
    io::stderr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use walkdir::{DirEntry, WalkDir};

/// Error that occurs during test execution
#[derive(Debug, Error)]
#[error("Path: {path}\nName: {name}\nError: {kind}")]
pub struct TestError {
    pub name: String,
    pub path: String,
    pub kind: TestErrorKind,
}

/// Specific kind of error that occurred during test execution
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum TestErrorKind {
    #[error("logs root mismatch: got {got}, expected {expected}")]
    LogsRootMismatch { got: B256, expected: B256 },
    #[error("state root mismatch: got {got}, expected {expected}")]
    StateRootMismatch { got: B256, expected: B256 },
    #[error("gas used mismatch: got {got}, expected {expected}")]
    GasUsedMismatch { got: u64, expected: u64 },
    #[error("status mismatch: got {got:?}, expected {expected:?}")]
    StatusMismatch { got: String, expected: String },
    #[error("unknown private key: {0:?}")]
    UnknownPrivateKey(B256),
    #[error("unexpected exception: got {got_exception:?}, expected {expected_exception:?}")]
    UnexpectedException { expected_exception: Option<String>, got_exception: Option<String> },
    #[error("unexpected output: got {got_output:?}, expected {expected_output:?}")]
    UnexpectedOutput { expected_output: Option<Bytes>, got_output: Option<Bytes> },
    #[error(transparent)]
    SerdeDeserialize(#[from] serde_json::Error),
    #[error("thread panicked")]
    Panic,
    #[error("path does not exist")]
    InvalidPath,
    #[error("no JSON test files found in path")]
    NoJsonFiles,
    #[error("fixture execution error: {0}")]
    FixtureError(String),
}

/// Find all JSON test files in the given path
/// If path is a file, returns it in a vector
/// If path is a directory, recursively finds all .json files
pub fn find_all_json_tests(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        vec![path.to_path_buf()]
    } else {
        WalkDir::new(path)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension() == Some("json".as_ref()))
            .map(DirEntry::into_path)
            .collect()
    }
}

/// Check if a test should be skipped based on its filename
/// Some tests are known to be problematic or take too long
///
/// These tests are skipped by `revm`, so we also skip them.
fn skip_test(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_str().unwrap();

    matches!(
        name,
        // Test check if gas price overflows, we handle this correctly but does not match tests
        // specific exception.
        | "CreateTransactionHighNonce.json"

        // Test with some storage check.
        | "RevertInCreateInInit_Paris.json"
        | "RevertInCreateInInit.json"
        | "dynamicAccountOverwriteEmpty.json"
        | "dynamicAccountOverwriteEmpty_Paris.json"
        | "RevertInCreateInInitCreate2Paris.json"
        | "create2collisionStorage.json"
        | "RevertInCreateInInitCreate2.json"
        | "create2collisionStorageParis.json"
        | "InitCollision.json"
        | "InitCollisionParis.json"

        // Malformed value.
        | "ValueOverflow.json"
        | "ValueOverflowParis.json"

        // These tests are passing, but they take a lot of time to execute so we are going to skip them.
        | "Call50000_sha256.json"
        | "static_Call50000_sha256.json"
        | "loopMul.json"
        | "CALLBlake2f_MaxRounds.json"
    )
}

struct TestExecutionContext<'a> {
    name: &'a str,
    unit: &'a TestUnit,
    test: &'a Test,
    cfg: &'a CfgEnv<MegaSpecId>,
    block: &'a BlockEnv,
    tx: &'a TxEnv,
    cache_state: &'a database::CacheState,
    elapsed: &'a Arc<Mutex<Duration>>,
    trace: bool,
    print_json_outcome: bool,
}

struct DebugContext<'a> {
    name: &'a str,
    path: &'a str,
    index: usize,
    unit: &'a TestUnit,
    test: &'a Test,
    cfg: &'a CfgEnv<MegaSpecId>,
    block: &'a BlockEnv,
    tx: &'a TxEnv,
    cache_state: &'a database::CacheState,
    error: &'a TestErrorKind,
}

fn build_json_output(
    test: &Test,
    test_name: &str,
    exec_result: &Result<
        ExecutionResult<MegaHaltReason>,
        EVMError<Infallible, MegaTransactionError>,
    >,
    validation: &TestValidationResult,
    spec: MegaSpecId,
    error: Option<String>,
) -> serde_json::Value {
    json!({
        "stateRoot": validation.state_root,
        "logsRoot": validation.logs_root,
        "output": exec_result.as_ref().ok().and_then(|r| r.output().cloned()).unwrap_or_default(),
        "gasUsed": exec_result.as_ref().ok().map(|r| r.gas_used()).unwrap_or_default(),
        "pass": error.is_none(),
        "errorMsg": error.unwrap_or_default(),
        "evmResult": format_evm_result(exec_result),
        "postLogsHash": validation.logs_root,
        "fork": spec,
        "test": test_name,
        "d": test.indexes.data,
        "g": test.indexes.gas,
        "v": test.indexes.value,
    })
}

fn format_evm_result(
    exec_result: &Result<
        ExecutionResult<MegaHaltReason>,
        EVMError<Infallible, MegaTransactionError>,
    >,
) -> String {
    match exec_result {
        Ok(r) => match r {
            ExecutionResult::Success { reason, .. } => format!("Success: {reason:?}"),
            ExecutionResult::Revert { .. } => "Revert".to_string(),
            ExecutionResult::Halt { reason, .. } => format!("Halt: {reason:?}"),
        },
        Err(e) => e.to_string(),
    }
}

fn validate_exception(
    test: &Test,
    exec_result: &Result<
        ExecutionResult<MegaHaltReason>,
        EVMError<Infallible, MegaTransactionError>,
    >,
) -> Result<bool, TestErrorKind> {
    match (&test.expect_exception, exec_result) {
        (None, Ok(_)) => Ok(false), // No exception expected, execution succeeded
        (Some(_), Err(_)) => Ok(true), // Exception expected and occurred
        _ => Err(TestErrorKind::UnexpectedException {
            expected_exception: test.expect_exception.clone(),
            got_exception: exec_result.as_ref().err().map(|e| e.to_string()),
        }),
    }
}

fn validate_output(
    expected_output: Option<&Bytes>,
    actual_result: &ExecutionResult<MegaHaltReason>,
) -> Result<(), TestErrorKind> {
    if let Some((expected, actual)) = expected_output.zip(actual_result.output()) {
        if expected != actual {
            return Err(TestErrorKind::UnexpectedOutput {
                expected_output: Some(expected.clone()),
                got_output: actual_result.output().cloned(),
            });
        }
    }
    Ok(())
}

/// Canonical status string for an execution result, matching the values
/// emitted into a dumped fixture's `megaStatus` field.
fn execution_status(result: &ExecutionResult<MegaHaltReason>) -> &'static str {
    match result {
        ExecutionResult::Success { .. } => "success",
        ExecutionResult::Revert { .. } => "revert",
        ExecutionResult::Halt { .. } => "halt",
    }
}

/// Validate the MegaETH-specific explicit expectations (`megaGasUsed`,
/// `megaStatus`) when present.
///
/// These produce readable, targeted diffs for replay-derived fixtures. They are
/// in addition to — not a replacement for — the state-root / logs-root backstop,
/// and are skipped entirely for pure-Ethereum tests that omit the fields.
fn validate_mega_expectations(
    test: &Test,
    actual_result: &ExecutionResult<MegaHaltReason>,
) -> Result<(), TestErrorKind> {
    if let Some(expected) = test.mega_gas_used {
        let got = actual_result.gas_used();
        if got != expected {
            return Err(TestErrorKind::GasUsedMismatch { got, expected });
        }
    }
    if let Some(expected) = &test.mega_status {
        let got = execution_status(actual_result);
        if got != expected {
            return Err(TestErrorKind::StatusMismatch {
                got: got.to_string(),
                expected: expected.clone(),
            });
        }
    }
    Ok(())
}

fn check_evm_execution(
    test: &Test,
    expected_output: Option<&Bytes>,
    test_name: &str,
    exec_result: &Result<
        ExecutionResult<MegaHaltReason>,
        EVMError<Infallible, MegaTransactionError>,
    >,
    db: &State<EmptyDB>,
    spec: MegaSpecId,
    print_json_outcome: bool,
) -> Result<(), TestErrorKind> {
    let validation = compute_test_roots(exec_result, db);

    let print_json = |error: Option<&TestErrorKind>| {
        if print_json_outcome {
            let json = build_json_output(
                test,
                test_name,
                exec_result,
                &validation,
                spec,
                error.map(|e| e.to_string()),
            );
            eprintln!("{json}");
        }
    };

    // Check if exception handling is correct
    let exception_expected = validate_exception(test, exec_result).inspect_err(|e| {
        print_json(Some(e));
    })?;

    // If exception was expected and occurred, we're done
    if exception_expected {
        print_json(None);
        return Ok(());
    }

    // Validate output if execution succeeded
    if let Ok(result) = exec_result {
        validate_output(expected_output, result).inspect_err(|e| {
            print_json(Some(e));
        })?;

        // MegaETH explicit expectations (replay fixtures): readable gas/status diff.
        validate_mega_expectations(test, result).inspect_err(|e| {
            print_json(Some(e));
        })?;
    }

    // Validate logs root
    if validation.logs_root != test.logs {
        let error =
            TestErrorKind::LogsRootMismatch { got: validation.logs_root, expected: test.logs };
        print_json(Some(&error));
        return Err(error);
    }

    // Validate state root
    if validation.state_root != test.hash {
        let error =
            TestErrorKind::StateRootMismatch { got: validation.state_root, expected: test.hash };
        print_json(Some(&error));
        return Err(error);
    }

    print_json(None);
    Ok(())
}

/// Execute a single test suite file containing multiple tests
///
/// # Arguments
/// * `path` - Path to the JSON test file
/// * `elapsed` - Shared counter for total execution time
/// * `trace` - Whether to enable EVM tracing
/// * `print_json_outcome` - Whether to print JSON formatted results
pub fn execute_test_suite(
    path: &Path,
    elapsed: &Arc<Mutex<Duration>>,
    trace: bool,
    print_json_outcome: bool,
) -> Result<(), TestError> {
    if skip_test(path) {
        return Ok(());
    }

    let s = std::fs::read_to_string(path).unwrap();
    let path = path.to_string_lossy().into_owned();
    let suite: TestSuite = serde_json::from_str(&s).map_err(|e| TestError {
        name: "Unknown".to_string(),
        path: path.clone(),
        kind: e.into(),
    })?;

    for (name, unit) in suite.0 {
        // Prepare initial state
        let cache_state = unit.state();

        // Setup base configuration
        let mut cfg = CfgEnv::default();
        cfg.chain_id = unit
            .env
            .current_chain_id
            .unwrap_or_else(|| U256::from(6342))
            .try_into()
            .unwrap_or(6342);

        // Post and execution
        for (spec_name, tests) in &unit.post {
            // Skip Constantinople spec
            if *spec_name == SpecName::Constantinople {
                continue;
            }

            cfg.spec = spec_name.to_spec_id();

            // Configure max blobs per spec
            if cfg.spec.into_eth_spec().is_enabled_in(SpecId::OSAKA) {
                cfg.set_max_blobs_per_tx(6);
            } else if cfg.spec.into_eth_spec().is_enabled_in(SpecId::PRAGUE) {
                cfg.set_max_blobs_per_tx(9);
            } else {
                cfg.set_max_blobs_per_tx(6);
            }

            // Setup block environment for this spec
            let block = unit.block_env(&cfg);

            for (index, test) in tests.iter().enumerate() {
                // Setup transaction environment
                let tx = match test.tx_env(&unit) {
                    Ok(tx) => tx,
                    Err(_) if test.expect_exception.is_some() => continue,
                    Err(_) => {
                        return Err(TestError {
                            name,
                            path,
                            kind: TestErrorKind::UnknownPrivateKey(unit.transaction.secret_key),
                        });
                    }
                };

                // Execute the test
                let result = execute_single_test(TestExecutionContext {
                    name: &name,
                    unit: &unit,
                    test,
                    cfg: &cfg,
                    block: &block,
                    tx: &tx,
                    cache_state: &cache_state,
                    elapsed,
                    trace,
                    print_json_outcome,
                });

                if let Err(e) = result {
                    // Handle error with debug trace if needed
                    static FAILED: AtomicBool = AtomicBool::new(false);
                    if print_json_outcome || FAILED.swap(true, Ordering::SeqCst) {
                        return Err(TestError { name, path, kind: e });
                    }

                    // Re-run with trace for debugging
                    debug_failed_test(DebugContext {
                        name: &name,
                        path: &path,
                        index,
                        unit: &unit,
                        test,
                        cfg: &cfg,
                        block: &block,
                        tx: &tx,
                        cache_state: &cache_state,
                        error: &e,
                    });

                    return Err(TestError { path, name, kind: e });
                }
            }
        }
    }
    Ok(())
}

/// Build the `MegaETH` external environment for a test unit, reproducing the
/// recorded SALT bucket capacities and oracle storage. Falls back to an empty
/// environment for pure-Ethereum tests, which omit the `megaEnv` field.
///
/// Uses [`AHashBucketHasher`] so that bucket IDs match those recorded during
/// `mega-evme replay` — a different hasher would map keys to different buckets
/// and reproduce different gas.
fn external_envs_for(unit: &TestUnit) -> mega_evm::TestExternalEnvs<Infallible, AHashBucketHasher> {
    unit.mega_env.clone().unwrap_or_default().to_external_envs::<AHashBucketHasher>()
}

fn execute_single_test<'a>(ctx: TestExecutionContext<'a>) -> Result<(), TestErrorKind> {
    // Prepare state
    let mut cache = ctx.cache_state.clone();
    cache.set_state_clear_flag(ctx.cfg.spec.into_eth_spec().is_enabled_in(SpecId::SPURIOUS_DRAGON));
    let mut state =
        database::State::builder().with_cached_prestate(cache).with_bundle_update().build();

    let evm_context = MegaContext::default()
        .with_db(&mut state)
        .with_cfg(ctx.cfg.clone())
        .with_block(ctx.block.clone())
        .with_external_envs(external_envs_for(ctx.unit).into());
    let mut tx = MegaTransaction::new(ctx.tx.clone());
    tx.enveloped_tx = Some(Bytes::default());

    // Execute
    let timer = Instant::now();
    let (db, exec_result) = if ctx.trace {
        let mut evm = MegaEvm::new(evm_context)
            .with_inspector(TracerEip3155::buffered(stderr()).without_summary());
        let res = evm.inspect_tx_commit(tx);
        let db = evm.into_inner().ctx.into_inner().journaled_state.database;
        (db, res)
    } else {
        let mut evm = MegaEvm::new(evm_context);
        let res = evm.transact_commit(tx);
        let db = evm.into_inner().ctx.into_inner().journaled_state.database;
        (db, res)
    };
    *ctx.elapsed.lock().unwrap() += timer.elapsed();

    // Optimism special handling: prune the changes to BaseFeeVault
    prune_base_fee_vault_changes(db);

    // Check results
    check_evm_execution(
        ctx.test,
        ctx.unit.out.as_ref(),
        ctx.name,
        &exec_result,
        db,
        ctx.cfg.spec(),
        ctx.print_json_outcome,
    )
}

/// Canonical post-execution outcome of running a single [`TestUnit`].
///
/// Returned by [`execute_unit_collect`] and used by `mega-evme --dump-fixture`
/// to fill a fixture's `post` expectation. Because dump and validation share
/// this exact execution + root-computation path, a fixture written from these
/// values is self-consistent: re-running it through [`execute_test_suite`]
/// reproduces the same roots.
#[derive(Debug, Clone)]
pub struct ExecutedUnit {
    /// Post-state trie root over the unit's account closure.
    pub state_root: B256,
    /// RLP hash of the emitted logs.
    pub logs_root: B256,
    /// Total gas used by the transaction.
    pub gas_used: u64,
    /// Execution status: `"success"`, `"revert"`, or `"halt"`.
    pub status: String,
    /// Transaction output bytes, if any.
    pub output: Option<Bytes>,
}

/// Execute a single [`TestUnit`] at transaction index 0 for the given spec, in
/// isolation, timing only the EVM `transact` call.
///
/// This runs the same `MegaEVM` pipeline as [`execute_test_suite`] — including the
/// reproduced external environment and the Optimism `BaseFeeVault` pruning. When
/// `compute_roots` is set, the post-state / logs roots are computed (outside the
/// timed region); otherwise they are skipped for leaner repeated benchmarking.
fn run_unit_once(
    unit: &TestUnit,
    spec: &SpecName,
    compute_roots: bool,
) -> Result<(Duration, ExecutionResult<MegaHaltReason>, Option<TestValidationResult>), TestErrorKind>
{
    let mut cfg = CfgEnv::default();
    cfg.chain_id =
        unit.env.current_chain_id.unwrap_or_else(|| U256::from(6342)).try_into().unwrap_or(6342);
    cfg.spec = spec.to_spec_id();

    // Match execute_test_suite's per-spec blob configuration.
    if cfg.spec.into_eth_spec().is_enabled_in(SpecId::OSAKA) {
        cfg.set_max_blobs_per_tx(6);
    } else if cfg.spec.into_eth_spec().is_enabled_in(SpecId::PRAGUE) {
        cfg.set_max_blobs_per_tx(9);
    } else {
        cfg.set_max_blobs_per_tx(6);
    }

    let block = unit.block_env(&cfg);
    let tx = tx_env_at(unit, TxPartIndices { data: 0, gas: 0, value: 0 }).map_err(|e| match e {
        TxBuildError::UnknownPrivateKey(k) => TestErrorKind::UnknownPrivateKey(k),
        other => TestErrorKind::FixtureError(other.to_string()),
    })?;

    let mut cache = unit.state();
    cache.set_state_clear_flag(cfg.spec.into_eth_spec().is_enabled_in(SpecId::SPURIOUS_DRAGON));
    let mut state =
        database::State::builder().with_cached_prestate(cache).with_bundle_update().build();

    let evm_context = MegaContext::default()
        .with_db(&mut state)
        .with_cfg(cfg.clone())
        .with_block(block)
        .with_external_envs(external_envs_for(unit).into());
    let mut megatx = MegaTransaction::new(tx);
    megatx.enveloped_tx = Some(Bytes::default());

    let mut evm = MegaEvm::new(evm_context);
    let timer = Instant::now();
    let exec_result = evm.transact_commit(megatx);
    let elapsed = timer.elapsed();

    let db = evm.into_inner().ctx.into_inner().journaled_state.database;
    prune_base_fee_vault_changes(db);
    let validation = compute_roots.then(|| compute_test_roots(&exec_result, db));

    let result = exec_result.map_err(|e| TestErrorKind::FixtureError(e.to_string()))?;
    Ok((elapsed, result, validation))
}

/// Execute a single [`TestUnit`] at transaction index 0 for the given spec and
/// collect its canonical post-execution roots, gas, status, and output.
///
/// Returns the computed values instead of comparing them against an expectation;
/// it is the dump-time counterpart to validation.
pub fn execute_unit_collect(
    unit: &TestUnit,
    spec: &SpecName,
) -> Result<ExecutedUnit, TestErrorKind> {
    let (_elapsed, result, validation) = run_unit_once(unit, spec, true)?;
    let validation = validation.expect("roots requested");
    Ok(ExecutedUnit {
        state_root: validation.state_root,
        logs_root: validation.logs_root,
        gas_used: result.gas_used(),
        status: execution_status(&result).to_string(),
        output: result.output().cloned(),
    })
}

/// Execute a single [`TestUnit`] at transaction index 0 once, returning the time
/// spent in the EVM `transact` call together with the gas used and status.
///
/// Used by `mega-evme replay --bench-runs` to measure EVM throughput in
/// isolation (excluding RPC fetch, preceding transactions, and root computation).
pub fn time_unit_execution(
    unit: &TestUnit,
    spec: &SpecName,
) -> Result<(Duration, u64, String), TestErrorKind> {
    let (elapsed, result, _validation) = run_unit_once(unit, spec, false)?;
    Ok((elapsed, result.gas_used(), execution_status(&result).to_string()))
}

fn prune_base_fee_vault_changes(db: &mut State<EmptyDB>) {
    let base_fee_vault = address!("0x4200000000000000000000000000000000000019");
    db.cache.accounts.remove(&base_fee_vault);
}

fn debug_failed_test<'a>(ctx: DebugContext<'a>) {
    println!("\nTraces:");

    // Re-run with tracing
    let mut cache = ctx.cache_state.clone();
    cache.set_state_clear_flag(ctx.cfg.spec.into_eth_spec().is_enabled_in(SpecId::SPURIOUS_DRAGON));
    let mut state =
        database::State::builder().with_cached_prestate(cache).with_bundle_update().build();

    let evm_context = MegaContext::default()
        .with_db(&mut state)
        .with_cfg(ctx.cfg.clone())
        .with_block(ctx.block.clone())
        .with_external_envs(external_envs_for(ctx.unit).into());
    let mut tx = MegaTransaction::new(ctx.tx.clone());
    tx.enveloped_tx = Some(Bytes::default());
    let mut evm = MegaEvm::new(evm_context)
        .with_inspector(TracerEip3155::buffered(stderr()).without_summary());

    let exec_result = evm.inspect_tx_commit(tx);

    let state_after = evm.into_inner().ctx.into_inner().journaled_state.database;
    prune_base_fee_vault_changes(state_after);

    println!("\nExecution result: {exec_result:#?}");
    println!("\nExpected exception: {:?}", ctx.test.expect_exception);
    println!("\nState before: {:#?}", ctx.cache_state);
    println!("\nState after: {:#?}", state_after);
    println!("\nSpecification: {:?}", ctx.cfg.spec);
    println!("\nTx: {:#?}", ctx.tx);
    println!("Block: {:#?}", ctx.block);
    println!("Cfg: {:#?}", ctx.cfg);
    println!(
        "\nTest name: {:?} (index: {}, path: {:?}) failed:\n{}",
        ctx.name, ctx.index, ctx.path, ctx.error
    );
}

#[derive(Clone, Copy)]
struct TestRunnerConfig {
    single_thread: bool,
    trace: bool,
    print_outcome: bool,
    keep_going: bool,
}

impl TestRunnerConfig {
    fn new(single_thread: bool, trace: bool, print_outcome: bool, keep_going: bool) -> Self {
        // Trace implies print_outcome
        let print_outcome = print_outcome || trace;
        // print_outcome or trace implies single_thread
        let single_thread = single_thread || print_outcome;

        Self { single_thread, trace, print_outcome, keep_going }
    }
}

#[derive(Clone)]
struct TestRunnerState {
    n_errors: Arc<AtomicUsize>,
    console_bar: Arc<ProgressBar>,
    queue: Arc<Mutex<(usize, Vec<PathBuf>)>>,
    elapsed: Arc<Mutex<Duration>>,
}

impl TestRunnerState {
    fn new(test_files: Vec<PathBuf>) -> Self {
        let n_files = test_files.len();
        Self {
            n_errors: Arc::new(AtomicUsize::new(0)),
            console_bar: Arc::new(ProgressBar::with_draw_target(
                Some(n_files as u64),
                ProgressDrawTarget::stdout(),
            )),
            queue: Arc::new(Mutex::new((0usize, test_files))),
            elapsed: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    fn next_test(&self) -> Option<PathBuf> {
        let (current_idx, queue) = &mut *self.queue.lock().unwrap();
        let idx = *current_idx;
        let test_path = queue.get(idx).cloned()?;
        *current_idx = idx + 1;
        Some(test_path)
    }
}

fn run_test_worker(state: TestRunnerState, config: TestRunnerConfig) -> Result<(), TestError> {
    loop {
        if !config.keep_going && state.n_errors.load(Ordering::SeqCst) > 0 {
            return Ok(());
        }

        let Some(test_path) = state.next_test() else {
            return Ok(());
        };

        let result =
            execute_test_suite(&test_path, &state.elapsed, config.trace, config.print_outcome);

        state.console_bar.inc(1);

        if let Err(err) = result {
            state.n_errors.fetch_add(1, Ordering::SeqCst);
            if !config.keep_going {
                return Err(err);
            }
        }
    }
}

fn determine_thread_count(single_thread: bool, n_files: usize) -> usize {
    match (single_thread, std::thread::available_parallelism()) {
        (true, _) | (false, Err(_)) => 1,
        (false, Ok(n)) => n.get().min(n_files),
    }
}

/// Run all test files in parallel or single-threaded mode
///
/// # Arguments
/// * `test_files` - List of test files to execute
/// * `single_thread` - Force single-threaded execution
/// * `trace` - Enable EVM execution tracing
/// * `print_outcome` - Print test outcomes in JSON format
/// * `keep_going` - Continue running tests even if some fail
pub fn run(
    test_files: Vec<PathBuf>,
    single_thread: bool,
    trace: bool,
    print_outcome: bool,
    keep_going: bool,
) -> Result<(), TestError> {
    let config = TestRunnerConfig::new(single_thread, trace, print_outcome, keep_going);
    let n_files = test_files.len();
    let state = TestRunnerState::new(test_files);
    let num_threads = determine_thread_count(config.single_thread, n_files);

    // Spawn worker threads
    let mut handles = Vec::with_capacity(num_threads);
    for i in 0..num_threads {
        let state = state.clone();

        let thread = std::thread::Builder::new()
            .name(format!("runner-{i}"))
            .spawn(move || run_test_worker(state, config))
            .unwrap();

        handles.push(thread);
    }

    // Collect results from all threads
    let mut thread_errors = Vec::new();
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => thread_errors.push(e),
            Err(_) => thread_errors.push(TestError {
                name: format!("thread {i} panicked"),
                path: String::new(),
                kind: TestErrorKind::Panic,
            }),
        }
    }

    state.console_bar.finish();

    // Print summary
    println!(
        "Finished execution. Total CPU time: {:.6}s",
        state.elapsed.lock().unwrap().as_secs_f64()
    );

    let n_errors = state.n_errors.load(Ordering::SeqCst);
    let n_thread_errors = thread_errors.len();

    if n_errors == 0 && n_thread_errors == 0 {
        println!("All tests passed!");
        Ok(())
    } else {
        println!("Encountered {n_errors} errors out of {n_files} total tests");

        if n_thread_errors == 0 {
            std::process::exit(1);
        }

        if n_thread_errors > 1 {
            println!("{n_thread_errors} threads returned an error, out of {num_threads} total:");
            for error in &thread_errors {
                println!("{error}");
            }
        }
        Err(thread_errors.swap_remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::revm::context::result::{Output, SuccessReason};
    use serde_json::json;

    fn success(gas_used: u64) -> ExecutionResult<MegaHaltReason> {
        ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas_used,
            gas_refunded: 0,
            logs: vec![],
            output: Output::Call(Bytes::new()),
        }
    }

    fn revert(gas_used: u64) -> ExecutionResult<MegaHaltReason> {
        ExecutionResult::Revert { gas_used, output: Bytes::new() }
    }

    fn test_with_mega(mega_gas_used: Option<u64>, mega_status: Option<&str>) -> Test {
        let mut value = json!({
            "indexes": { "data": 0, "gas": 0, "value": 0 },
            "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "logs": "0x0000000000000000000000000000000000000000000000000000000000000000",
        });
        if let Some(gas) = mega_gas_used {
            value["megaGasUsed"] = json!(gas);
        }
        if let Some(status) = mega_status {
            value["megaStatus"] = json!(status);
        }
        serde_json::from_value(value).expect("valid Test json")
    }

    #[test]
    fn test_execution_status_strings() {
        assert_eq!(execution_status(&success(21_000)), "success");
        assert_eq!(execution_status(&revert(21_000)), "revert");
    }

    #[test]
    fn test_mega_expectations_pass_when_matching() {
        let test = test_with_mega(Some(21_000), Some("success"));
        assert!(validate_mega_expectations(&test, &success(21_000)).is_ok());
    }

    #[test]
    fn test_mega_expectations_absent_fields_skip() {
        // Pure-Ethereum test: no mega expectations → never fails on gas/status.
        let test = test_with_mega(None, None);
        assert!(validate_mega_expectations(&test, &success(99_999)).is_ok());
    }

    #[test]
    fn test_mega_expectations_gas_mismatch() {
        let test = test_with_mega(Some(21_000), None);
        let err = validate_mega_expectations(&test, &success(21_042)).unwrap_err();
        match err {
            TestErrorKind::GasUsedMismatch { got, expected } => {
                assert_eq!(got, 21_042);
                assert_eq!(expected, 21_000);
            }
            other => panic!("expected GasUsedMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_mega_expectations_status_mismatch() {
        let test = test_with_mega(None, Some("success"));
        let err = validate_mega_expectations(&test, &revert(21_000)).unwrap_err();
        match err {
            TestErrorKind::StatusMismatch { got, expected } => {
                assert_eq!(got, "revert");
                assert_eq!(expected, "success");
            }
            other => panic!("expected StatusMismatch, got {other:?}"),
        }
    }
}
