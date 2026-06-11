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
