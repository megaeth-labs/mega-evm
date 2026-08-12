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
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    time::Duration,
};

mod common;

/// Offline RPC capture (includes the on-chain receipt needed by the fidelity
/// gate). Name of the committed offline capture, resolved through the shared fixture
/// helper so its location lives in exactly one place.
const CACHE: &str = "replay_offline.cache.json";

/// Path of the committed offline capture.
fn cache() -> PathBuf {
    common::fixture(CACHE)
}

/// The transaction captured in `CACHE` (a 75,514-gas Rex5 mainnet call).
const TX: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

fn mega_evme() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mega-evme"))
}

/// A temp path unique to this process and this test.
fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mega_evme_dump_{name}_{}.json", std::process::id()))
}

/// Run `replay --dump-fixture` offline against `cache`, writing to `out`.
fn dump(cache: &Path, out: &Path) -> Output {
    mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            cache.to_str().expect("cache path is utf-8"),
            "--dump-fixture",
            out.to_str().expect("fixture path is utf-8"),
            "--json",
            TX,
        ])
        .output()
        .expect("failed to run mega-evme")
}

/// Run `replay --verify-receipt` offline against `cache`, the mode whose
/// classification the dump path reuses.
fn verify(cache: &Path) -> Output {
    mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            cache.to_str().expect("cache path is utf-8"),
            "--verify-receipt",
            "--json",
            TX,
        ])
        .output()
        .expect("failed to run mega-evme")
}

/// The structured error object a failing `--json` run ends its stdout with.
fn error_object(stdout: &str) -> serde_json::Value {
    let values = common::json_values(stdout);
    let last = values
        .last()
        .unwrap_or_else(|| panic!("a failing --json run must not leave stdout empty:\n{stdout}"));
    assert!(common::is_run_error(last), "the last stdout value must be the error object: {last}");
    last["error"].clone()
}

/// Write a copy of the committed capture in which the `result` of every cached
/// response `selects` accepts is rewritten by `doctor`, and return its path.
///
/// Cache entries are keyed by the request, not the response, so a doctored entry
/// still resolves and the run meets the doctored answer where it would meet the
/// real one.
fn rewrite_cache(
    name: &str,
    selects: impl Fn(&serde_json::Value) -> bool,
    doctor: impl Fn(&mut serde_json::Value),
) -> PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
            .expect("parse offline cache");
    let mut doctored = false;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse cached response");
        if !selects(&response["result"]) {
            continue;
        }
        doctor(&mut response["result"]);
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored = true;
    }
    assert!(doctored, "offline capture should contain the entry being doctored");

    let path = temp_path(name);
    std::fs::write(&path, envelope.to_string()).expect("write doctored cache");
    path
}

/// A copy of the committed capture whose on-chain receipt response is rewritten
/// by `doctor`. The receipt is the only cached response carrying
/// `cumulativeGasUsed`.
fn cache_with_doctored_receipt(name: &str, doctor: impl Fn(&mut serde_json::Value)) -> PathBuf {
    rewrite_cache(name, |result| result.get("cumulativeGasUsed").is_some(), doctor)
}

/// A copy of the committed capture with the on-chain receipt dropped entirely.
/// Offline, the absent entry surfaces as a cache miss — the transport failing to
/// answer the receipt request at all.
fn cache_without_receipt(name: &str) -> PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
            .expect("parse offline cache");
    let entries = envelope["cache"].as_array_mut().expect("cache entries");
    let before = entries.len();
    entries.retain(|entry| {
        !entry["value"].as_str().expect("entry value is a string").contains("cumulativeGasUsed")
    });
    assert!(entries.len() < before, "offline capture should contain the receipt entry");

    let path = temp_path(name);
    std::fs::write(&path, envelope.to_string()).expect("write pruned cache");
    path
}

/// A copy of the committed capture whose target-transaction lookup answers null,
/// modelling an endpoint that does not know the hash the caller asked about.
fn cache_with_null_target_transaction(name: &str) -> PathBuf {
    rewrite_cache(
        name,
        |result| result.get("hash").and_then(serde_json::Value::as_str) == Some(TX),
        |result| *result = serde_json::Value::Null,
    )
}

/// `--dump-fixture` is incompatible with transaction overrides (the isolated
/// execution would not represent the on-chain transaction), and must be
/// rejected before any execution, writing nothing.
#[test]
fn test_replay_dump_rejects_transaction_overrides() {
    let out = std::env::temp_dir().join(format!("mega_evme_dump_ovr_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let cache = cache();
    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            cache.to_str().unwrap(),
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

    let cache = cache();
    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            cache.to_str().unwrap(),
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
        let cache = cache();
        let output = mega_evme()
            .args([
                "replay",
                "--rpc.replay-file",
                cache.to_str().unwrap(),
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

/// Dumping over an existing fixture must go through a unique temp file +
/// persist: on success the target holds the new (valid) content and no
/// leftover temp files remain in the destination directory, so an interrupt
/// mid-write can no longer truncate a committed corpus fixture.
#[test]
fn test_replay_dump_overwrites_atomically_without_tmp_residue() {
    let out =
        std::env::temp_dir().join(format!("mega_evme_dump_atomic_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);
    // Seed a pre-existing "committed" fixture that the dump overwrites in place.
    std::fs::write(&out, br#"{"pre-existing":"corpus fixture"}"#).expect("seed existing fixture");

    let cache = cache();
    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            cache.to_str().unwrap(),
            "--dump-fixture",
            out.to_str().unwrap(),
            TX,
        ])
        .output()
        .expect("failed to run mega-evme");

    assert!(
        output.status.success(),
        "dump over an existing fixture failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // NamedTempFile uses a random name; ensure only the destination remains.
    let parent = out.parent().expect("temp dir");
    let stem = out.file_stem().and_then(|s| s.to_str()).expect("utf-8 stem");
    let leftovers: Vec<_> = std::fs::read_dir(parent)
        .expect("list temp dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(stem) && name != out.file_name().unwrap().to_string_lossy()
        })
        .map(|e| e.path())
        .collect();
    assert!(leftovers.is_empty(), "dump must not leave temp residue: {leftovers:?}");

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
/// serving divergent views). The endpoint served a view the run cannot use, so
/// this is a retryable infrastructure failure (exit 3) — the same class
/// `--verify-receipt` gives the identical condition — and no fixture is written.
#[test]
fn test_replay_dump_rejects_receipt_from_different_block() {
    let doctored_cache = cache_with_doctored_receipt("reorg_cache", |receipt| {
        receipt["blockHash"] =
            "0x1111111111111111111111111111111111111111111111111111111111111111".into();
    });
    let out = temp_path("reorg");
    let _ = std::fs::remove_file(&out);

    let output = dump(&doctored_cache, &out);
    let _ = std::fs::remove_file(&doctored_cache);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "a receipt from a different block is an infrastructure failure.\nstderr: {stderr}",
    );
    assert!(
        stderr.contains("different inclusion"),
        "expected reorg/divergent-endpoint hint, got stderr:\n{stderr}"
    );
    let error = error_object(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(error["kind"].as_str(), Some("rpc-failure"), "got: {error}");
    assert!(!out.exists(), "must not write a fixture when the receipt anchor mismatches");
}

/// The fidelity gate must reject a receipt that describes a different
/// transaction than the one requested: anchoring a fixture to another
/// transaction's gas, status and logs would bake a wrong expectation into the
/// artifact. The endpoint answered a question that was never asked, so this is an
/// infrastructure failure (exit 3) and no fixture is written.
#[test]
fn test_replay_dump_rejects_receipt_for_another_transaction() {
    const OTHER_TX: &str = "0x00000000000000000000000000000000000000000000000000000000feed0001";

    let doctored_cache = cache_with_doctored_receipt("wrong_tx_cache", |receipt| {
        receipt["transactionHash"] = OTHER_TX.into();
    });
    let out = temp_path("wrong_tx");
    let _ = std::fs::remove_file(&out);

    let output = dump(&doctored_cache, &out);
    let _ = std::fs::remove_file(&doctored_cache);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "a receipt for another transaction is an infrastructure failure.\nstderr: {stderr}",
    );
    let error = error_object(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(error["kind"].as_str(), Some("rpc-failure"), "got: {error}");
    let message = error["message"].as_str().expect("the error object carries a message");
    assert!(
        message.contains(OTHER_TX) && message.contains(TX),
        "the message must name both the served and the requested transaction: {message}"
    );
    assert!(!out.exists(), "must not write a fixture when the receipt is for another transaction");
}

/// A receipt the endpoint answers with `null` leaves the fidelity gate's
/// question unanswered: the run resolved this very transaction as mined moments
/// earlier, so the null is a pruned receipt or a divergent backend, not a
/// definitive "unknown transaction". It must exit 3 as an infrastructure
/// failure, name the receipt, and write no fixture.
#[test]
fn test_replay_dump_null_receipt_is_an_rpc_failure() {
    let doctored_cache = cache_with_doctored_receipt("null_receipt_cache", |receipt| {
        *receipt = serde_json::Value::Null;
    });
    let out = temp_path("null_receipt");
    let _ = std::fs::remove_file(&out);

    let output = dump(&doctored_cache, &out);
    let _ = std::fs::remove_file(&doctored_cache);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "an unanswered receipt is an infrastructure failure.\nstderr: {stderr}",
    );
    let error = error_object(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(error["kind"].as_str(), Some("rpc-failure"), "got: {error}");
    let message = error["message"].as_str().expect("the error object carries a message");
    assert!(
        message.contains("No on-chain receipt for transaction") && message.contains(TX),
        "the message must name the unanswered receipt and its transaction: {message}"
    );
    assert!(
        !message.contains("Transaction not found"),
        "a replayed transaction must not be reported as unknown: {message}"
    );
    assert!(!out.exists(), "must not write a fixture when the receipt is unanswered");
}

/// A receipt request the transport never answers (offline: the capture holds no
/// receipt entry) is the third receipt-fetch anomaly, and stays an
/// infrastructure failure with no fixture written.
#[test]
fn test_replay_dump_unanswered_receipt_request_is_an_rpc_failure() {
    let pruned_cache = cache_without_receipt("pruned_receipt_cache");
    let out = temp_path("pruned_receipt");
    let _ = std::fs::remove_file(&out);

    let output = dump(&pruned_cache, &out);
    let _ = std::fs::remove_file(&pruned_cache);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "an unanswered receipt request is an infrastructure failure.\nstderr: {stderr}",
    );
    let error = error_object(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(error["kind"].as_str(), Some("rpc-failure"), "got: {error}");
    assert!(
        error["message"].as_str().is_some_and(|m| m.contains("eth_getTransactionReceipt")),
        "the message must name the unanswered request: {error}"
    );
    assert!(!out.exists(), "must not write a fixture when the receipt request goes unanswered");
}

/// Negative control for the classification above: a null answer for the *target
/// transaction* is the endpoint answering the caller's own question with a
/// definitive "unknown transaction", and keeps its execution class (exit 1).
/// Only the fidelity gate's receipt question moved to the infrastructure class,
/// not every null the run can meet.
#[test]
fn test_replay_dump_null_target_transaction_stays_an_execution_error() {
    let doctored_cache = cache_with_null_target_transaction("null_target_cache");
    let out = temp_path("null_target");
    let _ = std::fs::remove_file(&out);

    let output = dump(&doctored_cache, &out);
    let _ = std::fs::remove_file(&doctored_cache);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unknown target transaction is a definitive negative answer.\nstderr: {stderr}",
    );
    let error = error_object(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(error["kind"].as_str(), Some("execution-error"), "got: {error}");
    assert!(
        error["message"].as_str().is_some_and(|m| m.contains("Transaction not found")),
        "got: {error}"
    );
    assert!(!out.exists(), "must not write a fixture when the target is unknown");
}

/// The dump path and the verify path fetch the same receipt for the same
/// transaction, so an endpoint anomaly must produce the identical failure: same
/// exit code, same class, same message. A pipeline can then branch on the exit
/// code without knowing which mode produced it.
#[test]
fn test_dump_and_verify_classify_receipt_anomalies_identically() {
    // A receipt the endpoint does not serve at all.
    assert_dump_and_verify_agree("agree_null_receipt", |receipt| {
        *receipt = serde_json::Value::Null;
    });
    // A receipt describing a different inclusion than the replayed block.
    assert_dump_and_verify_agree("agree_reorg", |receipt| {
        receipt["blockHash"] =
            "0x1111111111111111111111111111111111111111111111111111111111111111".into();
    });
    // A receipt describing a different transaction than the one requested.
    assert_dump_and_verify_agree("agree_wrong_tx", |receipt| {
        receipt["transactionHash"] =
            "0x00000000000000000000000000000000000000000000000000000000feed0001".into();
    });
}

/// Run both modes against a capture whose receipt response is rewritten by
/// `doctor`, and assert the two runs fail with the same exit code and the same
/// error object.
fn assert_dump_and_verify_agree(name: &str, doctor: impl Fn(&mut serde_json::Value)) {
    let doctored_cache = cache_with_doctored_receipt(&format!("{name}_cache"), doctor);
    let out = temp_path(name);
    let _ = std::fs::remove_file(&out);

    let dumped = dump(&doctored_cache, &out);
    let verified = verify(&doctored_cache);
    let _ = std::fs::remove_file(&doctored_cache);
    let _ = std::fs::remove_file(&out);

    assert_eq!(
        dumped.status.code(),
        verified.status.code(),
        "{name}: dump and verify must exit with the same code.\ndump stderr: {}\nverify stderr: {}",
        String::from_utf8_lossy(&dumped.stderr),
        String::from_utf8_lossy(&verified.stderr),
    );
    assert_eq!(
        error_object(&String::from_utf8_lossy(&dumped.stdout)),
        error_object(&String::from_utf8_lossy(&verified.stdout)),
        "{name}: dump and verify must report the same failure object",
    );
}

/// `--dump-fixture` must reject `--override.spec` (a forced spec would make the
/// fixture a what-if, not the on-chain transaction) and write nothing.
#[test]
fn test_replay_dump_rejects_spec_override() {
    let out = std::env::temp_dir().join(format!("mega_evme_dump_spec_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let cache = cache();
    let output = mega_evme()
        .args([
            "replay",
            "--rpc.replay-file",
            cache.to_str().unwrap(),
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
