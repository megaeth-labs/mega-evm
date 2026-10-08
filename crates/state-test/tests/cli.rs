//! The exit-code contract the CI gate relies on: equivalence mode exits non-zero on a failure no
//! deviation explains and on a count that differs from its pin; Satin mode reports and exits zero
//! unless a fixture could not be read; `btest` exits non-zero as equivalence mode does.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use mega_evm::{
    alloy_consensus::{Block, BlockBody, Header, TxEnvelope, EMPTY_ROOT_HASH},
    revm::{
        database::PlainAccount,
        primitives::{address, Bytes, B256, U256},
        state::AccountInfo,
    },
};
use serde_json::json;
use state_test::{
    blockchain::SkipReason,
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

/// A blockchain fixture of one empty block on a pre-state of one funded account, with
/// `edit` applied to the block's header. With no pre-block contract and no transaction the block
/// changes nothing on Ethereum, so its expected roots are the pre-state's and the empty trie's,
/// whichever engine imports it.
fn blockchain_fixture(dir: &Path, name: &str, edit: impl FnOnce(&mut Header)) -> PathBuf {
    let sender = address!("0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b");
    let balance = U256::from(10).pow(U256::from(21));
    let info = AccountInfo { balance, ..Default::default() };
    let pre = PlainAccount { info, storage: Default::default() };
    let root = state_root([(sender, &pre)]);
    let genesis = Header { state_root: root, gas_limit: 30_000_000, ..Default::default() };
    let mut header = Header {
        parent_hash: genesis.hash_slow(),
        number: 1,
        timestamp: 12,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(7),
        state_root: root,
        receipts_root: EMPTY_ROOT_HASH,
        transactions_root: EMPTY_ROOT_HASH,
        withdrawals_root: Some(EMPTY_ROOT_HASH),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    };
    edit(&mut header);
    let rlp = |header: &Header| {
        let block = Block::<TxEnvelope> {
            header: header.clone(),
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: header.withdrawals_root.map(|_| Default::default()),
            },
        };
        Bytes::from(alloy_rlp::encode(&block))
    };
    let test = json!({
        "network": "Osaka",
        "genesisRLP": rlp(&genesis),
        "blocks": [{ "rlp": rlp(&header), "transactions": [], "withdrawals": [] }],
        "pre": { sender.to_string(): { "balance": balance, "code": "0x", "nonce": "0x00", "storage": {} } },
        "postState": { sender.to_string(): { "balance": balance, "code": "0x", "nonce": "0x00", "storage": {} } },
        "lastblockhash": header.hash_slow(),
        "config": { "chainid": "0x01", "blobSchedule": { "Osaka": { "baseFeeUpdateFraction": "0x4c6964" } } },
    });
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string(&json!({ "t": test })).unwrap()).unwrap();
    path
}

fn btest(args: &[&str], path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_state-test"))
        .arg("btest")
        .args(args)
        .arg(path)
        .output()
        .expect("the binary runs")
}

#[test]
fn test_btest_a_passing_run_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = blockchain_fixture(dir.path(), "a", |_| {});
    let summary = dir.path().join("summary.json");
    let output = btest(
        &[
            "--expect-executed",
            "1",
            "--expect-skipped",
            "withdrawals=0",
            "--summary-json",
            summary.to_str().unwrap(),
        ],
        &path,
    );
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("gate: passed"));
    let summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(summary).unwrap()).unwrap();
    assert_eq!(summary["suite"], "blockchain");
    assert_eq!(summary["fork"], "Osaka");
    assert_eq!(summary["summary"]["executed"], 1);
    assert_eq!(summary["summary"]["passed"], 1);
    assert_eq!(summary["summary"]["blocks"]["matched"], 1);
    assert_eq!(summary["summary"]["blocks"]["deviated"], 0);
    assert!(stdout(&output).contains(
        "blocks checked against Ethereum 1 (matched 1, refused as expected 0)  matched to a \
         deviation 0"
    ));
    // Every skip class is named with its reason, whether or not a test was skipped for it.
    let reasons = summary["skip_reasons"].as_object().unwrap();
    assert_eq!(reasons.len(), SkipReason::ALL.len());
    for reason in SkipReason::ALL {
        assert_eq!(reasons[reason.name()], reason.reason());
    }
}

#[test]
fn test_btest_a_count_off_its_pin_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = blockchain_fixture(dir.path(), "a", |_| {});
    let output = btest(&["--expect-executed", "2"], &path);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("gate: 1 tests executed, 2 pinned"), "{}", stdout(&output));

    let output = btest(&["--expect-skipped", "header-or-body=1"], &path);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(out.contains("gate: 0 tests skipped for header-or-body, 1 pinned"), "{out}");

    // Every registered deviation lists tests of the full release; one test runs none of them.
    let output = btest(&["--expect-deviations"], &path);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(
        out.contains(
            "gate: deviation amsterdam-opcodes-on-osaka: 3 of the 3 blockchain tests it lists \
             did not deviate as listed"
        ) && out.contains("unreproduced 3"),
        "{out}"
    );
}

#[test]
fn test_btest_an_unattributed_failure_fails_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let path = blockchain_fixture(dir.path(), "a", |header| header.gas_used = 1);
    let output = btest(&[], &path);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(out.contains("unattributed 1") && out.contains("gas-used-mismatch"), "{out}");
}

#[test]
fn test_btest_bad_arguments_exit_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(btest(&[], &dir.path().join("missing")).status.code(), Some(1));
    assert_eq!(btest(&[], dir.path()).status.code(), Some(1), "a directory with no fixtures");
    let path = blockchain_fixture(dir.path(), "a", |_| {});
    for pin in ["slow=1", "withdrawals", "withdrawals=x"] {
        assert!(!btest(&["--expect-skipped", pin], &path).status.success(), "{pin}");
    }
    let twice = ["--expect-skipped", "withdrawals=0", "--expect-skipped", "withdrawals=0"];
    assert_eq!(btest(&twice, &path).status.code(), Some(1));
    std::fs::write(dir.path().join("broken.json"), "{").unwrap();
    assert_eq!(btest(&[], dir.path()).status.code(), Some(1), "an unreadable fixture");
}
