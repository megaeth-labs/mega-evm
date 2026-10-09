//! Mainnet regression: a halted transaction's receipt must carry no logs.
//!
//! revm 27 gave `ExecutionResult::Halt` no `logs` field, so "a failed transaction's receipt has no
//! logs" was guaranteed by the type. revm 40 puts a log list on every variant and fills it from
//! `journal.take_logs()`. `MegaETH` rewrites an already-committed frame result into a failure —
//! pre-REX5 a CREATE's code-deposit compute gas is recorded once the constructor's checkpoint is
//! committed — so the committed logs reached the receipt and changed its logs root.
//!
//! A full-history replay of the pre-REX4 range caught three mainnet transactions doing exactly
//! that. They are captured here with their on-chain receipts, so the regression is pinned against
//! the chain rather than against a hand-written expectation: `--verify-receipt` compares status,
//! gas and logs, and fails the run on any difference.
//!
//! Runs fully offline — `--rpc.replay-file` never falls back to the network, and a cache miss is a
//! hard error. The unit-level coverage of the same defect lives in the `mega-evm` crate's
//! per-spec test suites; this file is the end-to-end half.

use std::{path::PathBuf, process::Command};

mod common;

/// Offline RPC capture: the three transactions, their on-chain receipts, the state their blocks
/// need, and the external-env snapshot. Stored compressed; resolved through the shared helper.
const CACHE: &str = "halt_logs_repro.cache.json";

/// The captured transactions. All three are large mainnet CREATEs on the `Rex` spec whose
/// constructor emitted a log before the post-commit code-deposit charge halted the transaction;
/// each on-chain receipt records zero logs.
const TXS: [&str; 3] = [
    "0x002ecbc328e5259b3756b69a221fc7ff7956dd616a9d872eda1701914bb6f3cc",
    "0x0a85678457f7b5db647f6ecd05f1ccaf17c5ef2df771d02126a73fa8b41865bb",
    "0xac0ae5fc76d7939fc55015d8865799412235387926dcf1444084c63e07ddf565",
];

fn cache() -> PathBuf {
    common::fixture(CACHE)
}

fn replay(tx: &str, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["replay", "--rpc.replay-file", cache().to_str().expect("cache path is utf-8")])
        .args(args)
        .arg(tx)
        .output()
        .expect("failed to run mega-evme");
    (
        output.status.success(),
        String::from_utf8(output.stdout).expect("stdout is utf-8"),
        String::from_utf8(output.stderr).expect("stderr is utf-8"),
    )
}

/// Each captured transaction replays to its on-chain receipt exactly. Before the log strip these
/// exited 2 with `logs_count: onchain 0 vs replay 1`.
#[test]
fn test_halted_mainnet_creates_reproduce_their_onchain_receipts() {
    for tx in TXS {
        let (success, stdout, stderr) = replay(tx, &["--verify-receipt", "--json"]);

        assert!(success, "{tx} must verify against its on-chain receipt.\nstderr: {stderr}");
        let result = common::json_values(&stdout)
            .pop()
            .unwrap_or_else(|| panic!("{tx} produced no JSON result"));
        assert_eq!(
            result["verification"],
            serde_json::json!({ "match": true }),
            "{tx} must report a receipt match, got: {result}",
        );
    }
}

/// The receipt each of them replays to reports failure and carries no logs — the window this
/// regression is about. Asserted separately from the match above so a capture that somehow lost
/// its on-chain receipts cannot let the previous test pass vacuously.
///
/// The assertion reads the emitted receipt, not the summary's `logs_count`: that field is only
/// populated on the success arm of the outcome builder and reports zero for every failed result,
/// so it cannot distinguish a leaking replay from a clean one.
#[test]
fn test_halted_mainnet_creates_report_failure_with_no_logs() {
    for tx in TXS {
        let (success, stdout, stderr) = replay(tx, &["--json"]);

        assert!(success, "{tx} must replay.\nstderr: {stderr}");
        let result = common::json_values(&stdout)
            .pop()
            .unwrap_or_else(|| panic!("{tx} produced no JSON result"));
        assert_eq!(result["success"], serde_json::json!(false), "{tx} halted on-chain");
        let logs = result["receipt"]["logs"]
            .as_array()
            .unwrap_or_else(|| panic!("{tx} produced no receipt logs array: {result}"));
        assert!(logs.is_empty(), "{tx} must replay with an empty receipt log list, got: {logs:?}");
    }
}
