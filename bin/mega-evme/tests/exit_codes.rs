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
const CACHE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay_offline.cache.json");

/// The transaction the committed capture can replay.
const TX_OK: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

/// A hash the capture holds no response for: the question goes unanswered.
const UNANSWERABLE_TX: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";

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
    let mut argv = vec!["replay", "--rpc.replay-file", CACHE];
    argv.extend_from_slice(args);
    run(&argv)
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
