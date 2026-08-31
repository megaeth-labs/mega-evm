//! Exit-code contract tests for the `state-test` CLI.
//!
//! The runner library no longer calls `std::process::exit`; the CLI owns the
//! contract CI relies on: failing tests (or any error) → stderr message +
//! exit code 1, passing run → exit code 0.

use std::{path::PathBuf, process::Command};

/// A unit whose execution succeeds but whose recorded roots are wrong, so
/// validation must fail.
const FAILING_SUITE: &str = r#"{
    "exit_code_test": {
        "env": {
            "currentChainID": "0x18c6",
            "currentCoinbase": "0x3000000000000000000000000000000000000003",
            "currentDifficulty": "0x0",
            "currentGasLimit": "0x1c9c380",
            "currentNumber": "0x10",
            "currentTimestamp": "0x3e8",
            "currentBaseFee": "0x0",
            "currentRandom": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "currentExcessBlobGas": "0x0"
        },
        "pre": {
            "0x1000000000000000000000000000000000000001": {
                "balance": "0xde0b6b3a7640000",
                "code": "0x",
                "nonce": "0x0",
                "storage": {}
            }
        },
        "transaction": {
            "type": 0,
            "data": ["0x"],
            "gasLimit": ["0x30d40"],
            "gasPrice": "0x0",
            "nonce": "0x0",
            "secretKey": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "sender": "0x1000000000000000000000000000000000000001",
            "to": "0x1000000000000000000000000000000000000001",
            "value": ["0x0"]
        },
        "post": {
            "Rex5": [{
                "indexes": { "data": 0, "gas": 0, "value": 0 },
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "logs": "0x0000000000000000000000000000000000000000000000000000000000000000"
            }]
        }
    }
}"#;

fn write_fixture(file_name: &str, contents: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("state_test_cli_exit");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(file_name);
    std::fs::write(&path, contents).expect("write fixture");
    path
}

fn run_cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_state-test")).args(args).output().expect("spawn state-test")
}

#[test]
fn failing_tests_exit_with_code_1() {
    let path = write_fixture("failing.json", FAILING_SUITE);
    let path = path.to_str().expect("utf8 path");

    // `--keep-going` exercises the `TestsFailed` summary path (no thread error).
    let out = run_cli(&[path, "--keep-going", "--json-outcome"]);
    assert_eq!(out.status.code(), Some(1), "failing tests must exit 1 (keep-going)");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("failed"),
        "stderr should report the failure"
    );

    // Without `--keep-going` the first thread error propagates; still exit 1.
    let out = run_cli(&[path, "--json-outcome"]);
    assert_eq!(out.status.code(), Some(1), "failing tests must exit 1 (fail-fast)");
}

#[test]
fn invalid_path_exits_with_code_1() {
    let out = run_cli(&["/nonexistent/state_test_cli_exit_4928"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!out.stderr.is_empty(), "stderr should carry the error message");
}

#[test]
fn passing_run_exits_with_code_0() {
    // The same unit with an empty `post` validates trivially: the run completes
    // with zero errors and must keep exiting 0.
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    suite["exit_code_test"]["post"] = serde_json::json!({});
    let passing = serde_json::to_string(&suite).expect("serialize");

    let path = write_fixture("passing.json", &passing);
    let out = run_cli(&[path.to_str().expect("utf8 path")]);
    assert_eq!(out.status.code(), Some(0), "passing run must exit 0");
}

#[test]
fn diff_run_with_no_unexplained_difference_exits_with_code_0() {
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    // The differential run computes both sides itself; the recorded `post` is irrelevant, and an
    // empty one keeps the fixture honest about that.
    suite["exit_code_test"]["post"] = serde_json::json!({});
    let path = write_fixture("diff_pass.json", &serde_json::to_string(&suite).expect("serialize"));
    let report = write_fixture("diff_pass_report.json", "");

    let out = run_cli(&[
        path.to_str().expect("utf8 path"),
        "--bench-spec",
        "Rex7",
        "--diff-spec",
        "Rex6",
        "--diff-report",
        report.to_str().expect("utf8 path"),
    ]);
    assert_eq!(out.status.code(), Some(0), "a run with no unexplained difference must exit 0");

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("read report"))
            .expect("report is json");
    assert_eq!(written["targetSpec"], "Rex7");
    assert_eq!(written["baseSpec"], "Rex6");
    assert_eq!(written["classes"]["PASS"], 1);
    assert_eq!(written["classes"]["UNEXPLAINED"], 0);
}

#[test]
fn diff_run_over_two_unrelated_specs_reports_unexplained_and_exits_1() {
    // The Rex7 precision invariant relates Rex7 to Rex6 and says nothing about any other pair, so
    // a Rex7-against-Equivalence difference carries no licensing evidence — the MegaETH intrinsic
    // surcharge alone moves the receipt. This is the negative control for the gate: a classifier
    // that explained everything would exit 0 here.
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    suite["exit_code_test"]["post"] = serde_json::json!({});
    let path = write_fixture("diff_fail.json", &serde_json::to_string(&suite).expect("serialize"));

    let out = run_cli(&[
        path.to_str().expect("utf8 path"),
        "--bench-spec",
        "Rex7",
        "--diff-spec",
        "Equivalence",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("UNEXPLAINED"), "tally should report the class: {stdout}");
    assert_eq!(out.status.code(), Some(1), "an unexplained difference must fail the gate");
}

#[test]
fn diff_spec_requires_an_explicit_target_spec() {
    let path = write_fixture("diff_needs_target.json", FAILING_SUITE);
    let out = run_cli(&[path.to_str().expect("utf8 path"), "--diff-spec", "Rex6"]);
    assert_eq!(out.status.code(), Some(2), "clap rejects the incomplete flag combination");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("bench-spec"),
        "the error should name the missing flag"
    );
}

#[test]
fn diff_spec_rejects_an_unknown_spec_name() {
    let path = write_fixture("diff_bad_spec.json", FAILING_SUITE);
    let out = run_cli(&[
        path.to_str().expect("utf8 path"),
        "--bench-spec",
        "Rex7",
        "--diff-spec",
        "FutureFork9000",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--diff-spec"),
        "the error should name the offending flag"
    );
}
