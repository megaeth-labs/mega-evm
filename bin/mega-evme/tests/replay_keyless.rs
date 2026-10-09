//! Offline replay of a real mainnet `KeylessDeploy` transaction.
//!
//! Uses a committed RPC capture (`fixtures/replay_offline_keyless.cache.json`)
//! so the tests are deterministic: the call tracer nests the sandbox CREATE,
//! the opcode tracer includes the synthetic CREATE plus constructor steps, and
//! `--dump-fixture` matches the committed replay-corpus fixture byte-for-byte.

use std::process::Command;

/// Offline RPC capture of mainnet tx `0xe2e63e7b…` (`KeylessDeploy`, REX2).
const CACHE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay_offline_keyless.cache.json");

/// The captured transaction.
const TX: &str = "0xe2e63e7b5fe6d247737ec0ec32b90bfdc565754144cb065a013829d3d81b0aac";

/// Committed corpus fixture dumped from the same capture.
const CORPUS_LARGE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../bench/replay/fixtures/keyless_deploy_rex2_large.json"
);

const KEYLESS_DEPLOY: &str = "0x6342000000000000000000000000000000000003";
const DEPLOYED: &str = "0x1820a4b7618bde71dce8cdc73aab6c95905fad24";

fn mega_evme() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mega-evme"))
}

fn replay_offline(args: &[&str]) -> std::process::Output {
    let output = mega_evme()
        .args(["replay", "--rpc.replay-file", CACHE])
        .args(args)
        .arg(TX)
        .output()
        .expect("failed to run mega-evme");
    assert!(
        output.status.success(),
        "replay failed.\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    output
}

fn replay_json(args: &[&str]) -> serde_json::Value {
    let output = replay_offline(args);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "failed to parse replay JSON: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// Call tracer must hang the sandbox CREATE under the intercepted `KeylessDeploy` CALL.
#[test]
fn test_replay_keyless_call_tracer_nests_sandbox_create() {
    let summary = replay_json(&["--trace", "--tracer", "call", "--json"]);

    assert_eq!(summary["success"].as_bool(), Some(true));
    assert_eq!(summary["gas_used"].as_u64(), Some(1_112_970));

    let root = &summary["trace"];
    assert_eq!(root["type"].as_str(), Some("CALL"));
    assert_eq!(root["to"].as_str(), Some(KEYLESS_DEPLOY));
    assert_eq!(root["gasUsed"].as_str(), Some("0x10fb8a"));

    let create = &root["calls"][0];
    assert_eq!(create["type"].as_str(), Some("CREATE"));
    assert_eq!(create["to"].as_str(), Some(DEPLOYED));
    assert_eq!(create["gasUsed"].as_str(), Some("0x1854351"));
    assert!(create.get("error").is_none_or(serde_json::Value::is_null));
}

/// Opcode tracer must emit the synthetic CREATE, then sandbox constructor steps
/// at intercept-frame depth + 1, including the constructor RETURN.
#[test]
fn test_replay_keyless_opcode_tracer_includes_sandbox_steps() {
    let summary = replay_json(&[
        "--trace",
        "--tracer",
        "opcode",
        "--trace.opcode.disable-memory",
        "--trace.opcode.disable-stack",
        "--trace.opcode.disable-storage",
        "--json",
    ]);

    let logs = summary["trace"]["structLogs"].as_array().expect("structLogs array");
    assert_eq!(logs.len(), 18, "intercepted CREATE plus 17 constructor opcodes");

    // Synthetic CREATE grafted onto the intercepted CALL (depth 1, geth convention).
    assert_eq!(logs[0]["op"].as_str(), Some("CREATE"));
    assert_eq!(logs[0]["depth"].as_u64(), Some(1));
    // `gas` is the intercepting frame's remaining gas once the 100K dispatch overhead is
    // charged; `gasCost` is the gas reserved for the sandbox (the caller's override on REX2).
    assert_eq!(logs[0]["gas"].as_u64(), Some(536_184));
    assert_eq!(logs[0]["gasCost"].as_u64(), Some(26_018_741));

    // Sandbox constructor starts on the next step at intercept-frame depth + 1.
    assert_eq!(logs[1]["op"].as_str(), Some("PUSH1"));
    assert_eq!(logs[1]["depth"].as_u64(), Some(2));
    assert_eq!(logs[1]["gas"].as_u64(), Some(25_510_737));

    // Constructor RETURN at the same sandbox depth.
    assert_eq!(logs[17]["op"].as_str(), Some("RETURN"));
    assert_eq!(logs[17]["depth"].as_u64(), Some(2));
}

/// Offline `--dump-fixture` of this capture must match the committed corpus fixture.
#[test]
fn test_replay_keyless_dump_matches_committed_corpus_fixture() {
    let out =
        std::env::temp_dir().join(format!("mega_evme_keyless_dump_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);

    replay_offline(&["--dump-fixture", out.to_str().expect("utf-8 temp path")]);

    let dumped = std::fs::read(&out).expect("read dumped fixture");
    let _ = std::fs::remove_file(&out);
    let committed = std::fs::read(CORPUS_LARGE).expect("read committed corpus fixture");
    assert_eq!(
        dumped, committed,
        "offline dump must match the committed corpus fixture byte-for-byte"
    );
}
