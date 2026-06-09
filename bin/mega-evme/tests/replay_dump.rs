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
