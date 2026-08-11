//! Integration tests for the CLI's exit-code taxonomy and its failure output.
//!
//! They run fully offline against the committed RPC capture
//! (`fixtures/replay_offline.cache.json`), so they are deterministic: a hash the
//! capture cannot answer models an endpoint that never answers, and the
//! validation paths need no provider at all. The mismatch class (exit 2) is
//! covered by `replay_verify.rs`, which doctors a copy of the same capture.

use std::process::{Command, Output};

mod common;

/// Offline RPC capture used as the replay file.
/// Name of the committed offline capture, resolved through the shared fixture
/// helper so its location lives in exactly one place.
const CACHE: &str = "replay_offline.cache.json";

/// Path of the committed offline capture.
fn cache() -> std::path::PathBuf {
    common::fixture(CACHE)
}

/// The transaction the committed capture can replay.
const TX_OK: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

/// A hash the capture holds no response for: the question goes unanswered.
const UNANSWERABLE_TX: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";

/// Request fingerprint of a state read `TX_OK` performs while it executes.
///
/// Entries are keyed by the request, so dropping this one from a copy of the
/// capture models an endpoint that stops answering mid-execution — the read
/// then fails inside the EVM and surfaces as a block execution error.
const IN_EXECUTION_STATE_READ: &str =
    "0x0d9aee1b171e0c4a2be0107def891d838cc94d71e4046cb95b00a1c2a61cffed";

/// Outcome of one `mega-evme` invocation.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The process exit code the run ended with.
    fn code(&self) -> i32 {
        self.code.expect("mega-evme was killed by a signal")
    }

    /// The structured error object a failing `--json` run ends with.
    fn error_object(&self) -> serde_json::Value {
        let values = common::json_values(&self.stdout);
        let last = values
            .last()
            .unwrap_or_else(|| panic!("a failing --json run must not leave stdout empty"));
        assert!(
            common::is_run_error(last),
            "the last stdout value must be the error object, got: {last}"
        );
        last.clone()
    }

    /// How many failure reports stderr carries.
    ///
    /// Counted by the report prefix: a message may itself span lines (an RPC
    /// error appends a re-capture hint), and only the report opens one.
    fn error_lines(&self) -> usize {
        self.stderr.lines().filter(|line| line.starts_with("error: ")).count()
    }
}

fn run(args: &[&str]) -> Run {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(args)
        .output()
        .expect("failed to run mega-evme");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

/// Run `replay` against the committed offline capture.
fn replay(args: &[&str]) -> Run {
    let cache = cache();
    let mut argv =
        vec!["replay", "--rpc.replay-file", cache.to_str().expect("fixture path is utf-8")];
    argv.extend_from_slice(args);
    run(&argv)
}

/// Write a copy of the committed capture without the entry `key` answers, and
/// return its path.
fn cache_without_entry(name: &str, key: &str) -> std::path::PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
            .expect("parse offline cache");
    let entries = envelope["cache"].as_array_mut().expect("cache entries");
    let before = entries.len();
    entries.retain(|entry| entry["key"].as_str() != Some(key));
    assert_eq!(entries.len() + 1, before, "the capture must hold exactly one entry for {key}");

    let path =
        std::env::temp_dir().join(format!("mega_evme_exit_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write pruned cache");
    path
}

/// Write a `--tx-file` holding `contents`, and return its path.
fn tx_file(name: &str, contents: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("mega_evme_exit_{name}_{}.txt", std::process::id()));
    std::fs::write(&path, contents).expect("write tx list");
    path
}

/// Bad input is an execution-class failure: exit 1, with the structured object
/// as the last stdout line.
#[test]
fn test_invalid_input_exits_one_with_a_json_error_object() {
    let list = tx_file("bad_hash", "not-a-hash\n");

    let run = replay(&["--tx-file", list.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 1, "bad input exits 1.\nstderr: {}", run.stderr);
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(1));
    assert_eq!(error["error"]["kind"].as_str(), Some("execution-error"));
    assert!(
        error["error"]["message"].as_str().is_some_and(|m| m.contains("not-a-hash")),
        "the message must name the offending input: {error}"
    );
}

/// A rejected flag combination is bad input too, and still ends `--json` stdout
/// with the error object rather than nothing at all.
#[test]
fn test_rejected_flag_combination_exits_one_with_a_json_error_object() {
    let run = replay(&["--dump-fixture-dir", "/tmp/mega-evme-should-not-exist", "--json", TX_OK]);

    assert_eq!(run.code(), 1, "a rejected flag combination exits 1.\nstderr: {}", run.stderr);
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("execution-error"));
}

/// A transaction the offline capture cannot answer is an RPC failure: the
/// question went unanswered, which is distinct from a definitive "no".
#[test]
fn test_offline_cache_miss_exits_rpc_failure_with_a_json_error_object() {
    let run = replay(&["--json", UNANSWERABLE_TX]);

    assert_eq!(run.code(), 3, "a cache miss exits 3.\nstderr: {}", run.stderr);
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        error["error"]["message"].as_str().is_some_and(|m| m.contains("cache miss")),
        "the message must explain the miss: {error}"
    );
}

/// A state read that fails while the EVM is executing arrives as a block
/// execution error, but it is still an unanswered question: the run exits 3, and
/// a batch reports the target as an `rpc` failure rather than an execution one.
#[test]
fn test_state_read_failure_during_execution_is_an_rpc_failure() {
    let path = cache_without_entry("state_read", IN_EXECUTION_STATE_READ);
    let cache = path.to_str().unwrap();

    let single = run(&["replay", "--rpc.replay-file", cache, "--json", TX_OK]);
    assert_eq!(single.code(), 3, "an unanswered state read exits 3.\nstderr: {}", single.stderr);
    let error = single.error_object();
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Block execution error") && m.contains("cache miss")),
        "the failure must be the block error carrying the missed read: {error}"
    );

    let list = tx_file("state_read", &format!("{TX_OK}\n"));
    let batch = run(&["replay", "--rpc.replay-file", cache, "--tx-file", list.to_str().unwrap()]);
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_file(&path);

    assert_eq!(batch.code(), 3, "the batch run exits 3 too.\nstderr: {}", batch.stderr);
    assert!(
        batch.stdout.contains("Error (rpc):"),
        "the target is reported as unanswered:\n{}",
        batch.stdout
    );
}

/// A batch run's error object follows the per-target lines, so a parser reading
/// the stream sees every target before the run-level verdict.
#[test]
fn test_batch_error_object_follows_the_per_target_lines() {
    let list = tx_file("batch_miss", &format!("{UNANSWERABLE_TX}\n"));

    let run = replay(&["--tx-file", list.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 3, "an unanswered target exits 3.\nstderr: {}", run.stderr);
    let values = common::json_values(&run.stdout);
    assert_eq!(values.len(), 2, "one per-target line plus the error object:\n{}", run.stdout);
    assert_eq!(
        values[0]["tx_hash"].as_str(),
        Some(UNANSWERABLE_TX),
        "the per-target line comes first: {}",
        values[0]
    );
    assert!(common::is_run_error(&values[1]), "the error object comes last: {}", values[1]);
}

/// Human mode reports the failure once, as `Display` text, and leaves stdout
/// untouched.
#[test]
fn test_human_failure_prints_exactly_one_error_line() {
    let list = tx_file("human", "not-a-hash\n");

    let run = replay(&["--tx-file", list.to_str().unwrap()]);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 1);
    assert_eq!(run.stderr.lines().count(), 1, "exactly one line on stderr:\n{}", run.stderr);
    assert!(run.stderr.starts_with("error: "), "the report is prefixed:\n{}", run.stderr);
    assert!(
        !run.stderr.contains("Evme(") && !run.stderr.contains("InvalidInput("),
        "the report must be Display-formatted, not Debug:\n{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "human mode prints no failure on stdout:\n{}", run.stdout);
}

/// A message that carries its own extra lines (the RPC hint) is still reported
/// exactly once, and never in `Debug` form.
#[test]
fn test_human_failure_reports_a_multi_line_message_once() {
    let run = replay(&[UNANSWERABLE_TX]);

    assert_eq!(run.code(), 3);
    assert_eq!(run.error_lines(), 1, "exactly one report on stderr:\n{}", run.stderr);
    assert!(
        !run.stderr.contains("Evme(") && !run.stderr.contains("RpcError("),
        "the report must be Display-formatted, not Debug:\n{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "human mode prints no failure on stdout:\n{}", run.stdout);
}

/// A successful run exits 0 and prints no error object: the failure surface
/// leaves the success output untouched.
#[test]
fn test_successful_run_exits_zero_without_an_error_object() {
    let run = replay(&["--json", TX_OK]);

    assert_eq!(run.code(), 0, "a faithful replay exits 0.\nstderr: {}", run.stderr);
    let values = common::json_values(&run.stdout);
    assert_eq!(values.len(), 1, "only the summary is printed:\n{}", run.stdout);
    assert!(!common::is_run_error(&values[0]), "a successful run prints no error object");
    assert_eq!(run.error_lines(), 0, "a successful run reports nothing on stderr");
}

/// A capture that could not be persisted is reported even when the run it was
/// capturing also failed: the run error keeps the exit code (it is the root
/// cause), and both failures are named on stderr without `-v`, so a stale or
/// missing capture file cannot go unnoticed.
#[tokio::test(flavor = "multi_thread")]
async fn test_capture_persist_failure_is_reported_next_to_the_run_error() {
    let server = common::MockRpcServer::start().await;
    // Chain id resolves, every other call fails: the replay itself goes
    // unanswered while the capture store still has entries to write.
    server.respond_eth_chain_id(6342, 1).await;
    server.respond_status_always(500).await;
    let url = server.uri();

    // A capture path whose parent is a regular file: persisting cannot succeed.
    let blocker =
        std::env::temp_dir().join(format!("mega_evme_capture_blocker_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&blocker);
    std::fs::write(&blocker, b"not a directory").expect("write blocker file");
    let capture = blocker.join("capture.json");

    let mut argv = vec![
        "replay",
        "--rpc",
        &url,
        "--rpc.capture-file",
        capture.to_str().unwrap(),
        "--rpc.max-retries",
        "0",
        "--rpc.backoff-ms",
        "1",
        TX_OK,
    ];
    let human = run(&argv);
    argv.push("--json");
    let json = run(&argv);
    let _ = std::fs::remove_file(&blocker);

    for run in [&human, &json] {
        assert_eq!(run.code(), 3, "the run error keeps the exit code.\nstderr: {}", run.stderr);
        assert_eq!(run.error_lines(), 2, "both failures are reported:\n{}", run.stderr);
        assert!(
            run.stderr.contains("Failed to fetch transaction"),
            "the run error must be reported:\n{}",
            run.stderr
        );
        assert!(
            run.stderr.contains(blocker.to_str().expect("blocker path is utf-8")),
            "the persist failure must name where the capture could not be written:\n{}",
            run.stderr
        );
    }

    // The structured object still reports the run error, which owns the code.
    let error = json.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Failed to fetch transaction")),
        "the object carries the run error, not the persist failure: {error}"
    );
}

/// A usage error is bad input, so it joins the execution class instead of
/// colliding with the mismatch code; `--help` stays a successful run.
#[test]
fn test_usage_errors_exit_one_and_help_exits_zero() {
    // No replay target: rejected by argument parsing.
    let usage = run(&["replay"]);
    assert_eq!(usage.code(), 1, "a usage error exits 1.\nstderr: {}", usage.stderr);
    assert!(usage.stdout.is_empty(), "a usage error prints nothing on stdout");

    let help = run(&["--help"]);
    assert_eq!(help.code(), 0, "--help exits 0");
    assert!(help.stdout.contains("mega-evme"), "--help prints usage on stdout");
}

/// A usage error of a `--json` run still ends stdout with the structured error
/// object: argument parsing fails before the command exists, but a
/// machine-readable run must never end with empty stdout.
#[test]
fn test_usage_error_in_json_mode_ends_stdout_with_the_error_object() {
    // No replay target: rejected by argument parsing.
    let usage = run(&["replay", "--json"]);

    assert_eq!(usage.code(), 1, "a usage error exits 1.\nstderr: {}", usage.stderr);
    let error = usage.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(1));
    assert_eq!(error["error"]["kind"].as_str(), Some("execution-error"));
    let message = error["error"]["message"].as_str().expect("the object carries a message");
    assert!(!message.contains('\n'), "the message is a single line: {message}");
    assert!(
        message.contains("required arguments"),
        "the message must summarize the usage error: {message}"
    );
    // clap keeps rendering its own report, including the usage block.
    assert!(usage.stderr.contains("Usage:"), "clap still reports on stderr:\n{}", usage.stderr);
}

/// `--help` in a `--json` run is still not a failure: no error object, exit 0.
#[test]
fn test_help_in_json_mode_prints_no_error_object() {
    let help = run(&["--help", "--json"]);

    assert_eq!(help.code(), 0, "--help exits 0");
    assert!(
        !help.stdout.lines().any(|line| line.trim_start().starts_with(r#"{"error""#)),
        "--help prints no error object:\n{}",
        help.stdout
    );
}

/// Closing stdout mid-batch must not abort the process.
///
/// Rust ignores SIGPIPE, so the next NDJSON `println!` panics with a broken
/// pipe. The panic hook still has to reach `exit(1)` even when it cannot write
/// the structured error object to the same closed stdout — otherwise the
/// runtime aborts (SIGABRT, shell status 134) and scripts that branch on the
/// documented 0/1/2/3 exit classes see an undefined status.
#[test]
fn test_closed_stdout_during_json_batch_exits_one() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };

    // Multi-target offline batch: many NDJSON lines, so dropping the pipe after
    // the first line still leaves further writes that hit the broken pipe.
    let envelope = common::fixture("replay_batch_blocks.cache.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args([
            "replay",
            "--rpc.replay-file",
            envelope.to_str().expect("fixture path is utf-8"),
            "--block",
            "22945844",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn mega-evme");

    let stdout = child.stdout.take().expect("child stdout was piped");
    let mut first_line = String::new();
    BufReader::new(stdout)
        .read_line(&mut first_line)
        .expect("failed to read the first NDJSON line");
    assert!(
        !first_line.trim().is_empty(),
        "batch --json must print at least one NDJSON line before further writes"
    );
    // Dropping the BufReader closes the read end. The child's next stdout write
    // then fails with EPIPE and panics into the process-wide hook.
    // (Binding ends here; no further use of the pipe.)

    let status = child.wait().expect("failed to wait for mega-evme");
    assert_eq!(
        status.code(),
        Some(1),
        "closed stdout must exit 1 (execution-error), not signal death.\nstatus: {status:?}"
    );
    assert!(
        status.code().is_some(),
        "process must not be signal-killed (e.g. SIGABRT from a double panic in the hook)"
    );
}
