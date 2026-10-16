//! The exit-code contract the CI gate relies on: equivalence mode exits non-zero on a failure no
//! deviation explains and on a count that differs from its pin; Satin mode reports and exits zero
//! unless a fixture could not be read.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::json;
use state_test::{
    roots::{logs_hash, state_root},
    types::TestSuite,
};

/// A fixture whose transaction cannot pay its intrinsic gas — 21,000 for a call carrying one byte
/// — and which expects `exception`. A rejected transaction leaves the pre-state, so the expected
/// roots are the pre-state's and the empty list's, whichever engine rejects it.
fn fixture(dir: &Path, exception: &str) -> PathBuf {
    let mut unit = json!({
        "env": {
            "currentCoinbase": "0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba",
            "currentGasLimit": "0x1000000",
            "currentNumber": "0x01",
            "currentTimestamp": "0x03e8",
            "currentBaseFee": "0x07",
            "currentRandom": "0x0000000000000000000000000000000000000000000000000000000000020000",
        },
        "pre": {
            "0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b": {
                "balance": "0x3635c9adc5dea00000", "code": "0x", "nonce": "0x00", "storage": {}
            },
        },
        "transaction": {
            "data": ["0x00"],
            "gasLimit": ["0x5208"],
            "gasPrice": "0x0a",
            "nonce": "0x00",
            "secretKey": "0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8",
            "to": "0x00000000000000000000000000000000000c0de0",
            "value": ["0x00"],
        },
        "post": { "Osaka": [{
            "indexes": { "data": 0, "gas": 0, "value": 0 },
            "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "logs": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "expectException": exception,
        }]},
    });
    let parsed: TestSuite = serde_json::from_value(json!({ "t": unit.clone() })).expect("a unit");
    let pre = parsed.0["t"].state();
    unit["post"]["Osaka"][0]["hash"] = json!(state_root(pre.trie_account()));
    unit["post"]["Osaka"][0]["logs"] = json!(logs_hash(&[]));

    let path = dir.join(format!("{}.json", exception.len()));
    std::fs::write(&path, serde_json::to_string(&json!({ "t": unit })).unwrap()).unwrap();
    path
}

fn state_test(args: &[&str], path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_state-test"))
        .args(args)
        .arg(path)
        .output()
        .expect("the binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn test_a_passing_run_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), "TransactionException.INTRINSIC_GAS_TOO_LOW");
    let summary = dir.path().join("summary.json");
    let output = state_test(
        &[
            "--fork",
            "Osaka",
            "--expect-executed",
            "1",
            "--expect-skipped",
            "0",
            "--summary-json",
            summary.to_str().unwrap(),
        ],
        &path,
    );
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("gate: passed"));
    let summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(summary).unwrap()).unwrap();
    assert_eq!(summary["mode"], "equivalence");
    assert_eq!(summary["summary"]["executed"], 1);
    assert_eq!(summary["summary"]["passed"], 1);
}

#[test]
fn test_a_count_off_its_pin_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), "TransactionException.INTRINSIC_GAS_TOO_LOW");
    let output = state_test(&["--fork", "Osaka", "--expect-executed", "2"], &path);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("gate: 1 tests executed, 2 pinned"), "{}", stdout(&output));

    // Every registered deviation lists entries of the full release; one test runs none of them.
    let output = state_test(&["--fork", "Osaka", "--expect-deviations"], &path);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(
        out.contains(
            "gate: deviation amsterdam-opcodes-on-osaka: 2 of the 2 entries it lists on Osaka \
             did not fail as listed"
        ) && out.contains("gate: deviation amsterdam-opcodes-on-osaka explains 0 failed tests"),
        "{out}"
    );
    assert!(out.contains("unreproduced 2") && out.contains("the run did not execute it"), "{out}");
}

#[test]
fn test_an_unattributed_failure_fails_the_gate_but_not_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), "TransactionException.NONCE_IS_MAX");
    let output = state_test(&["--fork", "Osaka"], &path);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(out.contains("unattributed 1") && out.contains("wrong-exception"), "{out}");

    let output = state_test(&["--mode", "satin", "--fork", "Osaka"], &path);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("failed   wrong-exception"));
}

#[test]
fn test_bad_arguments_exit_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let output = state_test(&["--fork", "Osaka"], &dir.path().join("missing"));
    assert_eq!(output.status.code(), Some(1));
    let output = state_test(&["--fork", "Osaka"], dir.path());
    assert_eq!(output.status.code(), Some(1), "a directory with no fixtures");
    let path = fixture(dir.path(), "TransactionException.INTRINSIC_GAS_TOO_LOW");
    assert!(!state_test(&["--fork", "Prague"], &path).status.success());
    assert!(!state_test(&[], &path).status.success(), "the fork is required");

    std::fs::write(dir.path().join("broken.json"), "{").unwrap();
    for mode in ["equivalence", "satin"] {
        let output = state_test(&["--mode", mode, "--fork", "Osaka"], dir.path());
        assert_eq!(output.status.code(), Some(1), "{mode}: an unreadable fixture");
    }
}
