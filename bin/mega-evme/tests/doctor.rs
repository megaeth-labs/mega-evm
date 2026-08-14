//! Contract tests for the typed capture-envelope doctor.
//!
//! These pin the locate-and-rewrite rules the integration tests rely on:
//! parse the cached JSON-RPC value and match a `result` field path (never a
//! raw substring), hit exactly one entry, and leave every non-target entry,
//! other envelope field, and the cache order unchanged.
//!
//! [`DoctoredEnvelope::keep_only_transactions_and_block`] is the documented
//! exception to exactly-one-hit: it filters the cache in one pass. Its contract
//! is the exact keep-set, not a single hit.

mod common;

use common::doctor::{cache_key, DoctoredEnvelope, PUBLIC_MUTATING_OPS};
use serde_json::{json, Value};

const TX: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
const OTHER: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";
const FROM: &str = "0x0000000000000000000000000000000000000abc";
const BLOCK: u64 = 0x10;

const TX_KEY: &str = "0xaaa";
const TRAP_KEY: &str = "0xtrap";
const RECEIPT_KEY: &str = "0xreceipt";
const BLOCK_KEY: &str = "0xblock";
const PARENT_KEY: &str = "0xparent";
const OTHER_KEY: &str = "0xother";
const CLONE_KEY: &str = "0xclone";

fn sender_balance_key() -> String {
    cache_key("eth_getBalance", &format!("[\"{FROM}\",\"0xf\"]"))
}

fn rpc_result(result: Value) -> String {
    json!({"jsonrpc": "2.0", "result": result}).to_string()
}

/// Shared envelope for every contract case.
///
/// The trap entry's value *string* contains the bare markers `"hash":"{TX}"`
/// and `cumulativeGasUsed`. A substring locator would count it as a
/// transaction and as a receipt; field matching must ignore it because
/// `result` has no `blockNumber` and no `cumulativeGasUsed` key.
fn sample() -> Value {
    json!({
        "version": 1,
        "chain_id": 4326,
        "meta": "keep-me",
        "cache": [
            {
                "key": TX_KEY,
                "value": rpc_result(json!({
                    "hash": TX,
                    "from": FROM,
                    "blockNumber": "0x10",
                    "blockHash": "0xbbb",
                    "gas": "0x1"
                }))
            },
            {
                "key": TRAP_KEY,
                "value": rpc_result(json!({
                    "hash": TX,
                    "from": "0x0000000000000000000000000000000000000def",
                    "note": "cumulativeGasUsed"
                }))
            },
            {
                "key": RECEIPT_KEY,
                "value": rpc_result(json!({
                    "transactionHash": TX,
                    "cumulativeGasUsed": "0x1",
                    "status": "0x1",
                    "logs": []
                }))
            },
            {
                "key": BLOCK_KEY,
                "value": rpc_result(json!({
                    "number": "0x10",
                    "hash": "0xbbb",
                    "parentHash": "0xccc",
                    "gasLimit": "0x64",
                    "transactions": [TX]
                }))
            },
            {
                "key": PARENT_KEY,
                "value": rpc_result(json!({
                    "number": "0x0f",
                    "hash": "0xccc",
                    "parentHash": "0xddd",
                    "gasLimit": "0x64",
                    "transactions": []
                }))
            },
            {
                "key": sender_balance_key(),
                "value": rpc_result(json!("0x1234"))
            },
            {
                "key": OTHER_KEY,
                "value": rpc_result(json!("0xabc"))
            }
        ]
    })
}

fn write_envelope(name: &str, envelope: &Value) -> std::path::PathBuf {
    let path = std::env::temp_dir()
        .join(format!("mega_evme_doctor_src_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write sample envelope");
    path
}

fn write_sample(name: &str) -> std::path::PathBuf {
    write_envelope(name, &sample())
}

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("parse")
}

fn load_envelope(name: &str, envelope: &Value) -> DoctoredEnvelope {
    DoctoredEnvelope::load(write_envelope(name, envelope))
}

fn cache(root: &Value) -> &[Value] {
    root["cache"].as_array().expect("cache")
}

fn entry_key(entry: &Value) -> &str {
    entry["key"].as_str().expect("key")
}

fn entry_by_key<'a>(root: &'a Value, key: &str) -> &'a Value {
    cache(root)
        .iter()
        .find(|entry| entry_key(entry) == key)
        .unwrap_or_else(|| panic!("missing cache key {key}"))
}

fn sample_without_key(key: &str) -> Value {
    let mut envelope = sample();
    envelope["cache"].as_array_mut().unwrap().retain(|entry| entry_key(entry) != key);
    envelope
}

fn sample_with_extra(entry: Value) -> Value {
    let mut envelope = sample();
    envelope["cache"].as_array_mut().unwrap().push(entry);
    envelope
}

fn sample_with_second_transaction() -> Value {
    sample_with_extra(json!({
        "key": "0xdup-tx",
        "value": rpc_result(json!({
            "hash": TX,
            "from": FROM,
            "blockNumber": "0x11",
            "blockHash": "0xeee",
            "gas": "0x1"
        }))
    }))
}

// --- Field-match trap -----------------------------------------------------

/// Field matching must treat the trap as a non-hit: nulling `TX` succeeds
/// (exactly one hit) and leaves the trap's key/value bytes untouched.
///
/// A regression to `"hash":"{tx}"` substring matching sees the trap as a
/// second transaction and panics inside `find_one`.
#[test]
fn test_transaction_locator_ignores_bare_hash_marker_trap() {
    let src = write_sample("trap");
    let before = read_json(&src);
    let out = DoctoredEnvelope::load(&src).null_transaction(TX).write_to_temp("trap");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    assert_eq!(entry_by_key(&after, TRAP_KEY), entry_by_key(&before, TRAP_KEY));
    let response: Value =
        serde_json::from_str(entry_by_key(&after, TX_KEY)["value"].as_str().unwrap()).unwrap();
    assert!(response["result"].is_null());
}

// --- Locator-family 0-hit / 2-hit ----------------------------------------

/// A missing transaction is a programming error in the test, not a silent no-op.
#[test]
#[should_panic(expected = "exactly one response")]
fn test_transaction_locator_panics_when_no_entry_matches() {
    let _ = load_envelope("tx_0", &sample()).null_transaction(OTHER);
}

/// Two field-level transaction objects (`result.hash` + `blockNumber`) for the
/// same hash are a programming error. The trap is not a second hit.
#[test]
#[should_panic(expected = "exactly one response")]
fn test_transaction_locator_panics_when_two_entries_match() {
    let _ = load_envelope("tx_2", &sample_with_second_transaction()).null_transaction(TX);
}

/// Stripping the real receipt leaves only the trap's `cumulativeGasUsed`
/// mention. Field matching must report zero hits.
#[test]
#[should_panic(expected = "exactly one receipt entry")]
fn test_receipt_locator_panics_when_no_entry_matches() {
    let _ = load_envelope("receipt_0", &sample_without_key(RECEIPT_KEY)).null_receipt();
}

#[test]
#[should_panic(expected = "exactly one receipt entry")]
fn test_receipt_locator_panics_when_two_entries_match() {
    let envelope = sample_with_extra(json!({
        "key": "0xdup-receipt",
        "value": rpc_result(json!({
            "transactionHash": OTHER,
            "cumulativeGasUsed": "0x2",
            "status": "0x1",
            "logs": []
        }))
    }));
    let _ = load_envelope("receipt_2", &envelope).null_receipt();
}

#[test]
#[should_panic(expected = "exactly one header for block")]
fn test_block_locator_panics_when_no_entry_matches() {
    let _ = load_envelope("block_0", &sample()).null_block(0x99);
}

#[test]
#[should_panic(expected = "exactly one header for block")]
fn test_block_locator_panics_when_two_entries_match() {
    let envelope = sample_with_extra(json!({
        "key": "0xdup-block",
        "value": rpc_result(json!({
            "number": "0x10",
            "hash": "0xeee",
            "parentHash": "0xfff",
            "gasLimit": "0x64",
            "transactions": [OTHER]
        }))
    }));
    let _ = load_envelope("block_2", &envelope).null_block(BLOCK);
}

#[test]
#[should_panic(expected = "exactly one entry for")]
fn test_key_locator_panics_when_no_entry_matches() {
    let _ = load_envelope("key_0", &sample()).drop_entry("0xmissing");
}

#[test]
#[should_panic(expected = "exactly one entry for")]
fn test_key_locator_panics_when_two_entries_match() {
    let envelope = sample_with_extra(json!({
        "key": OTHER_KEY,
        "value": rpc_result(json!("0xdup"))
    }));
    let _ = load_envelope("key_2", &envelope).drop_entry(OTHER_KEY);
}

/// `drop_one`'s exactly-one assertion. If that assert is removed, this test
/// stops panicking and the contract goes red.
#[test]
#[should_panic(expected = "exactly one response")]
fn test_drop_one_panics_when_no_entry_matches() {
    let _ = load_envelope("drop_0", &sample()).drop_transaction(OTHER);
}

/// Two real transaction objects: `drop_one` must refuse to delete both.
#[test]
#[should_panic(expected = "exactly one response")]
fn test_drop_one_panics_when_two_entries_match() {
    let _ = load_envelope("drop_2", &sample_with_second_transaction()).drop_transaction(TX);
}

// --- keep_only exception --------------------------------------------------

/// `keep_only_transactions_and_block` is the exactly-one-hit exception: it
/// filters the cache in one pass. The keep-set is the target transaction
/// lookup and the block body at `BLOCK`; everything else, including the trap,
/// is dropped. Survivors stay byte-identical and in original order.
#[test]
fn test_keep_only_transactions_and_block_retains_exact_keep_set() {
    let src = write_sample("keep_only");
    let before = read_json(&src);
    let out = DoctoredEnvelope::load(&src)
        .keep_only_transactions_and_block(&[TX], BLOCK)
        .write_to_temp("keep_only");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    assert_eq!(after["version"], before["version"]);
    assert_eq!(after["chain_id"], before["chain_id"]);
    assert_eq!(after["meta"], before["meta"]);
    let keys: Vec<&str> = cache(&after).iter().map(entry_key).collect();
    assert_eq!(keys, [TX_KEY, BLOCK_KEY]);
    assert_eq!(entry_by_key(&after, TX_KEY), entry_by_key(&before, TX_KEY));
    assert_eq!(entry_by_key(&after, BLOCK_KEY), entry_by_key(&before, BLOCK_KEY));
}

// --- Per-op non-target invariance -----------------------------------------

enum Mutation {
    Rewrite(Vec<String>),
    Drop(Vec<String>),
    Append(String),
    Filter(Vec<String>),
}

struct Case {
    op: &'static str,
    apply: fn(DoctoredEnvelope) -> DoctoredEnvelope,
    mutation: Mutation,
}

fn rewrite(keys: &[&str]) -> Mutation {
    Mutation::Rewrite(keys.iter().map(|key| (*key).to_string()).collect())
}

fn drop_keys(keys: &[&str]) -> Mutation {
    Mutation::Drop(keys.iter().map(|key| (*key).to_string()).collect())
}

fn filter_keys(keys: &[&str]) -> Mutation {
    Mutation::Filter(keys.iter().map(|key| (*key).to_string()).collect())
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            op: "null_transaction",
            apply: |env| env.null_transaction(TX),
            mutation: rewrite(&[TX_KEY]),
        },
        Case {
            op: "drop_transaction",
            apply: |env| env.drop_transaction(TX),
            mutation: drop_keys(&[TX_KEY]),
        },
        Case {
            op: "drain_sender_balance",
            apply: |env| env.drain_sender_balance(TX),
            mutation: Mutation::Rewrite(vec![sender_balance_key()]),
        },
        Case {
            op: "zero_transaction_gas",
            apply: |env| env.zero_transaction_gas(TX),
            mutation: rewrite(&[TX_KEY]),
        },
        Case {
            op: "reassign_transaction_sender",
            apply: |env| env.reassign_transaction_sender(TX),
            mutation: rewrite(&[TX_KEY]),
        },
        Case {
            op: "set_transaction_field",
            apply: |env| env.set_transaction_field(TX, "gas", Value::String("0x2".into())),
            mutation: rewrite(&[TX_KEY]),
        },
        Case {
            op: "set_transaction_fields",
            apply: |env| {
                env.set_transaction_fields(
                    TX,
                    &[("gas", Value::String("0x2".into())), ("nonce", Value::String("0x9".into()))],
                )
            },
            mutation: rewrite(&[TX_KEY]),
        },
        Case {
            op: "set_transaction_block_hash",
            apply: |env| env.set_transaction_block_hash(TX, Value::String("0xeeee".into())),
            mutation: rewrite(&[TX_KEY]),
        },
        Case {
            op: "set_transaction_block_number",
            apply: |env| env.set_transaction_block_number(TX, Value::String("0x11".into())),
            mutation: rewrite(&[TX_KEY]),
        },
        Case {
            op: "mark_transaction_pending",
            apply: |env| env.mark_transaction_pending(TX),
            mutation: rewrite(&[TX_KEY]),
        },
        Case {
            op: "push_cloned_transaction",
            apply: |env| {
                env.push_cloned_transaction(TX, CLONE_KEY, |result| {
                    result["hash"] = Value::String(OTHER.to_string());
                })
            },
            mutation: Mutation::Append(CLONE_KEY.to_string()),
        },
        Case {
            op: "rewrite_receipt",
            apply: |env| {
                env.rewrite_receipt(|result| {
                    result["status"] = Value::String("0x0".into());
                })
            },
            mutation: rewrite(&[RECEIPT_KEY]),
        },
        Case {
            op: "null_receipt",
            apply: |env| env.null_receipt(),
            mutation: rewrite(&[RECEIPT_KEY]),
        },
        Case {
            op: "drop_receipt",
            apply: |env| env.drop_receipt(),
            mutation: drop_keys(&[RECEIPT_KEY]),
        },
        Case {
            op: "null_block",
            apply: |env| env.null_block(BLOCK),
            mutation: rewrite(&[BLOCK_KEY]),
        },
        Case {
            op: "set_block_gas_limit",
            apply: |env| env.set_block_gas_limit(BLOCK, 0x20),
            mutation: rewrite(&[BLOCK_KEY]),
        },
        Case {
            op: "set_block_hash",
            apply: |env| env.set_block_hash(BLOCK, "0xeeee"),
            mutation: rewrite(&[BLOCK_KEY]),
        },
        Case {
            op: "set_lowest_block_hash",
            apply: |env| env.set_lowest_block_hash("0xeeee"),
            mutation: rewrite(&[PARENT_KEY]),
        },
        Case {
            op: "remove_from_block_body",
            apply: |env| env.remove_from_block_body(BLOCK, &[TX]),
            mutation: rewrite(&[BLOCK_KEY]),
        },
        Case {
            op: "remove_from_listing_block",
            apply: |env| env.remove_from_listing_block(TX),
            mutation: rewrite(&[BLOCK_KEY]),
        },
        Case {
            op: "keep_only_transactions_and_block",
            apply: |env| env.keep_only_transactions_and_block(&[TX], BLOCK),
            mutation: filter_keys(&[TX_KEY, BLOCK_KEY]),
        },
        Case {
            op: "drop_entry",
            apply: |env| env.drop_entry(OTHER_KEY),
            mutation: drop_keys(&[OTHER_KEY]),
        },
    ]
}

fn assert_non_cache_fields_identical(op: &str, before: &Value, after: &Value) {
    let before_obj = before.as_object().expect("envelope object");
    let after_obj = after.as_object().expect("envelope object");
    assert_eq!(after_obj.len(), before_obj.len(), "{op}: envelope field set must be unchanged");
    for (field, value) in before_obj {
        if field == "cache" {
            continue;
        }
        assert_eq!(&after_obj[field], value, "{op}: envelope field {field} must be unchanged");
    }
}

fn assert_rewrite(op: &str, before_cache: &[Value], after_cache: &[Value], keys: &[String]) {
    assert_eq!(after_cache.len(), before_cache.len(), "{op}: rewrite must not add or drop entries");
    let mut seen = 0usize;
    for (got, want) in after_cache.iter().zip(before_cache) {
        assert_eq!(entry_key(got), entry_key(want), "{op}: cache order must be unchanged");
        if keys.iter().any(|key| key == entry_key(want)) {
            assert_ne!(got["value"], want["value"], "{op}: target {} must change", entry_key(want));
            seen += 1;
        } else {
            assert_eq!(got, want, "{op}: non-target {} must be byte-identical", entry_key(want));
        }
    }
    assert_eq!(seen, keys.len(), "{op}: every listed target key must be present");
}

fn assert_mutation(op: &str, before: &Value, after: &Value, mutation: &Mutation) {
    assert_non_cache_fields_identical(op, before, after);
    let before_cache = cache(before);
    let after_cache = cache(after);
    match mutation {
        Mutation::Rewrite(keys) => assert_rewrite(op, before_cache, after_cache, keys),
        Mutation::Drop(keys) => {
            let expected: Vec<&Value> = before_cache
                .iter()
                .filter(|entry| !keys.iter().any(|key| key == entry_key(entry)))
                .collect();
            assert_eq!(after_cache.len(), expected.len(), "{op}: drop count");
            for (got, want) in after_cache.iter().zip(expected) {
                assert_eq!(
                    got, want,
                    "{op}: remaining entries must stay in order and byte-identical"
                );
            }
        }
        Mutation::Append(key) => {
            assert_eq!(after_cache.len(), before_cache.len() + 1, "{op}: one append");
            assert_eq!(
                &after_cache[..before_cache.len()],
                before_cache,
                "{op}: original entries must be unchanged"
            );
            assert_eq!(
                entry_key(after_cache.last().expect("appended entry")),
                key.as_str(),
                "{op}: appended key"
            );
        }
        Mutation::Filter(keys) => {
            let expected: Vec<&Value> = before_cache
                .iter()
                .filter(|entry| keys.iter().any(|key| key == entry_key(entry)))
                .collect();
            assert_eq!(after_cache.len(), keys.len(), "{op}: keep-set size");
            assert_eq!(
                after_cache.len(),
                expected.len(),
                "{op}: every keep key must exist in the sample"
            );
            for (got, want) in after_cache.iter().zip(expected) {
                assert_eq!(
                    got, want,
                    "{op}: kept entries must be byte-identical and in original order"
                );
            }
        }
    }
}

/// Every public mutator must leave non-target entry key/value strings, other
/// envelope fields, and remaining cache order byte-identical.
///
/// The table is keyed by [`PUBLIC_MUTATING_OPS`]: adding a mutator without a
/// case (or a case without an inventory entry) fails the length/order assert.
#[test]
fn test_each_public_mutating_op_leaves_non_targets_byte_identical() {
    let cases = cases();
    let listed: Vec<&str> = cases.iter().map(|case| case.op).collect();
    assert_eq!(
        listed.as_slice(),
        PUBLIC_MUTATING_OPS,
        "contract table must list every PUBLIC_MUTATING_OPS entry in declaration order"
    );

    for case in &cases {
        let name = format!("inv_{}", case.op);
        let src = write_sample(&name);
        let before = read_json(&src);
        let out = (case.apply)(DoctoredEnvelope::load(&src)).write_to_temp(&name);
        let after = read_json(&out);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&out);
        assert_mutation(case.op, &before, &after, &case.mutation);
    }
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
