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

/// The digest options print one line per executed transaction on standard error: the compact form
/// the test and the hash of its record, the full form the record too, and the same line twice.
#[test]
fn test_the_digest_options_print_a_line_per_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), "TransactionException.INTRINSIC_GAS_TOO_LOW");
    let lines = |flag: &str| -> Vec<serde_json::Value> {
        let output = state_test(&[flag, "--fork", "Osaka"], &path);
        assert!(output.status.success(), "{}", stdout(&output));
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let compact = lines("--json-digest");
    assert_eq!(compact.len(), 1);
    assert_eq!(compact[0]["row"], "outcome");
    assert!(compact[0]["digest"].as_str().unwrap().starts_with("0x"));
    assert!(compact[0].get("record").is_none());
    assert_eq!(compact, lines("--json-digest"));

    let full = lines("--json-digest-full");
    assert_eq!(full.len(), 1);
    assert_eq!(full[0]["digest"], compact[0]["digest"]);
    assert!(full[0]["record"]["rejected"].is_string(), "the transaction is rejected");
}

/// The JSON lines of a run's standard error.
fn stderr_lines(output: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The witness options print one line per executed transaction on standard error, as the digest
/// options do: the compact form the test and the hash of what it read, the full form the reads
/// too, and the same line twice. Asked for with the digest, both lines are printed in one run, and
/// the digest line is the one the digest options print alone.
#[test]
fn test_the_witness_options_print_a_line_per_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), "TransactionException.INTRINSIC_GAS_TOO_LOW");
    let lines = |flags: &[&str]| -> Vec<serde_json::Value> {
        let mut args = flags.to_vec();
        args.extend(["--fork", "Osaka"]);
        let output = state_test(&args, &path);
        assert!(output.status.success(), "{}", stdout(&output));
        stderr_lines(&output)
    };
    let compact = lines(&["--json-witness"]);
    assert_eq!(compact.len(), 1);
    assert_eq!(compact[0]["row"], "outcome");
    assert!(compact[0]["witness"].as_str().unwrap().starts_with("0x"));
    assert!(compact[0].get("reads").is_none() && compact[0].get("digest").is_none());
    assert_eq!(compact, lines(&["--json-witness"]));

    let full = lines(&["--json-witness-full"]);
    assert_eq!(full.len(), 1);
    assert_eq!(full[0]["witness"], compact[0]["witness"]);
    // A transaction rejected for its intrinsic gas read the live system address and nothing else:
    // every transaction reads it first, from the `SequencerRegistry`, which the pre-state lacks.
    let registry = "0x6342000000000000000000000000000000000006";
    let reads = &full[0]["reads"];
    assert_eq!(reads["accounts"], json!({ registry: null }), "{reads}");
    assert_eq!(reads["storage"][registry].as_object().map(|slots| slots.len()), Some(1));

    let both = lines(&["--json-digest", "--json-witness"]);
    assert_eq!(both, [lines(&["--json-digest"])[0].clone(), compact[0].clone()]);
}

/// A state test whose transaction calls a contract that reads the hash of the block its calldata
/// names, `number`, and keeps nothing of it, with placeholder roots: Satin mode reports it whatever
/// they are. Any one-byte number costs the same calldata.
fn block_hash_fixture(dir: &Path, number: u8) -> PathBuf {
    // PUSH0 CALLDATALOAD BLOCKHASH POP STOP
    let code = "0x5f35405000";
    let data = format!("0x{number:064x}");
    let unit = json!({
        "env": {
            "currentCoinbase": "0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba",
            "currentGasLimit": "0x1000000",
            "currentNumber": "0x10",
            "currentTimestamp": "0x03e8",
            "currentBaseFee": "0x07",
            "currentRandom": "0x0000000000000000000000000000000000000000000000000000000000020000",
        },
        "pre": {
            "0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b": {
                "balance": "0x3635c9adc5dea00000", "code": "0x", "nonce": "0x00", "storage": {}
            },
            "0x00000000000000000000000000000000000c0de0": {
                "balance": "0x00", "code": code, "nonce": "0x01", "storage": {}
            },
        },
        "transaction": {
            "data": [data],
            "gasLimit": ["0x0f4240"],
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
        }]},
    });
    let path = dir.join(format!("block-hash-{number}.json"));
    std::fs::write(&path, serde_json::to_string(&json!({ "t": unit })).unwrap()).unwrap();
    path
}

/// Two transactions that read the hashes of two different blocks and keep nothing of them end in
/// the same outcome, so their digests are equal; what they read is not, and the witness tells
/// them apart, by the one block hash each read.
#[test]
fn test_the_witness_tells_apart_two_runs_the_digest_cannot() {
    let dir = tempfile::tempdir().unwrap();
    let lines = |number: u8| -> Vec<serde_json::Value> {
        let path = block_hash_fixture(dir.path(), number);
        let output = state_test(
            &["--mode", "satin", "--fork", "Osaka", "--json-digest", "--json-witness-full"],
            &path,
        );
        assert!(output.status.success(), "{}", stdout(&output));
        stderr_lines(&output)
    };
    let (one, two) = (lines(15), lines(14));
    assert_eq!(one.len(), 2);
    assert_eq!(one[0]["digest"], two[0]["digest"], "the outcomes are the same");
    assert_ne!(one[1]["witness"], two[1]["witness"], "the reads are not");
    let number =
        |lines: &[serde_json::Value]| lines[1]["reads"]["block_hashes"][0]["number"].clone();
    assert_eq!((number(&one), number(&two)), (json!(15), json!(14)));
    let mut without_hashes = [one[1]["reads"].clone(), two[1]["reads"].clone()];
    for reads in &mut without_hashes {
        reads["block_hashes"] = json!([]);
    }
    assert_eq!(without_hashes[0], without_hashes[1], "the block hash is the one read apart");
}

/// The limit options tighten Satin mode's limits; equivalence mode holds none, so it refuses them.
#[test]
fn test_the_limit_options_need_satin_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path(), "TransactionException.INTRINSIC_GAS_TOO_LOW");
    for option in [
        ["--limit-data-size", "1024"],
        ["--limit-frame-data-size", "1024"],
        ["--limit-compute-gas", "30000"],
        ["--limit-kv-updates", "10"],
        ["--limit-state-growth", "10"],
    ] {
        let mut args = vec!["--fork", "Osaka"];
        args.extend(option);
        let output = state_test(&args, &path);
        assert_eq!(output.status.code(), Some(1), "{option:?} in equivalence mode");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--mode satin"));

        args.extend(["--mode", "satin"]);
        assert!(state_test(&args, &path).status.success(), "{option:?} in Satin mode");
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
    assert!(
        out.contains("gate: 1 blockchain tests failed: no listed outcome explains them"),
        "{out}"
    );
    // The help says what the deviation pin requires of a listed test.
    let help = Command::new(env!("CARGO_BIN_EXE_state-test"))
        .args(["btest", "--help"])
        .output()
        .expect("the binary runs");
    let help = stdout(&help).split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(help.contains("deviates exactly as listed"), "{help}");
    assert!(!help.contains("fails exactly as listed"), "{help}");
}

/// The witness options print one line per imported block, accepted or refused, with what it
/// read; the import is judged as without them.
#[test]
fn test_btest_the_witness_options_print_a_line_per_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = blockchain_fixture(dir.path(), "a", |_| {});
    let output = btest(&["--json-witness"], &path);
    assert!(output.status.success(), "{}", stdout(&output));
    assert_eq!(stdout(&output), stdout(&btest(&[], &path)));
    let compact = stderr_lines(&output);
    assert_eq!(compact.len(), 1);
    assert_eq!((&compact[0]["block"], &compact[0]["row"]), (&json!(0), &json!("accepted")));
    assert!(compact[0]["witness"].as_str().unwrap().starts_with("0x"));
    assert!(compact[0].get("reads").is_none());

    let full = stderr_lines(&btest(&["--json-witness-full"], &path));
    assert_eq!(full[0]["witness"], compact[0]["witness"]);
    // The pre-block calls read the EIP-2935 history contract, which the pre-state does not hold.
    let history = "0x0000f90827f1c53a10cb7a02335b175320002935";
    assert!(full[0]["reads"]["accounts"][history].is_null(), "{}", full[0]["reads"]);

    // A block without a parent beacon root, which the EIP-4788 pre-block call needs, is refused
    // after what it read so far: a line of its own, which the import's failure does not drop.
    let refused = blockchain_fixture(dir.path(), "b", |header| {
        header.parent_beacon_block_root = None;
    });
    let output = btest(&["--json-witness"], &refused);
    assert!(stdout(&output).contains("unattributed 1"), "{}", stdout(&output));
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!((&lines[0]["block"], &lines[0]["row"]), (&json!(0), &json!("refused")));
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
