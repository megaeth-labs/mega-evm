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
//! the chain rather than against a hand-written expectation: `--verify-receipt` compares the
//! receipt's consensus fields (status, gas, logs, type), and fails the run on any difference. One
//! of the three closes its block, whose whole body is captured, so `--verify-header` also checks
//! that block against its header.
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

/// A failed CREATE still reports the address it targeted, exactly as the node's receipt does.
///
/// The address is derived from the sender and its nonce, not from a deployment, so the node
/// stamps it on every creation receipt whatever the outcome. The expected values are the ones
/// the captured on-chain receipts carry. The execution summary keeps naming a deployed contract
/// only, so it must not report one for these halts.
#[test]
fn test_halted_mainnet_creates_report_the_onchain_contract_address() {
    const ONCHAIN_CONTRACT_ADDRESSES: [(&str, &str); 3] = [
        (TXS[0], "0x3152a8cd6ca0c64675c73486b06203a9d8226448"),
        (TXS[1], "0xd8e977e9e7e81d29daec823bf60e0303e80281cb"),
        (TXS[2], "0xa190ae4c4f01740a4ac1e15d4e26a9991cfaeaab"),
    ];
    for (tx, expected) in ONCHAIN_CONTRACT_ADDRESSES {
        let (success, stdout, stderr) = replay(tx, &["--json"]);

        assert!(success, "{tx} must replay.\nstderr: {stderr}");
        let result = common::json_values(&stdout)
            .pop()
            .unwrap_or_else(|| panic!("{tx} produced no JSON result"));
        assert_eq!(
            result["receipt"]["contractAddress"],
            serde_json::json!(expected),
            "{tx} must report the on-chain contractAddress, got: {result}",
        );
        assert!(
            result.get("contract_address").is_none(),
            "a halted CREATE deployed nothing, so the summary must not name a contract: {result}",
        );
    }
}

/// The first capture's transaction is the last of its block, and the capture holds every body
/// transaction before it, so the whole block replays offline. `--verify-header` then checks the
/// halted CREATE against the chain's own commitment rather than only against its served receipt:
/// the rebuilt receipts root (which commits to its status, cumulative gas and empty logs), logs
/// bloom and gas used must reproduce the header. The capture holds only the targets' receipts, so
/// `--verify-receipt` cannot run over this block.
#[test]
fn test_halted_create_block_reproduces_its_header() {
    const BLOCK: u64 = 3_452_027;
    const BODY_LEN: usize = 22;

    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["replay", "--rpc.replay-file", cache().to_str().expect("cache path is utf-8")])
        .args(["--block", &BLOCK.to_string(), "--verify-header", "--json"])
        .output()
        .expect("failed to run mega-evme");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(output.status.success(), "block {BLOCK} must replay in full.\nstderr: {stderr}");

    let lines = common::json_values(&stdout);
    let (headers, txs): (Vec<_>, Vec<_>) =
        lines.iter().partition(|line| line.get("header_verification").is_some());
    assert_eq!(txs.len(), BODY_LEN, "every body transaction must replay: {stdout}");
    assert!(
        txs.iter().all(|line| line.get("error").is_none()),
        "no transaction may fail: {stdout}"
    );
    assert_eq!(
        txs.last().expect("the block has transactions")["tx_hash"],
        serde_json::json!(TXS[0]),
        "the halted CREATE is the last transaction of the block",
    );
    let [header] = headers.as_slice() else {
        panic!("expected exactly one header verdict, got {headers:?}");
    };
    assert_eq!(header["block_number"], serde_json::json!(BLOCK));
    assert_eq!(
        header["header_verification"],
        serde_json::json!({ "match": true }),
        "the replayed block must reproduce its header, got: {header}",
    );
}
