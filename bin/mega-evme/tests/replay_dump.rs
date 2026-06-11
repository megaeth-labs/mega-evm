//! Integration tests for `mega-evme replay --dump-fixture`: the end-to-end CLI
//! write path and its flag guards.
//!
//! Runs fully offline against a committed RPC capture
//! (`fixtures/replay_offline.cache.json`, which includes the on-chain receipt),
//! so it is deterministic: the tests dump a fixture, validate the written file
//! with the state-test runner, check byte-reproducibility, and verify the
//! transaction-override and `--override.spec` guards. (Benchmarking lives in
//! `state-test --bench`; see `bench/replay/`.)

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Offline RPC capture (includes the on-chain receipt needed by the fidelity gate).
const CACHE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay_offline.cache.json");

/// The transaction captured in `CACHE` (a 75,514-gas Rex5 mainnet call).
const TX: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

fn mega_evme() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mega-evme"))
}

/// `--dump-fixture` is incompatible with transaction overrides (the isolated
/// execution would not represent the on-chain transaction), and must be
/// rejected before any execution, writing nothing.
#[test]
fn test_replay_dump_rejects_transaction_overrides() {
    let out = std::env::temp_dir().join(format!("mega_evme_dump_ovr_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            CACHE,
            "--dump-fixture",
            out.to_str().unwrap(),
            "--override.gas-limit",
            "50000",
            TX,
        ])
        .output()
        .expect("failed to run mega-evme");

    assert!(!output.status.success(), "--dump-fixture + overrides should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be combined with transaction"),
        "expected override-incompatibility error, got stderr:\n{stderr}"
    );
    assert!(!out.exists(), "must not write a fixture when the run is rejected");
}

/// `replay --dump-fixture` must write a fixture file that the state-test runner
/// validates — exercising the full CLI write path (receipt capture, fidelity
/// gate, finalize/write ordering, path handling).
#[test]
fn test_replay_dump_fixture_writes_validatable_file() {
    let out = std::env::temp_dir().join(format!("mega_evme_dump_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let output = mega_evme()
        .args(["replay", "--rpc.replay-file", CACHE, "--dump-fixture", out.to_str().unwrap(), TX])
        .output()
        .expect("failed to run mega-evme");

    assert!(
        output.status.success(),
        "dump failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(out.exists(), "dump did not write the fixture file");

    // Validate the dumped fixture through the state-test runner (no RPC).
    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    let result = state_test::runner::execute_test_suite(&out, &elapsed, false, false);
    let _ = std::fs::remove_file(&out);
    result.unwrap_or_else(|e| panic!("dumped fixture failed to validate: {e}"));
}

/// Two dumps of the same transaction must be byte-identical. This guards against
/// non-deterministic serialization (e.g. a hash-map creeping back into the
/// fixture's pre-state, storage, or megaEnv) — which content-based validation
/// cannot catch, and which only surfaces across separate process invocations.
#[test]
fn test_replay_dump_is_byte_reproducible() {
    let dump_to = |suffix: &str| {
        let out = std::env::temp_dir()
            .join(format!("mega_evme_repro_{}_{suffix}.json", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let output = mega_evme()
            .args([
                "replay",
                "--rpc.replay-file",
                CACHE,
                "--dump-fixture",
                out.to_str().unwrap(),
                TX,
            ])
            .output()
            .expect("failed to run mega-evme");
        assert!(
            output.status.success(),
            "dump failed.\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(&out).expect("read dumped fixture");
        let _ = std::fs::remove_file(&out);
        bytes
    };

    assert_eq!(
        dump_to("a"),
        dump_to("b"),
        "two dumps of the same transaction must be byte-identical"
    );
}

/// Dumping over an existing fixture must go through a sibling temp file +
/// rename: on success the target holds the new (valid) content and no
/// `.json.tmp` residue is left behind, so an interrupt mid-write can no longer
/// truncate a committed corpus fixture.
#[test]
fn test_replay_dump_overwrites_atomically_without_tmp_residue() {
    let out =
        std::env::temp_dir().join(format!("mega_evme_dump_atomic_{}.json", std::process::id()));
    let tmp = out.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    // Seed a pre-existing "committed" fixture that the dump overwrites in place.
    std::fs::write(&out, br#"{"pre-existing":"corpus fixture"}"#).expect("seed existing fixture");

    let output = mega_evme()
        .args(["replay", "--rpc.replay-file", CACHE, "--dump-fixture", out.to_str().unwrap(), TX])
        .output()
        .expect("failed to run mega-evme");

    assert!(
        output.status.success(),
        "dump over an existing fixture failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!tmp.exists(), "dump must not leave a .json.tmp file behind");

    let content = std::fs::read_to_string(&out).expect("read dumped fixture");
    let _ = std::fs::remove_file(&out);
    let suite: serde_json::Value =
        serde_json::from_str(&content).expect("overwritten fixture must be valid JSON");
    let unit = suite
        .get(format!("replay_{TX}"))
        .expect("overwritten fixture must hold the replayed transaction's unit");
    assert!(
        unit.get("post").is_some(),
        "overwritten fixture must contain the freshly computed post section"
    );
}

/// The fidelity gate must reject a receipt that describes a different inclusion
/// than the replayed block (a reorg in progress, or a load-balanced endpoint
/// serving divergent views). Doctor the captured receipt's `blockHash` and
/// expect a clear error with no fixture written.
#[test]
fn test_replay_dump_rejects_receipt_from_different_block() {
    // Doctor the capture: flip the receipt's blockHash. Cache entries are keyed
    // by the request, not the response, so the doctored entry still resolves.
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(CACHE).expect("read offline cache"))
            .expect("parse offline cache");
    let mut doctored = false;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        // The receipt is the only cached response carrying cumulativeGasUsed.
        if !value.contains("cumulativeGasUsed") {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse receipt response");
        response["result"]["blockHash"] = serde_json::Value::String(
            "0x1111111111111111111111111111111111111111111111111111111111111111".into(),
        );
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored = true;
    }
    assert!(doctored, "offline cache should contain the receipt entry");

    let doctored_cache =
        std::env::temp_dir().join(format!("mega_evme_reorg_cache_{}.json", std::process::id()));
    std::fs::write(&doctored_cache, envelope.to_string()).expect("write doctored cache");
    let out =
        std::env::temp_dir().join(format!("mega_evme_dump_reorg_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            doctored_cache.to_str().unwrap(),
            "--dump-fixture",
            out.to_str().unwrap(),
            TX,
        ])
        .output()
        .expect("failed to run mega-evme");
    let _ = std::fs::remove_file(&doctored_cache);

    assert!(!output.status.success(), "a receipt from a different block must abort the dump");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("different inclusion"),
        "expected reorg/divergent-endpoint hint, got stderr:\n{stderr}"
    );
    assert!(!out.exists(), "must not write a fixture when the receipt anchor mismatches");
}

/// `--dump-fixture` must reject `--override.spec` (a forced spec would make the
/// fixture a what-if, not the on-chain transaction) and write nothing.
#[test]
fn test_replay_dump_rejects_spec_override() {
    let out = std::env::temp_dir().join(format!("mega_evme_dump_spec_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            CACHE,
            "--dump-fixture",
            out.to_str().unwrap(),
            "--override.spec",
            "Rex4",
            TX,
        ])
        .output()
        .expect("failed to run mega-evme");

    assert!(!output.status.success(), "--dump-fixture + --override.spec should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be combined with --override.spec"),
        "expected spec-override-incompatibility error, got stderr:\n{stderr}"
    );
    assert!(!out.exists(), "must not write a fixture when the run is rejected");
}
