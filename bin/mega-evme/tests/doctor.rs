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

/// Hash the sample's block is served under.
const BLOCK_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// Hash the sample's parent block is served under, and the block's `parentHash`.
const PARENT_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

/// `parentHash` of the sample's parent block, so its header is well formed.
const GRANDPARENT_HASH: &str = "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

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

/// A served block body carrying a complete consensus header.
///
/// The header is complete because the reseal operations hash it: an abbreviated
/// header would make them panic on a field the real captures always carry. The
/// served `hash` is a recognizable constant rather than the hash of these
/// fields, so an entry that a reseal has rewritten is unmistakable.
fn block_result(number: u64, hash: &str, parent_hash: &str, transactions: Value) -> Value {
    json!({
        "hash": hash,
        "parentHash": parent_hash,
        "number": format!("0x{number:x}"),
        "timestamp": "0x6a704607",
        "gasLimit": "0x64",
        "gasUsed": "0x0",
        "baseFeePerGas": "0xf4240",
        "blobGasUsed": "0x0",
        "excessBlobGas": "0x0",
        "difficulty": "0x0",
        "extraData": "0x00000000fa00000001",
        "logsBloom": format!("0x{}", "0".repeat(512)),
        "miner": "0x4200000000000000000000000000000000000011",
        "mixHash": "0x5cd8791a477b467456670744425e11d5bd91fd54575d6d3bf80d761ab39d957f",
        "nonce": "0x0000000000000000",
        "receiptsRoot": "0x16fe124682128dd43a5da7f2cee0a3bf076deaf12682d19c656914bbea4615e3",
        "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
        "size": "0x43e7",
        "stateRoot": "0xa342aba318978654abcf7f09f9494ed271e2136040b628edacb6d384e9074416",
        "transactionsRoot": "0x2f3c5d0b0c4c8d34dd4e1c8bb4b4a4b6d6a2a3d3b8f6a9a2c1d0e9f8a7b6c5d4",
        "withdrawalsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
        "uncles": [],
        "withdrawals": [],
        "transactions": transactions,
    })
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
                    "blockHash": BLOCK_HASH,
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
                    "blockHash": BLOCK_HASH,
                    "cumulativeGasUsed": "0x1",
                    "status": "0x1",
                    "logs": []
                }))
            },
            {
                "key": BLOCK_KEY,
                "value": rpc_result(block_result(0x10, BLOCK_HASH, PARENT_HASH, json!([TX])))
            },
            {
                "key": PARENT_KEY,
                "value": rpc_result(block_result(0x0f, PARENT_HASH, GRANDPARENT_HASH, json!([])))
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

// --- Block header resealing -----------------------------------------------

/// The `result` object of a cache entry, parsed.
fn result_of(root: &Value, key: &str) -> Value {
    let raw = entry_by_key(root, key)["value"].as_str().expect("value");
    serde_json::from_str::<Value>(raw).expect("parse")["result"].clone()
}

/// Resealing recomputes the served `hash` from the rewritten header, so the
/// entry is authentic afterwards — which is the whole reason the operation
/// exists, since the replay recomputes exactly this.
#[test]
fn test_reseal_block_serves_the_hash_the_rewritten_header_produces() {
    let src = write_sample("reseal_authentic");
    let before = read_json(&src);
    let out = DoctoredEnvelope::load(&src)
        .reseal_block(BLOCK, |header| header["gasLimit"] = json!("0x20"))
        .write_to_temp("reseal_authentic");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    let block = result_of(&after, BLOCK_KEY);
    assert_eq!(block["gasLimit"], json!("0x20"), "the rewrite must land on the header");
    assert_eq!(
        block["hash"].as_str().expect("hash"),
        common::block_hash_of(&block),
        "the resealed block must be served under the hash its own fields produce",
    );
    assert_ne!(
        block["hash"],
        result_of(&before, BLOCK_KEY)["hash"],
        "a rewritten header must not keep the hash it was served under",
    );
}

/// Relinking follows the resealed hash everywhere the capture referenced the
/// previous one, so the doctored capture stays coherent under the inclusion and
/// linkage guards rather than tripping them by accident.
#[test]
fn test_reseal_block_relinks_every_reference_to_the_previous_hash() {
    let src = write_sample("reseal_relink");
    let out = DoctoredEnvelope::load(&src)
        .reseal_block(BLOCK - 1, |header| header["gasLimit"] = json!("0x20"))
        .write_to_temp("reseal_relink");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    let resealed = result_of(&after, PARENT_KEY)["hash"].clone();
    assert_eq!(
        result_of(&after, BLOCK_KEY)["parentHash"],
        resealed,
        "the child must name the hash its parent is now served under",
    );
    assert_eq!(
        result_of(&after, BLOCK_KEY)["hash"],
        json!(BLOCK_HASH),
        "relinking must not touch another block's own hash",
    );
}

/// The reference-keeping variant reseals and stops there: the child keeps
/// naming the hash the parent no longer has, which is the divergence it is
/// there to manufacture.
#[test]
fn test_reseal_block_keeping_references_leaves_the_child_unlinked() {
    let src = write_sample("reseal_unlinked");
    let out = DoctoredEnvelope::load(&src)
        .reseal_block_keeping_references(BLOCK - 1, |header| header["gasLimit"] = json!("0x20"))
        .write_to_temp("reseal_unlinked");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    let parent = result_of(&after, PARENT_KEY);
    assert_eq!(
        parent["hash"].as_str().expect("hash"),
        common::block_hash_of(&parent),
        "the resealed parent must still authenticate",
    );
    assert_eq!(
        result_of(&after, BLOCK_KEY)["parentHash"],
        json!(PARENT_HASH),
        "the child must keep naming the parent's previous hash",
    );
}

/// Rewriting a field without resealing leaves the served hash behind, which is
/// what makes `set_block_*` the vehicle for the header authentication itself.
#[test]
fn test_set_block_field_leaves_the_served_hash_behind() {
    let src = write_sample("unauthentic_field");
    let out = DoctoredEnvelope::load(&src)
        .set_block_field(BLOCK, "timestamp", json!("0x1"))
        .write_to_temp("unauthentic_field");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    let block = result_of(&after, BLOCK_KEY);
    assert_eq!(block["timestamp"], json!("0x1"));
    assert_eq!(block["hash"], json!(BLOCK_HASH), "the served hash must be left alone");
    assert_ne!(
        block["hash"].as_str().expect("hash"),
        common::block_hash_of(&block),
        "the rewritten header must no longer hash to what it is served under",
    );
}

/// A field the captured header does not report is a programming error in the
/// test: silently adding it would rewrite a header the endpoint never served
/// that way.
#[test]
#[should_panic(expected = "must already report `nosuchfield`")]
fn test_set_block_field_panics_on_an_unreported_field() {
    let _ =
        load_envelope("unknown_field", &sample()).set_block_field(BLOCK, "nosuchfield", json!(1));
}

// --- Moving one block's answer onto another height's request ---------------

/// The moved answer is the source entry's response verbatim — same height, same
/// served hash, byte for byte — which is what makes it authenticate: nothing
/// about the header was touched, so whatever the source hashed to, it still
/// hashes to.
///
/// The source entry keeps answering its own request, so the capture ends up
/// serving that one block at two heights: the endpoint being modelled.
#[test]
fn test_answer_block_with_moves_the_response_verbatim() {
    let src = write_sample("answer_block_with");
    let before = read_json(&src);
    let out = DoctoredEnvelope::load(&src)
        .answer_block_with(BLOCK, BLOCK - 1)
        .write_to_temp("answer_block_with");
    let after = read_json(&out);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);

    assert_eq!(
        entry_by_key(&after, BLOCK_KEY)["value"],
        entry_by_key(&before, PARENT_KEY)["value"],
        "the requested height must be answered with the other block's response, verbatim",
    );
    let moved = result_of(&after, BLOCK_KEY);
    assert_eq!(moved["number"], json!(format!("0x{:x}", BLOCK - 1)), "the served height moves too");
    assert_eq!(moved["hash"], json!(PARENT_HASH), "and so does the hash it is served under");
    assert_eq!(
        entry_by_key(&after, PARENT_KEY),
        entry_by_key(&before, PARENT_KEY),
        "the source entry must keep answering its own request",
    );
}

/// A height the capture does not serve as a block body is a programming error
/// in the test, on either side of the move.
#[test]
#[should_panic(expected = "exactly one header for block")]
fn test_answer_block_with_panics_when_the_served_height_is_absent() {
    let _ = load_envelope("answer_missing_src", &sample()).answer_block_with(BLOCK, 0x99);
}

#[test]
#[should_panic(expected = "exactly one header for block")]
fn test_answer_block_with_panics_when_the_requested_height_is_absent() {
    let _ = load_envelope("answer_missing_dst", &sample()).answer_block_with(0x99, BLOCK);
}

/// Answering a height with its own block is a no-op the caller did not mean to
/// write, so it is refused rather than silently doing nothing.
#[test]
#[should_panic(expected = "changes nothing")]
fn test_answer_block_with_panics_on_the_same_height() {
    let _ = load_envelope("answer_same", &sample()).answer_block_with(BLOCK, BLOCK);
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
            op: "set_block_field",
            apply: |env| env.set_block_field(BLOCK, "timestamp", Value::String("0x1".into())),
            mutation: rewrite(&[BLOCK_KEY]),
        },
        // Resealing the block rewrites its own `hash`, so the transaction lookup
        // and the receipt that named the previous one are relinked with it.
        Case {
            op: "reseal_block",
            apply: |env| env.reseal_block(BLOCK, |header| header["gasLimit"] = json!("0x20")),
            mutation: rewrite(&[TX_KEY, RECEIPT_KEY, BLOCK_KEY]),
        },
        // Without relinking, only the resealed entry changes — which is the
        // point: the child block keeps naming the parent's previous hash.
        Case {
            op: "reseal_block_keeping_references",
            apply: |env| {
                env.reseal_block_keeping_references(BLOCK - 1, |header| {
                    header["gasLimit"] = json!("0x20");
                })
            },
            mutation: rewrite(&[PARENT_KEY]),
        },
        Case {
            op: "set_block_hash",
            apply: |env| env.set_block_hash(BLOCK, "0xeeee"),
            mutation: rewrite(&[BLOCK_KEY]),
        },
        // Only the entry that answered the requested height changes; the block
        // whose response was moved onto it stays where it was.
        Case {
            op: "answer_block_with",
            apply: |env| env.answer_block_with(BLOCK, BLOCK - 1),
            mutation: rewrite(&[BLOCK_KEY]),
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
