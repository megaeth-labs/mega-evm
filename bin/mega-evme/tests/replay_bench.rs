//! Integration tests for `mega-evme replay --dump-fixture` / `--bench-runs`
//! output and flag guards.
//!
//! Runs fully offline against a committed RPC capture
//! (`fixtures/replay_offline_bench.cache.json`, which includes the on-chain
//! receipt), so it is deterministic. Benchmark timing is inherently
//! non-deterministic, so the bench tests assert output *shape* (single JSON
//! document, `bench` field, deterministic gas/runs) rather than exact values —
//! the regression the fixture-driven, exact-match harness cannot cover. The
//! dump tests exercise the end-to-end CLI write path and validate the written
//! file with the state-test runner.

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Offline RPC capture (includes the on-chain receipt needed by the fidelity gate).
const CACHE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay_offline_bench.cache.json");

/// The transaction captured in `CACHE` (a 75,514-gas Rex5 mainnet call).
const TX: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

fn mega_evme() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mega-evme"))
}

/// `--json --bench-runs` must emit a single JSON document with the benchmark
/// folded in under a `bench` field — not two concatenated documents.
#[test]
fn test_replay_bench_json_is_single_document() {
    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            CACHE,
            "--bench-runs",
            "5",
            "--bench-warmup",
            "1",
            "--json",
            TX,
        ])
        .output()
        .expect("failed to run mega-evme");

    assert!(
        output.status.success(),
        "replay failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    // Parsing the whole stdout as one value fails if two JSON documents are printed.
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not a single JSON document: {e}\n{stdout}"));

    assert!(value.get("success").is_some(), "missing replay `success` field");
    assert!(value.get("gas_used").is_some(), "missing replay `gas_used` field");

    let bench = value.get("bench").expect("missing `bench` field");
    assert_eq!(bench["runs"], 5, "bench.runs");
    assert_eq!(bench["gasUsed"], 75_514, "bench.gasUsed (deterministic)");
    for key in ["minNs", "medianNs", "meanNs", "mgasPerSec"] {
        assert!(bench.get(key).is_some(), "missing bench.{key}");
    }
}

/// `--bench-runs` (and `--dump-fixture`) are incompatible with transaction
/// overrides, and must be rejected before any execution.
#[test]
fn test_replay_bench_rejects_transaction_overrides() {
    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            CACHE,
            "--bench-runs",
            "5",
            "--override.gas-limit",
            "50000",
            TX,
        ])
        .output()
        .expect("failed to run mega-evme");

    assert!(!output.status.success(), "overrides + --bench-runs should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be combined with transaction"),
        "expected override-incompatibility error, got stderr:\n{stderr}"
    );
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
