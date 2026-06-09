//! Integration tests for `mega-evme replay --bench-runs` output and flag guards.
//!
//! Runs fully offline against a committed RPC capture
//! (`fixtures/replay_offline_bench.cache.json`), so it is deterministic. Timing
//! values are inherently non-deterministic, so these tests assert output
//! *shape* (single JSON document, `bench` field, deterministic gas/runs) rather
//! than exact values — which is exactly the regression the fixture-driven,
//! exact-match harness cannot cover.

use std::process::Command;

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
