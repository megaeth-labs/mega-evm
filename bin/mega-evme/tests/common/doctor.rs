//! Typed doctor for committed RPC capture envelopes.
//!
//! Offline tests manufacture fault scenarios by rewriting a copy of a capture
//! (`{"chain_id", "cache": [{"key", "value"}, ...], ...}`). Entries are keyed by
//! the request (`keccak256("{method}\\x00{params_json}")`), so a doctored
//! response still resolves. Request method/params are not stored on disk: an
//! entry is located by parsing the cached JSON-RPC value and matching a
//! `result` field path, or by recomputing its request key. Every operation
//! asserts it hit exactly one entry
//! ([`DoctoredEnvelope::keep_only_transactions_and_block`] is the documented
//! exception).
//!
//! Non-target entries, other envelope fields, and cache order are left
//! untouched. The typical call is
//! `DoctoredEnvelope::load(path).null_transaction(hash).write_to_temp(name)`.
//! One-shot associated functions wrap the high-frequency single-operation
//! cases so call sites stay in domain language.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Recomputes a capture-entry key the same way the capturing transport does:
/// `keccak256("{method}\x00{params_json}")` as a 0x-prefixed lowercase hex string.
///
/// Request method/params are not on disk; tests that must locate an entry by
/// request (for example a sender's parent-block balance) recompute this.
pub(crate) fn cache_key(method: &str, params_json: &str) -> String {
    alloy_primitives::keccak256(format!("{method}\x00{params_json}")).to_string()
}

/// Names of every public mutating operation that returns [`DoctoredEnvelope`].
///
/// One-shot `PathBuf` wrappers are excluded: they only load, delegate to one of
/// these methods, and write. Contract tests assert their table covers this list
/// so a newly added mutator cannot land without an invariance case.
pub(crate) const PUBLIC_MUTATING_OPS: &[&str] = &[
    "null_transaction",
    "drop_transaction",
    "drain_sender_balance",
    "zero_transaction_gas",
    "reassign_transaction_sender",
    "set_transaction_field",
    "set_transaction_fields",
    "set_transaction_block_hash",
    "set_transaction_block_number",
    "mark_transaction_pending",
    "push_cloned_transaction",
    "rewrite_receipt",
    "null_receipt",
    "drop_receipt",
    "null_block",
    "set_block_gas_limit",
    "set_block_hash",
    "set_lowest_block_hash",
    "remove_from_block_body",
    "remove_from_listing_block",
    "keep_only_transactions_and_block",
    "drop_entry",
];

/// In-memory copy of a capture envelope, mutated one operation at a time.
pub(crate) struct DoctoredEnvelope {
    root: Value,
}

impl DoctoredEnvelope {
    /// Parses a committed (or previously written) capture envelope from `path`.
    ///
    /// # Panics
    ///
    /// Panics if the file cannot be read or is not a JSON object with a `cache`
    /// array.
    pub(crate) fn load(path: impl AsRef<Path>) -> Self {
        let raw = std::fs::read_to_string(path.as_ref()).expect("read envelope");
        let root: Value = serde_json::from_str(&raw).expect("parse envelope");
        assert!(
            root.get("cache").and_then(Value::as_array).is_some(),
            "envelope must have a cache array"
        );
        Self { root }
    }

    /// Writes the envelope to a process-unique temp path derived from `name`
    /// and returns that path.
    pub(crate) fn write_to_temp(self, name: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("mega_evme_doctored_{name}_{}.json", std::process::id()));
        std::fs::write(&path, self.root.to_string()).expect("write doctored envelope");
        path
    }

    // --- High-frequency one-shot wrappers ---------------------------------

    /// Nulls the `eth_getTransactionByHash` result for `tx_hash` (endpoint
    /// answers "unknown transaction") and writes the copy.
    pub(crate) fn without_transaction(src: impl AsRef<Path>, name: &str, tx_hash: &str) -> PathBuf {
        Self::load(src).null_transaction(tx_hash).write_to_temp(name)
    }

    /// Drops the `eth_getTransactionByHash` entry for `tx_hash` (offline cache
    /// miss) and writes the copy.
    pub(crate) fn dropping_transaction(
        src: impl AsRef<Path>,
        name: &str,
        tx_hash: &str,
    ) -> PathBuf {
        Self::load(src).drop_transaction(tx_hash).write_to_temp(name)
    }

    /// Zeros the sender's parent-block `eth_getBalance` answer for `tx_hash`
    /// and writes the copy.
    pub(crate) fn with_drained_sender(src: impl AsRef<Path>, name: &str, tx_hash: &str) -> PathBuf {
        Self::load(src).drain_sender_balance(tx_hash).write_to_temp(name)
    }

    /// Sets `gas` to `0x0` on the transaction object for `tx_hash` and writes
    /// the copy.
    pub(crate) fn with_zero_gas_transaction(
        src: impl AsRef<Path>,
        name: &str,
        tx_hash: &str,
    ) -> PathBuf {
        Self::load(src).zero_transaction_gas(tx_hash).write_to_temp(name)
    }

    /// Replaces the served `from` on `tx_hash` with a different address and
    /// writes the copy.
    pub(crate) fn with_reassigned_sender(
        src: impl AsRef<Path>,
        name: &str,
        tx_hash: &str,
    ) -> PathBuf {
        Self::load(src).reassign_transaction_sender(tx_hash).write_to_temp(name)
    }

    /// Nulls the `eth_getBlockByNumber` body for `number` and writes the copy.
    pub(crate) fn without_block(src: impl AsRef<Path>, name: &str, number: u64) -> PathBuf {
        Self::load(src).null_block(number).write_to_temp(name)
    }

    /// Rewrites the advertised `gasLimit` of block `number` and writes the copy.
    pub(crate) fn with_block_gas_limit(
        src: impl AsRef<Path>,
        name: &str,
        number: u64,
        gas_limit: u64,
    ) -> PathBuf {
        Self::load(src).set_block_gas_limit(number, gas_limit).write_to_temp(name)
    }

    /// Rewrites the unique receipt result with `doctor` and writes the copy.
    pub(crate) fn with_receipt(
        src: impl AsRef<Path>,
        name: &str,
        doctor: impl FnOnce(&mut Value),
    ) -> PathBuf {
        Self::load(src).rewrite_receipt(doctor).write_to_temp(name)
    }

    /// Drops the unique receipt entry and writes the copy.
    pub(crate) fn without_receipt(src: impl AsRef<Path>, name: &str) -> PathBuf {
        Self::load(src).drop_receipt().write_to_temp(name)
    }

    /// Nulls the unique receipt result and writes the copy.
    pub(crate) fn with_null_receipt(src: impl AsRef<Path>, name: &str) -> PathBuf {
        Self::load(src).null_receipt().write_to_temp(name)
    }

    /// Drops the unique entry whose key is `key` and writes the copy.
    pub(crate) fn without_entry(src: impl AsRef<Path>, name: &str, key: &str) -> PathBuf {
        Self::load(src).drop_entry(key).write_to_temp(name)
    }

    /// Rewrites the lower-numbered block body's own `hash` and writes the copy.
    ///
    /// Returns the path together with the hash the untouched capture reported.
    /// The capture is expected to hold exactly two block bodies (replayed block
    /// and its parent), matching the single-transaction offline fixture.
    pub(crate) fn with_unlinked_parent(
        src: impl AsRef<Path>,
        name: &str,
        wrong_hash: &str,
    ) -> (PathBuf, String) {
        let env = Self::load(src);
        let original = env.lowest_block_hash();
        (env.set_lowest_block_hash(wrong_hash).write_to_temp(name), original)
    }

    /// Rewrites `tx_hash`'s inclusion `blockHash` and writes the copy.
    ///
    /// Returns the path together with the hash the untouched capture reported.
    pub(crate) fn with_inclusion_hash(
        src: impl AsRef<Path>,
        name: &str,
        tx_hash: &str,
        block_hash: Value,
    ) -> (PathBuf, String) {
        let env = Self::load(src);
        let original = env.transaction_str(tx_hash, "blockHash");
        (env.set_transaction_block_hash(tx_hash, block_hash).write_to_temp(name), original)
    }

    /// Removes `tx_hash` from the unique block body that lists it and writes
    /// the copy.
    ///
    /// Returns the path together with that block's hash.
    pub(crate) fn without_target_in_block_body(
        src: impl AsRef<Path>,
        name: &str,
        tx_hash: &str,
    ) -> (PathBuf, String) {
        let env = Self::load(src);
        let block_hash = env.listing_block_hash(tx_hash);
        (env.remove_from_listing_block(tx_hash).write_to_temp(name), block_hash)
    }

    // --- Transaction operations -------------------------------------------

    /// Sets the unique `eth_getTransactionByHash` result for `tx_hash` to null.
    #[must_use]
    pub(crate) fn null_transaction(mut self, tx_hash: &str) -> Self {
        let idx = self.transaction_index(tx_hash);
        self.rewrite_at(idx, |response| {
            response["result"] = Value::Null;
        });
        self
    }

    /// Removes the unique `eth_getTransactionByHash` entry for `tx_hash`.
    #[must_use]
    pub(crate) fn drop_transaction(mut self, tx_hash: &str) -> Self {
        self.drop_one(
            |entry| is_transaction_for(entry, tx_hash),
            &format!("response for {tx_hash}"),
        );
        self
    }

    /// Zeros the unique parent-block `eth_getBalance` answer for `tx_hash`'s
    /// sender. The served transaction stays byte-identical.
    #[must_use]
    pub(crate) fn drain_sender_balance(mut self, tx_hash: &str) -> Self {
        let result = self.transaction_result(tx_hash);
        let from = result["from"].as_str().expect("transaction `from`").to_string();
        let number = parse_hex_u64(result["blockNumber"].as_str().expect("blockNumber"))
            .expect("hex block number");
        let params = format!("[\"{from}\",\"0x{:x}\"]", number - 1);
        let key = cache_key("eth_getBalance", &params);
        let idx = self.find_one(
            |entry| entry["key"].as_str() == Some(key.as_str()),
            "sender parent-block balance",
        );
        self.rewrite_at(idx, |response| {
            response["result"] = Value::String("0x0".into());
        });
        self
    }

    /// Sets `gas` to `0x0` on the unique transaction object for `tx_hash`.
    #[must_use]
    pub(crate) fn zero_transaction_gas(self, tx_hash: &str) -> Self {
        self.set_transaction_field(tx_hash, "gas", Value::String("0x0".into()))
    }

    /// Replaces the served `from` on `tx_hash` with `0x…dead`.
    ///
    /// A signed transaction's `from` is derived from the signature, so the
    /// tampered answer still hashes to the requested hash.
    #[must_use]
    pub(crate) fn reassign_transaction_sender(self, tx_hash: &str) -> Self {
        self.set_transaction_field(
            tx_hash,
            "from",
            Value::String("0x000000000000000000000000000000000000dead".into()),
        )
    }

    /// Sets one field on the unique transaction object for `tx_hash`.
    #[must_use]
    pub(crate) fn set_transaction_field(
        mut self,
        tx_hash: &str,
        field: &str,
        value: Value,
    ) -> Self {
        self.rewrite_transaction(tx_hash, |result| {
            assert!(result.is_object(), "expected a transaction object for {tx_hash}");
            result[field] = value;
        });
        self
    }

    /// Sets several fields on the unique transaction object for `tx_hash`.
    #[must_use]
    pub(crate) fn set_transaction_fields(
        mut self,
        tx_hash: &str,
        fields: &[(&str, Value)],
    ) -> Self {
        self.rewrite_transaction(tx_hash, |result| {
            assert!(result.is_object(), "expected a transaction object for {tx_hash}");
            for (field, value) in fields {
                result[*field] = value.clone();
            }
        });
        self
    }

    /// Rewrites the inclusion `blockHash` of a mined transaction lookup.
    #[must_use]
    pub(crate) fn set_transaction_block_hash(
        mut self,
        tx_hash: &str,
        block_hash: impl Into<Value>,
    ) -> Self {
        let block_hash = block_hash.into();
        self.rewrite_transaction(tx_hash, |result| {
            assert!(result.is_object(), "expected a transaction object for {tx_hash}");
            assert!(
                result.get("blockNumber").is_some_and(|n| !n.is_null()),
                "the captured transaction must report a block number"
            );
            result["blockHash"] = block_hash;
        });
        self
    }

    /// Rewrites the inclusion `blockNumber` of a transaction lookup that still
    /// carries an inclusion hash.
    #[must_use]
    pub(crate) fn set_transaction_block_number(
        mut self,
        tx_hash: &str,
        block_number: Value,
    ) -> Self {
        self.rewrite_transaction(tx_hash, |result| {
            assert!(result.is_object(), "expected a transaction object for {tx_hash}");
            assert!(
                result.get("blockHash").is_some_and(|h| !h.is_null()),
                "fixture transaction must report an inclusion hash"
            );
            assert!(
                result.get("blockNumber").is_some_and(|n| !n.is_null()),
                "fixture transaction must report a block number before doctoring"
            );
            result["blockNumber"] = block_number;
        });
        self
    }

    /// Marks the unique transaction lookup as pending (`blockNumber` and
    /// `blockHash` both null).
    #[must_use]
    pub(crate) fn mark_transaction_pending(self, tx_hash: &str) -> Self {
        self.set_transaction_fields(
            tx_hash,
            &[("blockNumber", Value::Null), ("blockHash", Value::Null)],
        )
    }

    /// Clones the unique transaction object for `src_hash`, rewrites it with
    /// `doctor`, and appends a new cache entry under `key`.
    #[must_use]
    pub(crate) fn push_cloned_transaction(
        mut self,
        src_hash: &str,
        key: &str,
        doctor: impl FnOnce(&mut Value),
    ) -> Self {
        let idx = self.transaction_index(src_hash);
        let mut response = parsed_response(&self.entries()[idx]);
        {
            let result = response.get_mut("result").expect("transaction result");
            assert!(
                result.get("hash").and_then(Value::as_str) == Some(src_hash),
                "offline cache must hold the source transaction object"
            );
            doctor(result);
        }
        self.entries_mut().push(serde_json::json!({
            "key": key,
            "value": response.to_string(),
        }));
        self
    }

    // --- Receipt operations -----------------------------------------------

    /// Rewrites the unique receipt result (the only cached response carrying
    /// `cumulativeGasUsed`).
    #[must_use]
    pub(crate) fn rewrite_receipt(mut self, doctor: impl FnOnce(&mut Value)) -> Self {
        let idx = self.receipt_index();
        self.rewrite_result_at(idx, doctor);
        self
    }

    /// Sets the unique receipt result to null.
    #[must_use]
    pub(crate) fn null_receipt(mut self) -> Self {
        let idx = self.receipt_index();
        self.rewrite_at(idx, |response| {
            response["result"] = Value::Null;
        });
        self
    }

    /// Removes the unique receipt entry.
    #[must_use]
    pub(crate) fn drop_receipt(mut self) -> Self {
        self.drop_one(is_receipt_entry, "receipt entry");
        self
    }

    // --- Block operations -------------------------------------------------

    /// Sets the unique block body at `number` to a null result.
    #[must_use]
    pub(crate) fn null_block(mut self, number: u64) -> Self {
        let idx = self.block_with_transactions_index(number);
        self.rewrite_at(idx, |response| {
            response["result"] = Value::Null;
        });
        self
    }

    /// Rewrites `gasLimit` on the unique block body at `number`.
    #[must_use]
    pub(crate) fn set_block_gas_limit(mut self, number: u64, gas_limit: u64) -> Self {
        let idx = self.block_with_transactions_index(number);
        self.rewrite_result_at(idx, |result| {
            result["gasLimit"] = Value::String(format!("0x{gas_limit:x}"));
        });
        self
    }

    /// Rewrites `hash` on the unique result object whose `number` is `number`.
    #[must_use]
    pub(crate) fn set_block_hash(mut self, number: u64, hash: &str) -> Self {
        let idx = self.numbered_object_index(number);
        self.rewrite_result_at(idx, |result| {
            assert!(
                result.get("hash").and_then(Value::as_str).is_some(),
                "block must report a hash"
            );
            result["hash"] = Value::String(hash.to_string());
        });
        self
    }

    /// Rewrites `hash` on the lower-numbered of the two captured block bodies.
    #[must_use]
    pub(crate) fn set_lowest_block_hash(mut self, hash: &str) -> Self {
        let idx = self.lowest_parent_block_index();
        self.rewrite_result_at(idx, |result| {
            result["hash"] = Value::String(hash.to_string());
        });
        self
    }

    /// Removes `tx_hashes` from the unique block body at `number`.
    #[must_use]
    pub(crate) fn remove_from_block_body(mut self, number: u64, tx_hashes: &[&str]) -> Self {
        let idx = self.find_one(
            |entry| block_lists_any(entry, number, tx_hashes),
            &format!("block body for {number} listing a target"),
        );
        self.rewrite_result_at(idx, |result| {
            let txs = result["transactions"].as_array_mut().expect("block transactions");
            txs.retain(|tx| !tx.as_str().is_some_and(|h| tx_hashes.contains(&h)));
        });
        self
    }

    /// Removes `tx_hash` from the unique block body that lists it, at any height.
    #[must_use]
    pub(crate) fn remove_from_listing_block(mut self, tx_hash: &str) -> Self {
        let idx = self
            .find_one(|entry| lists_tx(entry, tx_hash), &format!("block body listing {tx_hash}"));
        self.rewrite_result_at(idx, |result| {
            let txs = result["transactions"].as_array_mut().expect("block transactions");
            txs.retain(|hash| hash.as_str() != Some(tx_hash));
        });
        self
    }

    /// Keeps only transaction lookups for `tx_hashes` and the block body at
    /// `number`. Used to prove a path never touches parent, state, or body-tx
    /// objects.
    ///
    /// This is the exception to the module's exactly-one-hit contract: it
    /// filters the cache in one pass instead of locating a single entry.
    #[must_use]
    pub(crate) fn keep_only_transactions_and_block(
        mut self,
        tx_hashes: &[&str],
        number: u64,
    ) -> Self {
        self.entries_mut().retain(|entry| keep_tx_or_block(entry, tx_hashes, number));
        self
    }

    // --- Entry operations -------------------------------------------------

    /// Removes the unique cache entry whose key is `key`.
    #[must_use]
    pub(crate) fn drop_entry(mut self, key: &str) -> Self {
        self.drop_one(|entry| entry["key"].as_str() == Some(key), &format!("entry for {key}"));
        self
    }

    // --- Inspectors -------------------------------------------------------

    /// Returns a string field of the unique transaction object for `tx_hash`.
    pub(crate) fn transaction_str(&self, tx_hash: &str, field: &str) -> String {
        self.transaction_result(tx_hash)[field].as_str().expect("field is a string").to_string()
    }

    /// Returns `hash` of the unique result object whose `number` is `number`.
    pub(crate) fn block_hash_at(&self, number: u64) -> String {
        let idx = self.numbered_object_index(number);
        parsed_response(&self.entries()[idx])["result"]["hash"]
            .as_str()
            .expect("block hash")
            .to_string()
    }

    /// Returns `hash` of the lower-numbered of the two captured block bodies.
    pub(crate) fn lowest_block_hash(&self) -> String {
        let idx = self.lowest_parent_block_index();
        parsed_response(&self.entries()[idx])["result"]["hash"]
            .as_str()
            .expect("parent block hash")
            .to_string()
    }

    /// Returns `hash` of the unique block body that lists `tx_hash`.
    pub(crate) fn listing_block_hash(&self, tx_hash: &str) -> String {
        let idx = self
            .find_one(|entry| lists_tx(entry, tx_hash), &format!("block body listing {tx_hash}"));
        parsed_response(&self.entries()[idx])["result"]["hash"]
            .as_str()
            .expect("block hash")
            .to_string()
    }

    // --- Internals --------------------------------------------------------

    fn entries(&self) -> &[Value] {
        self.root["cache"].as_array().expect("cache entries")
    }

    fn entries_mut(&mut self) -> &mut Vec<Value> {
        self.root["cache"].as_array_mut().expect("cache entries")
    }

    fn find_one(&self, pred: impl Fn(&Value) -> bool, what: &str) -> usize {
        let hits: Vec<usize> = self
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| pred(entry))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(hits.len(), 1, "the envelope must hold exactly one {what}");
        hits[0]
    }

    fn drop_one(&mut self, pred: impl Fn(&Value) -> bool, what: &str) {
        let entries = self.entries_mut();
        let before = entries.len();
        entries.retain(|entry| !pred(entry));
        assert_eq!(before - entries.len(), 1, "the envelope must hold exactly one {what}");
    }

    fn rewrite_at(&mut self, index: usize, doctor: impl FnOnce(&mut Value)) {
        let entry = &mut self.entries_mut()[index];
        let mut response = parsed_response(entry);
        doctor(&mut response);
        entry["value"] = Value::String(response.to_string());
    }

    fn rewrite_result_at(&mut self, index: usize, doctor: impl FnOnce(&mut Value)) {
        self.rewrite_at(index, |response| {
            doctor(response.get_mut("result").expect("JSON-RPC result"));
        });
    }

    fn transaction_index(&self, tx_hash: &str) -> usize {
        self.find_one(
            |entry| is_transaction_for(entry, tx_hash),
            &format!("response for {tx_hash}"),
        )
    }

    fn rewrite_transaction(&mut self, tx_hash: &str, doctor: impl FnOnce(&mut Value)) {
        let idx = self.transaction_index(tx_hash);
        self.rewrite_result_at(idx, doctor);
    }

    fn transaction_result(&self, tx_hash: &str) -> Value {
        parsed_response(&self.entries()[self.transaction_index(tx_hash)])["result"].clone()
    }

    fn receipt_index(&self) -> usize {
        self.find_one(is_receipt_entry, "receipt entry")
    }

    fn block_with_transactions_index(&self, number: u64) -> usize {
        self.find_one(
            |entry| is_block_with_transactions(entry, number),
            &format!("header for block {number}"),
        )
    }

    fn numbered_object_index(&self, number: u64) -> usize {
        self.find_one(
            |entry| is_numbered_object(entry, number),
            &format!("block body for {number}"),
        )
    }

    fn lowest_parent_block_index(&self) -> usize {
        let mut blocks: Vec<(usize, u64)> = Vec::new();
        for (index, entry) in self.entries().iter().enumerate() {
            let Some(result) = result_object(entry) else {
                continue;
            };
            if result.get("parentHash").is_none() {
                continue;
            }
            let number = result["number"].as_str().expect("block number is a string");
            let number = parse_hex_u64(number).expect("block number is hex");
            blocks.push((index, number));
        }
        assert_eq!(blocks.len(), 2, "the capture must hold the replayed block and its parent");
        blocks.sort_unstable_by_key(|&(_, number)| number);
        blocks[0].0
    }
}

fn parsed_response(entry: &Value) -> Value {
    serde_json::from_str(entry["value"].as_str().expect("entry value is a string"))
        .expect("parse cached response")
}

/// Parsed JSON-RPC `result` object, if the cache value is a response whose
/// result is an object. Unparseable values and non-object results (`null`,
/// scalars) yield `None` rather than a substring hit.
fn result_object(entry: &Value) -> Option<Value> {
    let raw = entry.get("value")?.as_str()?;
    let response: Value = serde_json::from_str(raw).ok()?;
    let result = response.get("result")?;
    result.is_object().then(|| result.clone())
}

fn result_number(result: &Value) -> Option<u64> {
    result.get("number").and_then(Value::as_str).and_then(parse_hex_u64)
}

/// Transaction lookups carry `result.hash` plus `blockNumber`; block bodies
/// carry `number` instead. Matching the parsed fields ignores a nested `hash`
/// or an escaped mention of the same bytes in another string.
fn is_transaction_for(entry: &Value, tx_hash: &str) -> bool {
    result_object(entry).is_some_and(|result| {
        result.get("hash").and_then(Value::as_str) == Some(tx_hash) &&
            result.get("blockNumber").is_some()
    })
}

fn is_receipt_entry(entry: &Value) -> bool {
    result_object(entry).is_some_and(|result| result.get("cumulativeGasUsed").is_some())
}

fn is_block_with_transactions(entry: &Value, number: u64) -> bool {
    result_object(entry).is_some_and(|result| {
        result.get("transactions").is_some() && result_number(&result) == Some(number)
    })
}

fn is_numbered_object(entry: &Value, number: u64) -> bool {
    result_object(entry).is_some_and(|result| result_number(&result) == Some(number))
}

fn lists_tx(entry: &Value, tx_hash: &str) -> bool {
    result_object(entry).is_some_and(|result| {
        result
            .get("transactions")
            .and_then(Value::as_array)
            .is_some_and(|txs| txs.iter().any(|hash| hash.as_str() == Some(tx_hash)))
    })
}

fn block_lists_any(entry: &Value, number: u64, tx_hashes: &[&str]) -> bool {
    result_object(entry).is_some_and(|result| {
        result_number(&result) == Some(number) &&
            result.get("transactions").and_then(Value::as_array).is_some_and(|txs| {
                txs.iter().any(|tx| tx.as_str().is_some_and(|h| tx_hashes.contains(&h)))
            })
    })
}

fn keep_tx_or_block(entry: &Value, tx_hashes: &[&str], number: u64) -> bool {
    result_object(entry).is_some_and(|result| {
        let is_tx =
            result.get("hash").and_then(Value::as_str).is_some_and(|h| tx_hashes.contains(&h)) &&
                result.get("blockNumber").is_some();
        let is_block =
            result_number(&result) == Some(number) && result.get("transactions").is_some();
        is_tx || is_block
    })
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}
