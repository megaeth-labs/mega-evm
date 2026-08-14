//! Contract tests for the typed capture-envelope doctor.
//!
//! These pin the locate-and-rewrite rules the integration tests rely on:
//! exactly one hit, and a round-trip that leaves every non-target entry, other
//! envelope field, and the cache order unchanged.

mod common;

use common::doctor::{cache_key, DoctoredEnvelope};

const TX: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
const OTHER: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";

fn sample() -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "chain_id": 4326,
        "meta": "keep-me",
        "cache": [
            {
                "key": "0xaaa",
                "value": format!(
                    "{{\"jsonrpc\":\"2.0\",\"result\":{{\"hash\":\"{TX}\",\"from\":\"0xabc\",\"blockNumber\":\"0x10\",\"blockHash\":\"0xbbb\",\"gas\":\"0x1\"}}}}"
                )
            },
            {
                "key": "0xbbb",
                "value": format!(
                    "{{\"jsonrpc\":\"2.0\",\"result\":{{\"number\":\"0x10\",\"hash\":\"0xbbb\",\"parentHash\":\"0xccc\",\"gasLimit\":\"0x64\",\"transactions\":[\"{TX}\"]}}}}"
                )
            },
            {
                "key": "0xccc",
                "value": "{\"jsonrpc\":\"2.0\",\"result\":\"0xabc\"}"
            }
        ]
    })
}

fn write_sample(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir()
        .join(format!("mega_evme_doctor_src_{name}_{}.json", std::process::id()));
    std::fs::write(&path, sample().to_string()).expect("write sample envelope");
    path
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("parse")
}

/// Nulling one transaction rewrites only that entry's value string.
#[test]
fn test_null_transaction_leaves_other_entries_and_fields_untouched() {
    let src = write_sample("null_tx");
    let before = read_json(&src);
    let out = DoctoredEnvelope::load(&src).null_transaction(TX).write_to_temp("null_tx");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    assert_eq!(after["version"], before["version"]);
    assert_eq!(after["chain_id"], before["chain_id"]);
    assert_eq!(after["meta"], before["meta"]);
    let before_cache = before["cache"].as_array().unwrap();
    let after_cache = after["cache"].as_array().unwrap();
    assert_eq!(after_cache.len(), before_cache.len());
    assert_eq!(after_cache[0]["key"], before_cache[0]["key"]);
    assert_eq!(after_cache[1], before_cache[1]);
    assert_eq!(after_cache[2], before_cache[2]);
    let response: serde_json::Value =
        serde_json::from_str(after_cache[0]["value"].as_str().unwrap()).unwrap();
    assert!(response["result"].is_null());
}

/// Dropping an entry keeps the remaining entries in their original order.
#[test]
fn test_drop_transaction_preserves_remaining_order() {
    let src = write_sample("drop_tx");
    let before = read_json(&src);
    let out = DoctoredEnvelope::load(&src).drop_transaction(TX).write_to_temp("drop_tx");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    let after_cache = after["cache"].as_array().unwrap();
    let before_cache = before["cache"].as_array().unwrap();
    assert_eq!(after_cache.len(), 2);
    assert_eq!(after_cache[0], before_cache[1]);
    assert_eq!(after_cache[1], before_cache[2]);
}

/// A missing transaction is a programming error in the test, not a silent no-op.
#[test]
#[should_panic(expected = "exactly one response")]
fn test_null_transaction_panics_when_no_entry_matches() {
    let src = write_sample("missing");
    let _ = DoctoredEnvelope::load(&src).null_transaction(OTHER).write_to_temp("missing");
}

/// Two matching transaction objects are a programming error in the test.
#[test]
#[should_panic(expected = "exactly one response")]
fn test_null_transaction_panics_when_two_entries_match() {
    let mut envelope = sample();
    envelope["cache"].as_array_mut().unwrap().push(serde_json::json!({
        "key": "0xddd",
        "value": format!(
            "{{\"jsonrpc\":\"2.0\",\"result\":{{\"hash\":\"{TX}\",\"from\":\"0xdef\"}}}}"
        )
    }));
    let src =
        std::env::temp_dir().join(format!("mega_evme_doctor_src_dup_{}.json", std::process::id()));
    std::fs::write(&src, envelope.to_string()).expect("write duplicate envelope");
    let _ = DoctoredEnvelope::load(&src).null_transaction(TX).write_to_temp("dup");
}

/// The key formula matches the capturing transport (and the hardcoded key
/// already used by the dump-abort fixture injection).
#[test]
fn test_cache_key_matches_transport_formula() {
    let later = "0xfc0a0b9d76b13125ac1e36e524f6df3a72c25720c023b960b23c6f5891be05bc";
    assert_eq!(
        cache_key("eth_getTransactionByHash", &format!("[\"{later}\"]")),
        "0x91bbb37d27a588e217e5be6aeab0fb377ffea0ad3a2714d1f54ceb69852124f2"
    );
}
