//! Audit-driven hardening tests for the published runner API.
//!
//! The runner was extracted from a CLI into a library; these tests pin down
//! that former CLI-era behaviors (panics, process exits, swallowed or masked
//! errors) now surface as structured [`TestError`]s through the public API.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use state_test::{
    runner::{
        bench_test_suite, execute_test_suite, execute_unit_collect, fill_test_suite, run,
        TestError, TestErrorKind,
    },
    types::{SpecName, TestUnit},
};

/// Minimal valid unit JSON: a funded sender sends a legacy transaction to a
/// recipient whose code is `code` (use `"0x"` for a plain transfer).
fn unit_json(code: &str) -> serde_json::Value {
    serde_json::json!({
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
            },
            "0x2000000000000000000000000000000000000002": {
                "balance": "0x0",
                "code": code,
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
            "to": "0x2000000000000000000000000000000000000002",
            "value": ["0x0"]
        },
        "post": {}
    })
}

/// A `post` entry with dummy roots; usable when the error under test fires
/// before root validation is ever reached.
fn dummy_post_entry() -> serde_json::Value {
    serde_json::json!({
        "indexes": { "data": 0, "gas": 0, "value": 0 },
        "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "logs": "0x0000000000000000000000000000000000000000000000000000000000000000"
    })
}

/// Write a one-unit suite to a unique temp file and return its path.
fn write_suite(file_name: &str, unit: &serde_json::Value) -> PathBuf {
    let suite = serde_json::json!({ "hardening_test": unit });
    let dir = std::env::temp_dir().join("mega_state_test_hardening");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(file_name);
    std::fs::write(&path, serde_json::to_string_pretty(&suite).expect("serialize"))
        .expect("write fixture");
    path
}

fn run_suite(path: &Path) -> Result<(), TestError> {
    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    // `print_json_outcome: true` keeps the failure path single-shot (no debug
    // re-run with tracing), so error assertions stay quiet and fast.
    execute_test_suite(path, &elapsed, false, true)
}

fn expect_fixture_error(result: Result<(), TestError>, needle: &str) {
    let err = result.expect_err("suite must fail");
    match &err.kind {
        TestErrorKind::FixtureError(msg) => {
            assert!(msg.contains(needle), "error should mention {needle:?}: {msg}")
        }
        other => panic!("expected FixtureError mentioning {needle:?}, got {other:?}"),
    }
}

// Fix 1: an unreadable suite file is a structured error, not a panic.
#[test]
fn unreadable_suite_file_is_fixture_error() {
    let path =
        std::env::temp_dir().join("mega_state_test_hardening").join("does_not_exist_4928.json");
    expect_fixture_error(run_suite(&path), "read");
}

// Fix 6: a present-but-overflowing currentChainID must not silently run as 6342.
#[test]
fn chain_id_overflow_is_fixture_error() {
    let mut unit = unit_json("0x");
    unit["env"]["currentChainID"] = serde_json::json!("0x10000000000000000");
    let path = write_suite("chain_id_overflow.json", &unit);
    expect_fixture_error(run_suite(&path), "currentChainID");
}

// Fix 2: an unknown spec key in `post` is a clean error, not a panic.
#[test]
fn unknown_post_spec_is_fixture_error() {
    let mut unit = unit_json("0x");
    unit["post"] = serde_json::json!({ "FutureFork9000": [dummy_post_entry()] });
    let path = write_suite("unknown_post_spec.json", &unit);
    expect_fixture_error(run_suite(&path), "spec");
}

// Fix 8 (kept behavior): a transaction invalid *by construction* — here a blob
// tx without a destination, whose type cannot be derived — still satisfies
// `expectException`.
#[test]
fn underivable_tx_type_satisfies_expect_exception() {
    let mut unit = unit_json("0x");
    let tx = &mut unit["transaction"];
    tx.as_object_mut().unwrap().remove("type");
    tx["maxFeePerBlobGas"] = serde_json::json!("0x1");
    tx["to"] = serde_json::json!("");
    let mut entry = dummy_post_entry();
    entry["expectException"] = serde_json::json!("TransactionException.TYPE_NOT_SUPPORTED");
    unit["post"] = serde_json::json!({ "Rex5": [entry] });

    let path = write_suite("expect_exception_invalid_type.json", &unit);
    run_suite(&path).expect("invalid-by-construction tx with expectException passes");
}

// A structural fixture defect — an out-of-bounds part index —
// must propagate even when the fixture expects an exception.
#[test]
fn out_of_bounds_index_propagates_despite_expect_exception() {
    let mut unit = unit_json("0x");
    let mut entry = dummy_post_entry();
    entry["indexes"]["data"] = serde_json::json!(5);
    entry["expectException"] = serde_json::json!("TransactionException.INTRINSIC_GAS_TOO_LOW");
    unit["post"] = serde_json::json!({ "Rex5": [entry] });

    let path = write_suite("expect_exception_oob_index.json", &unit);
    expect_fixture_error(run_suite(&path), "out of bounds");
}

// Fixes 3 + 8: an out-of-range fixture value (nonce > u64::MAX) is a structured
// error and is never counted as an expected exception.
#[test]
fn nonce_overflow_propagates_despite_expect_exception() {
    let mut unit = unit_json("0x");
    unit["transaction"]["nonce"] = serde_json::json!("0x10000000000000000");
    let mut entry = dummy_post_entry();
    entry["expectException"] = serde_json::json!("TransactionException.NONCE_OVERFLOW");
    unit["post"] = serde_json::json!({ "Rex5": [entry] });

    let path = write_suite("expect_exception_nonce_overflow.json", &unit);
    expect_fixture_error(run_suite(&path), "nonce");
}

// Fixes 4a + 4b + 8: an unrecoverable secret key propagates as
// `UnknownPrivateKey` (not masked, not an expected-exception pass), and its
// rendered message never contains the key material.
#[test]
fn unrecoverable_secret_key_propagates_and_is_redacted() {
    let mut unit = unit_json("0x");
    // No explicit sender → recovery from the (invalid, all-zero) key must fail.
    unit["transaction"].as_object_mut().unwrap().remove("sender");
    let mut entry = dummy_post_entry();
    entry["expectException"] = serde_json::json!("TransactionException.SENDER_NOT_EOA");
    unit["post"] = serde_json::json!({ "Rex5": [entry] });

    let path = write_suite("unknown_key_redacted.json", &unit);
    let err = run_suite(&path).expect_err("unknown key must propagate");
    assert!(matches!(err.kind, TestErrorKind::UnknownPrivateKey(_)), "got {:?}", err.kind);
    let msg = err.to_string();
    assert!(
        !msg.contains("0000000000000000000000000000000000000000000000000000000000000000"),
        "rendered error leaks key material: {msg}"
    );
    assert!(msg.contains("redacted"), "rendered error should say redacted: {msg}");
}

// Fix 4b: without `expectException`, the real underlying cause propagates
// instead of being remapped to `UnknownPrivateKey`.
#[test]
fn structural_error_is_not_masked_as_unknown_key() {
    let mut unit = unit_json("0x");
    let mut entry = dummy_post_entry();
    entry["indexes"]["data"] = serde_json::json!(5);
    unit["post"] = serde_json::json!({ "Rex5": [entry] });

    let path = write_suite("unmasked_cause.json", &unit);
    let err = run_suite(&path).expect_err("out-of-bounds index must fail");
    assert!(
        !matches!(err.kind, TestErrorKind::UnknownPrivateKey(_)),
        "cause masked as UnknownPrivateKey: {:?}",
        err.kind
    );
    match &err.kind {
        TestErrorKind::FixtureError(msg) => assert!(msg.contains("out of bounds"), "{msg}"),
        other => panic!("expected FixtureError, got {other:?}"),
    }
}

// Fix 5: `run()` returns `TestsFailed` instead of exiting the process when
// tests fail without a thread error (the `keep_going` path).
#[test]
fn run_returns_tests_failed_instead_of_exiting() {
    let mut unit = unit_json("0x");
    // Wrong roots: execution succeeds but validation must fail.
    unit["post"] = serde_json::json!({ "Rex5": [dummy_post_entry()] });
    let path = write_suite("run_tests_failed.json", &unit);

    let err = run(vec![path], true, false, true, true).expect_err("failing tests must error");
    match err.kind {
        TestErrorKind::TestsFailed { failed, total } => {
            assert_eq!(failed, 1);
            assert_eq!(total, 1);
        }
        other => panic!("expected TestsFailed, got {other:?}"),
    }
}

#[test]
fn run_returns_ok_when_all_pass() {
    // Self-validate via fill, then drive the full `run()` happy path.
    let unit = unit_json("0x");
    let path = write_suite("run_all_pass.json", &unit);
    fill_test_suite(&path, Some(SpecName::Rex5), false).expect("fill");
    run(vec![path], true, false, false, false).expect("passing suite returns Ok");
}

/// Recipient code `PUSH1 0x0f; BLOCKHASH; PUSH1 0x00; SSTORE`: stores the hash
/// of block 15 (current block is 0x10 = 16) into storage slot 0, making the
/// post-state root depend on the BLOCKHASH answer.
const BLOCKHASH_PROBE_CODE: &str = "0x600f40600055";

fn blockhash_unit(hashes: Option<serde_json::Value>) -> TestUnit {
    let mut unit = unit_json(BLOCKHASH_PROBE_CODE);
    if let Some(hashes) = hashes {
        unit["env"]["blockHashes"] = hashes;
    }
    serde_json::from_value(unit).expect("parse unit")
}

// Recorded blockHashes are injected and actually change the
// BLOCKHASH answer the EVM sees.
#[test]
fn block_hashes_are_injected_into_execution() {
    let h1 = serde_json::json!({
        "0xf": "0x1111111111111111111111111111111111111111111111111111111111111111"
    });
    let h2 = serde_json::json!({
        "0xf": "0x2222222222222222222222222222222222222222222222222222222222222222"
    });

    let run1 = execute_unit_collect(&blockhash_unit(Some(h1)), &SpecName::Rex5).expect("run h1");
    let run2 = execute_unit_collect(&blockhash_unit(Some(h2)), &SpecName::Rex5).expect("run h2");
    assert_eq!(run1.status, "success");
    assert_eq!(run2.status, "success");
    assert_ne!(
        run1.state_root, run2.state_root,
        "different recorded block hashes must produce different post-state roots"
    );

    // Absent blockHashes: execution still works on the synthetic default.
    let synthetic = execute_unit_collect(&blockhash_unit(None), &SpecName::Rex5)
        .expect("run without blockHashes");
    assert_eq!(synthetic.status, "success");
}

// Fix 11: a blockHashes key beyond u64 is a fixture error, not a silent drop.
#[test]
fn block_hashes_key_overflow_is_fixture_error() {
    let overflow = serde_json::json!({
        "0x10000000000000000":
            "0x1111111111111111111111111111111111111111111111111111111111111111"
    });
    let err = execute_unit_collect(&blockhash_unit(Some(overflow)), &SpecName::Rex5)
        .expect_err("overflowing blockHashes key must fail");
    assert!(err.to_string().contains("blockHashes"), "unexpected error: {err}");
}

// Fix 11: the suite validation path injects the same hashes the fill path used,
// so a filled BLOCKHASH-dependent fixture self-validates.
#[test]
fn block_hashes_fixture_round_trips_through_fill_and_validate() {
    let mut unit = unit_json(BLOCKHASH_PROBE_CODE);
    unit["env"]["blockHashes"] = serde_json::json!({
        "0xf": "0x3333333333333333333333333333333333333333333333333333333333333333"
    });
    let path = write_suite("blockhash_roundtrip.json", &unit);
    fill_test_suite(&path, Some(SpecName::Rex5), false).expect("fill");
    run_suite(&path).expect("filled BLOCKHASH fixture must self-validate");
}

// Fixes 2 + 10 adjacent: bench/fill reject an unknown post spec at selection
// time with an actionable message.
#[test]
fn bench_rejects_unknown_post_spec() {
    let mut unit = unit_json("0x");
    unit["post"] = serde_json::json!({ "FutureFork9000": [] });
    let path = write_suite("bench_unknown_spec.json", &unit);
    let err = bench_test_suite(&path, 1, 0, None).expect_err("unknown spec must be rejected");
    assert!(err.to_string().contains("unknown spec"), "unexpected error: {err}");
}

#[test]
fn fill_rejects_unknown_post_spec() {
    let mut unit = unit_json("0x");
    unit["post"] = serde_json::json!({ "FutureFork9000": [] });
    let path = write_suite("fill_unknown_spec.json", &unit);
    let err = fill_test_suite(&path, None, false).expect_err("unknown spec must be rejected");
    assert!(err.to_string().contains("unknown spec"), "unexpected error: {err}");
}

// A `--fill` rewrite must not delete fields outside the schema (e.g. the EEST
// `config` block): `TestUnit::extra` captures them and round-trips verbatim.
#[test]
fn fill_preserves_unknown_unit_fields() {
    let mut unit = unit_json("0x");
    unit["config"] = serde_json::json!({ "network": "Rex5", "custom": [1, 2, 3] });
    let path = write_suite("fill_preserves_extra.json", &unit);
    fill_test_suite(&path, Some(SpecName::Rex5), false).expect("fill");
    let rewritten: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    let (_, rewritten_unit) = rewritten.as_object().expect("suite").iter().next().expect("unit");
    assert_eq!(
        rewritten_unit["config"],
        serde_json::json!({ "network": "Rex5", "custom": [1, 2, 3] }),
        "unknown unit fields must survive a fill rewrite"
    );
}
