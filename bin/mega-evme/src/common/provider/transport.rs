//! Transport-level RPC cache and the two transports that wrap it.
//!
//! Unlike alloy's provider-level `CacheLayer` (which only caches a subset of
//! methods it explicitly overrides), [`TransportCache`] captures single JSON-RPC
//! request/response pairs that pass through the transport — making it the
//! substrate for complete offline replay fixtures backing `--rpc.capture-file` /
//! `--rpc.replay-file`.
//!
//! Two dimensions govern what the cache holds:
//!
//! - **Capacity.** [`TransportCache::new`] is unbounded — capture and replay both hold a whole
//!   fixture in memory. [`TransportCache::with_max_entries`] bounds the entry count and evicts the
//!   least recently used entry on overflow. Neither form preallocates: a huge ceiling costs nothing
//!   until that many responses are really cached.
//! - **Policy.** [`CacheRole::decide`] answers "may this response be cached, and may it reach the
//!   envelope?" for one role. Every row (JSON-RPC error, null result, chain-tip method, block-tag
//!   params, pending transaction metadata) is stated once there and decided from the method name,
//!   the literal params and the response body alone, so each row is unit-testable and the two roles
//!   cannot drift apart by accident.

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_primitives::{keccak256, B256};
use alloy_provider::transport::{TransportError, TransportErrorKind};
use alloy_transport::TransportFut;
use serde::{Deserialize, Serialize};

use crate::common::{EvmeError, Result};

/// A single entry in the transport-level cache.
#[derive(Debug, Serialize, Deserialize)]
struct TransportCacheEntry {
    key: B256,
    value: String,
}

/// One cached response plus the bookkeeping eviction and persistence need.
#[derive(Debug)]
struct CacheSlot {
    /// Serialized JSON-RPC response.
    value: String,
    /// Whether this entry may be written to the on-disk envelope. An entry that
    /// is safe to reuse for the rest of the process but must not outlive it is
    /// held with `false`.
    persistable: bool,
    /// Recency stamp; the highest stamp is the most recently used entry.
    tick: u64,
}

/// Entry store behind [`TransportCache`]: a map plus a recency index.
///
/// The index makes eviction order explicit instead of leaving it to hash order,
/// and it is what keeps allocation proportional to the entries actually
/// inserted rather than to the configured ceiling.
#[derive(Debug, Default)]
struct CacheEntries {
    /// Cached responses by request key.
    slots: HashMap<B256, CacheSlot>,
    /// Recency index: stamps ascend, so the first key is the least recently used.
    recency: BTreeMap<u64, B256>,
    /// Stamp handed to the next read or write. Monotonic; exhausting `u64` needs
    /// 2^64 cache operations in a process that makes an RPC call per new entry.
    next_tick: u64,
    /// Entry ceiling. `None` means unbounded — capture and replay both hold a
    /// whole fixture and must not drop any of it.
    capacity: Option<NonZeroUsize>,
}

impl CacheEntries {
    /// Store that evicts once it holds more than `capacity` entries.
    fn bounded(capacity: NonZeroUsize) -> Self {
        Self { capacity: Some(capacity), ..Default::default() }
    }

    /// Take the next recency stamp.
    fn next_tick(&mut self) -> u64 {
        let tick = self.next_tick;
        self.next_tick += 1;
        tick
    }

    /// Read an entry, promoting it to most-recently-used.
    fn get(&mut self, key: &B256) -> Option<String> {
        // Stamp before the mutable borrow: `next_tick` borrows all of `self`.
        if !self.slots.contains_key(key) {
            return None;
        }
        let tick = self.next_tick();
        let slot = self.slots.get_mut(key).expect("presence checked above");
        let previous = core::mem::replace(&mut slot.tick, tick);
        let value = slot.value.clone();
        self.recency.remove(&previous);
        self.recency.insert(tick, *key);
        Some(value)
    }

    /// Insert or overwrite an entry, then evict down to the ceiling.
    fn put(&mut self, key: B256, value: String, persistable: bool) {
        let tick = self.next_tick();
        if let Some(previous) = self.slots.insert(key, CacheSlot { value, persistable, tick }) {
            self.recency.remove(&previous.tick);
        }
        self.recency.insert(tick, key);
        self.evict_to_capacity();
    }

    /// Insert only when the key is absent, leaving an existing entry untouched.
    fn put_if_absent(&mut self, key: B256, value: String, persistable: bool) {
        if !self.slots.contains_key(&key) {
            self.put(key, value, persistable);
        }
    }

    /// Drop least-recently-used entries until the ceiling is respected.
    fn evict_to_capacity(&mut self) {
        let Some(capacity) = self.capacity else { return };
        while self.slots.len() > capacity.get() {
            let Some((&tick, &key)) = self.recency.first_key_value() else { break };
            self.recency.remove(&tick);
            self.slots.remove(&key);
        }
    }
}

/// Transport-level RPC cache. Captures **single** JSON-RPC request/response
/// pairs, keyed by `keccak256(method + params)` (batch requests bypass the
/// cache).
///
/// Unlike alloy's provider-level `CacheLayer` (which only caches a subset
/// of methods it explicitly overrides), this captures every individual RPC
/// call that passes through the transport — making it suitable for building
/// complete offline replay fixtures.
#[derive(Debug, Clone, Default)]
pub(super) struct TransportCache {
    entries: Arc<RwLock<CacheEntries>>,
}

impl TransportCache {
    /// Unbounded cache: every entry is kept until the process exits.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Cache bounded to `max_entries` entries, evicting the least recently used
    /// entry on overflow.
    ///
    /// `0` maps through [`super::cache_max_entries_capacity`] to the same
    /// published ceiling as `--rpc.cache-max-entries 0` — a guard rail so a
    /// long-running process cannot grow without bound, not an allocation
    /// budget: the store allocates for the entries it actually holds, so even
    /// `u32::MAX` costs nothing at construction.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn with_max_entries(max_entries: u32) -> Self {
        let capacity = NonZeroUsize::new(super::cache_max_entries_capacity(max_entries) as usize)
            .expect("the capacity mapping never yields zero");
        Self { entries: Arc::new(RwLock::new(CacheEntries::bounded(capacity))) }
    }

    /// Read an entry, promoting it to most-recently-used.
    ///
    /// Takes the write lock: a read that does not promote would make the
    /// eviction order depend on writes alone.
    fn get(&self, key: &B256) -> Option<String> {
        self.entries.write().expect("cache lock poisoned").get(key)
    }

    /// Store a response. `persistable` decides whether it may reach the envelope.
    fn put(&self, key: B256, value: String, persistable: bool) {
        self.entries.write().expect("cache lock poisoned").put(key, value, persistable);
    }

    pub(super) fn len(&self) -> usize {
        self.entries.read().expect("cache lock poisoned").slots.len()
    }

    /// Merge entries from a serialized cache value. Existing entries are NOT
    /// overwritten — this preserves fresh responses (e.g. `eth_chainId`)
    /// that were fetched before the merge.
    ///
    /// Merged entries are persistable: they came from an envelope and must
    /// survive the rewrite that follows.
    pub(super) fn merge(&self, value: &serde_json::Value) -> Result<()> {
        let entries = parse_cache_entries(value)?;
        let mut store = self.entries.write().expect("cache lock poisoned");
        for entry in entries {
            store.put_if_absent(entry.key, entry.value, true);
        }
        Ok(())
    }

    /// Serve a cached response if the key is present. Deserializes the cached
    /// JSON, fixes up the response ID, and returns a ready future. Returns
    /// `None` on cache miss.
    fn try_serve(
        &self,
        key: &B256,
        request_id: alloy_json_rpc::Id,
    ) -> Option<TransportFut<'static>> {
        let cached = self.get(key)?;
        Some(Box::pin(async move {
            let mut resp: alloy_json_rpc::Response =
                serde_json::from_str(&cached).map_err(TransportErrorKind::custom)?;
            resp.id = request_id;
            Ok(ResponsePacket::Single(resp))
        }))
    }

    /// Serialize the persistable entries to a JSON value for the envelope.
    /// Sorted by key for deterministic output (avoids noisy diffs on committed fixtures).
    ///
    /// Entries the policy admitted for this process only are held back here —
    /// they answer the run that fetched them and are never written to disk.
    pub(super) fn to_value(&self) -> serde_json::Value {
        let mut entries: Vec<TransportCacheEntry> = self
            .entries
            .read()
            .expect("cache lock poisoned")
            .slots
            .iter()
            .filter(|(_, slot)| slot.persistable)
            .map(|(key, slot)| TransportCacheEntry { key: *key, value: slot.value.clone() })
            .collect();
        entries.sort_by_key(|e| e.key);
        serde_json::to_value(entries).expect("TransportCacheEntry is always serializable")
    }

    /// Deserialize from the envelope's `cache` field.
    ///
    /// Loaded entries are persistable: replay never writes, and any other reader
    /// of an envelope must be able to write back what it read.
    pub(super) fn from_value(value: &serde_json::Value) -> Result<Self> {
        let entries = parse_cache_entries(value)?;
        let cache = Self::new();
        {
            let mut store = cache.entries.write().expect("cache lock poisoned");
            for entry in entries {
                store.put(entry.key, entry.value, true);
            }
        }
        Ok(cache)
    }
}

/// Decode the envelope's `cache` array into entries.
fn parse_cache_entries(value: &serde_json::Value) -> Result<Vec<TransportCacheEntry>> {
    serde_json::from_value(value.clone())
        .map_err(|e| EvmeError::RpcError(format!("Failed to parse transport cache entries: {e}")))
}

/// Compute a deterministic cache key from an RPC method and params.
/// The request ID is deliberately excluded so the same logical request
/// produces the same key across runs.
fn transport_cache_key(method: &str, params: Option<&serde_json::value::RawValue>) -> B256 {
    let params_str = params.map_or("null", |p| p.get());
    keccak256(format!("{method}\x00{params_str}"))
}

/// Whether a success response carries a JSON `null` result.
///
/// A null is never load-bearing for offline replay — every consumer fails its
/// target or run on it — so capture must not bake it into the fixture. The
/// correct offline representation of "no answer" is a cache miss.
fn is_null_success_result(resp: &alloy_json_rpc::Response) -> bool {
    resp.payload.as_success().is_some_and(|raw| raw.get().trim() == "null")
}

/// Which build path a [`CachingTransport`] serves.
///
/// The role is the only dispatch dimension of the cache policy: both roles
/// agree on the rows that are never cached (JSON-RPC errors, null results) and
/// differ only on responses whose value holds just for "now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheRole {
    /// The online path, which reuses its cache across runs against the same
    /// endpoint.
    ///
    /// Not constructed in production yet: the online provider still caches at
    /// alloy's provider layer, which refuses block-tag requests outright and
    /// never sees most methods. These rows are what the same protection looks
    /// like once online caching moves down to the transport.
    #[cfg_attr(not(test), allow(dead_code))]
    Online,
    /// The `--rpc.capture-file` path, which writes a fixture meant to answer
    /// every request a later offline run repeats.
    Capture,
}

/// What the policy allows for one response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheDecision {
    /// Not cached at all: the caller still gets the response, the cache never
    /// sees it, and the next identical request goes back to the endpoint.
    Skip(SkipReason),
    /// Cached for this process only, never written to the envelope.
    ///
    /// Not produced by either role today. It is the shape of an answer that is
    /// worth reusing while one run is in flight but must not become a fixture
    /// entry, and the store and the envelope writer already honour it.
    #[cfg_attr(not(test), allow(dead_code))]
    MemoryOnly,
    /// Cached and eligible for the envelope.
    Persist,
}

impl CacheDecision {
    /// Whether an entry admitted under this decision may reach the envelope.
    fn persistable(self) -> bool {
        matches!(self, Self::Persist)
    }
}

/// Why a response was not cached. Carried by [`CacheDecision::Skip`] so the log
/// line names the row that rejected it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkipReason {
    /// The endpoint answered with a JSON-RPC `error` body.
    JsonRpcError,
    /// The endpoint answered with `"result": null`.
    NullResult,
    /// The method reports the chain tip, so its answer ages out with the next block.
    #[cfg_attr(not(test), allow(dead_code))]
    ChainTipMethod,
    /// The params name a moving block (`latest`, `pending`, …) rather than a fixed one.
    #[cfg_attr(not(test), allow(dead_code))]
    BlockTagParams,
    /// Transaction metadata for a transaction that is not in a block yet.
    #[cfg_attr(not(test), allow(dead_code))]
    PendingTransaction,
}

impl SkipReason {
    /// Log line explaining why the response was not cached.
    fn message(self) -> &'static str {
        match self {
            Self::JsonRpcError => "Skipping cache for JSON-RPC error response",
            Self::NullResult => "Skipping cache for JSON-RPC null result",
            Self::ChainTipMethod => "Skipping cache for a chain-tip method",
            Self::BlockTagParams => "Skipping cache for a block-tag request",
            Self::PendingTransaction => "Skipping cache for pending transaction metadata",
        }
    }
}

impl CacheRole {
    /// Decide what may be done with the response to `method` / `params`.
    ///
    /// Only the method name, the literal params and the response body are
    /// consulted — the transport has nothing else, and that keeps every row
    /// decidable without a live endpoint.
    fn decide(
        self,
        method: &str,
        params: &str,
        response: &alloy_json_rpc::Response,
    ) -> CacheDecision {
        // Rows that hold for every role.
        //
        // A JSON-RPC error body (e.g. a transient rate limit the endpoint
        // surfaces via `error` instead of an HTTP status) would otherwise be
        // replayed as a permanent failure. A `"result": null` (e.g.
        // `eth_getTransactionByHash` for a briefly invisible transaction) would
        // freeze "not found" with no in-tool recovery — `--rpc.clear-cache`
        // conflicts with capture mode. Offline, a cache miss names the exact
        // request instead.
        if response.is_error() {
            return CacheDecision::Skip(SkipReason::JsonRpcError);
        }
        if is_null_success_result(response) {
            return CacheDecision::Skip(SkipReason::NullResult);
        }

        // Rows whose answer is only true for "now".
        if is_chain_tip_method(method) {
            return self.on_unstable(SkipReason::ChainTipMethod);
        }
        if params_carry_block_tag(params) {
            return self.on_unstable(SkipReason::BlockTagParams);
        }
        if is_pending_transaction_metadata(method, response) {
            return self.on_unstable(SkipReason::PendingTransaction);
        }

        CacheDecision::Persist
    }

    /// How this role treats a response that is only true for "now".
    fn on_unstable(self, reason: SkipReason) -> CacheDecision {
        match self {
            // Online: reusing such an answer pins "now" for the rest of the
            // process and, once persisted, for every later run sharing the cache
            // file — a transaction seen while pending would stay pending, and a
            // `latest` read would stay at the height it was first taken.
            Self::Online => CacheDecision::Skip(reason),
            // Capture: a fixture has to answer every request the offline run
            // repeats, and replaying a pending transaction needs exactly these
            // rows — the pending metadata that selects the pending path, and the
            // chain-tip read that path makes to pick its block. Dropping them
            // would make the fixture unusable for the case it exists to cover.
            Self::Capture => CacheDecision::Persist,
        }
    }
}

/// Whether the method's result *is* the chain tip, so any answer ages out with
/// the next block regardless of params.
fn is_chain_tip_method(method: &str) -> bool {
    method == "eth_blockNumber"
}

/// Block tags of the JSON-RPC block-parameter grammar. A request naming one of
/// them asks about a moving target, so its answer belongs to the moment it was
/// taken.
const BLOCK_TAGS: [&str; 5] = ["latest", "pending", "earliest", "safe", "finalized"];

/// Whether the literal params name a block tag anywhere inside them.
///
/// The scan is recursive because the tag can sit at any depth: `eth_getBalance`
/// puts it beside the address, `eth_getLogs` puts it in the filter object.
fn params_carry_block_tag(params: &str) -> bool {
    // Params reach the transport already serialized by the request builder, so
    // an unreadable value means a hand-built request. Classify it as a tag: the
    // answer that never freezes a moving target into the cache.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(params) else { return true };
    json_carries_block_tag(&value)
}

/// Recursive half of [`params_carry_block_tag`].
fn json_carries_block_tag(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => BLOCK_TAGS.contains(&text.as_str()),
        serde_json::Value::Array(items) => items.iter().any(json_carries_block_tag),
        serde_json::Value::Object(fields) => fields.values().any(json_carries_block_tag),
        _ => false,
    }
}

/// Whether `response` describes a transaction that is not in a block yet.
///
/// A pending answer carries a null `blockNumber` / `blockHash`, which is what
/// every consumer keys its pending handling off. Reusing it reports the
/// transaction as pending long after it landed.
fn is_pending_transaction_metadata(method: &str, response: &alloy_json_rpc::Response) -> bool {
    if method != "eth_getTransactionByHash" {
        return false;
    }
    let Some(raw) = response.payload.as_success() else { return false };
    let Ok(serde_json::Value::Object(tx)) = serde_json::from_str::<serde_json::Value>(raw.get())
    else {
        return false;
    };
    tx.get("blockNumber").is_none_or(serde_json::Value::is_null) ||
        tx.get("blockHash").is_none_or(serde_json::Value::is_null)
}

/// Transport wrapper that records JSON-RPC responses into a [`TransportCache`]
/// under a [`CacheRole`]'s policy. Cache hits are served locally, misses are
/// forwarded to the inner transport and the response is offered to the policy.
#[derive(Debug, Clone)]
pub(super) struct CachingTransport<T> {
    inner: T,
    cache: TransportCache,
    role: CacheRole,
}

impl<T> CachingTransport<T> {
    /// Transport for the `--rpc.capture-file` path.
    pub(super) fn new(inner: T, cache: TransportCache) -> Self {
        Self::with_role(inner, cache, CacheRole::Capture)
    }

    /// Transport running an explicitly chosen policy role.
    pub(super) fn with_role(inner: T, cache: TransportCache, role: CacheRole) -> Self {
        Self { inner, cache, role }
    }
}

impl<T> tower::Service<RequestPacket> for CachingTransport<T>
where
    T: tower::Service<
            RequestPacket,
            Response = ResponsePacket,
            Error = TransportError,
            Future = TransportFut<'static>,
        > + Send
        + Sync
        + Clone
        + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        if let RequestPacket::Single(ref r) = req {
            let key = transport_cache_key(r.method(), r.params());

            if let Some(fut) = self.cache.try_serve(&key, r.id().clone()) {
                return fut;
            }

            // Cache miss: forward to inner and let the role's policy decide what
            // may be kept. The method and params are copied out here because the
            // request itself moves into the inner call.
            let cache = self.cache.clone();
            let role = self.role;
            let method = r.method().to_string();
            let params = r.params().map_or_else(|| "null".to_string(), |p| p.get().to_string());
            let fut = self.inner.call(req);
            return Box::pin(async move {
                let response = fut.await?;
                if let ResponsePacket::Single(ref resp) = response {
                    match role.decide(&method, &params, resp) {
                        CacheDecision::Skip(reason) => {
                            tracing::warn!(method = %method, "{}", reason.message());
                        }
                        decision => {
                            if let Ok(serialized) = serde_json::to_string(resp) {
                                cache.put(key, serialized, decision.persistable());
                            }
                        }
                    }
                }
                Ok(response)
            });
        }

        // Batch: forward without caching. alloy's default provider never batches,
        // so uncached batch responses won't cause offline replay failures in practice.
        self.inner.call(req)
    }
}

/// A transport that serves RPC responses from a [`TransportCache`] and
/// returns `BackendGone` on cache miss. Used for offline replay — no
/// network I/O, no retry layer.
#[derive(Debug, Clone)]
pub(super) struct ReplayTransport {
    cache_file_path: PathBuf,
    cache: TransportCache,
}

impl ReplayTransport {
    pub(super) fn new(cache_file_path: PathBuf, cache: TransportCache) -> Self {
        Self { cache_file_path, cache }
    }
}

impl tower::Service<RequestPacket> for ReplayTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        if let RequestPacket::Single(ref r) = req {
            let key = transport_cache_key(r.method(), r.params());

            if let Some(fut) = self.cache.try_serve(&key, r.id().clone()) {
                return fut;
            }

            // Cache miss: build a descriptive error so the user knows which RPC
            // response is missing and how to refresh the fixture.
            let path = self.cache_file_path.display().to_string();
            let method = r.method().to_string();
            let params = r.params().map_or_else(|| "null".to_string(), |p| p.get().to_string());
            let msg = format!(
                "cache miss in offline replay file '{path}': method={method}, params={params}\n\
                 hint: re-capture with `mega-evme replay <tx> --rpc <URL> --rpc.capture-file {path}`"
            );
            // Custom error so the message propagates. Safe: retry layer is never
            // installed on the replay path.
            return Box::pin(async move { Err(TransportErrorKind::custom_str(&msg)) });
        }

        // Batch miss: BackendGone (not Custom, avoids retry).
        Box::pin(async { Err(TransportErrorKind::backend_gone()) })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::{Context, Poll},
    };

    use alloy_json_rpc::{Id, Response, ResponsePayload};
    use alloy_provider::transport::RpcError;
    use serde_json::value::to_raw_value;
    use tower::Service;

    use super::*;

    /// Inner transport that returns a fixed single response for every call.
    #[derive(Clone)]
    struct FixedResponseTransport {
        response: Arc<Response>,
        calls: Arc<AtomicUsize>,
    }

    impl FixedResponseTransport {
        fn new(response: Response) -> Self {
            Self { response: Arc::new(response), calls: Arc::new(AtomicUsize::new(0)) }
        }
    }

    impl Service<RequestPacket> for FixedResponseTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: RequestPacket) -> Self::Future {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let response = ResponsePacket::Single((*self.response).clone());
            Box::pin(async move { Ok(response) })
        }
    }

    fn success_response(result_json: &str) -> Response {
        Response {
            id: Id::Number(1),
            payload: ResponsePayload::Success(
                to_raw_value(&serde_json::from_str::<serde_json::Value>(result_json).unwrap())
                    .unwrap(),
            ),
        }
    }

    fn serialized_request(method: &'static str) -> alloy_json_rpc::SerializedRequest {
        alloy_json_rpc::Request::new(method, Id::Number(1), ())
            .serialize()
            .expect("serialize request")
    }

    fn single_request(method: &'static str) -> RequestPacket {
        RequestPacket::Single(serialized_request(method))
    }

    fn single_request_with_params(
        method: &'static str,
        params: serde_json::Value,
    ) -> RequestPacket {
        RequestPacket::Single(
            alloy_json_rpc::Request::new(method, Id::Number(1), params)
                .serialize()
                .expect("serialize request"),
        )
    }

    /// Distinct cache key for a test entry.
    fn key(seed: &str) -> B256 {
        keccak256(seed)
    }

    /// Heap slots the entry map has actually reserved. The observable stand-in
    /// for "does a huge ceiling cost memory before it is used?".
    fn allocated_slots(cache: &TransportCache) -> usize {
        cache.entries.read().expect("cache lock poisoned").slots.capacity()
    }

    /// The configured entry ceiling, `None` when unbounded.
    fn configured_capacity(cache: &TransportCache) -> Option<usize> {
        cache.entries.read().expect("cache lock poisoned").capacity.map(NonZeroUsize::get)
    }

    /// A transaction-metadata response for a transaction that is not mined yet.
    fn pending_transaction_response() -> Response {
        success_response(r#"{"hash":"0x01","blockNumber":null,"blockHash":null}"#)
    }

    /// The same metadata once the transaction has landed in a block.
    fn mined_transaction_response() -> Response {
        success_response(r#"{"hash":"0x01","blockNumber":"0x10","blockHash":"0x02"}"#)
    }

    /// The replay transport is always ready.
    /// Single-request cache misses return a descriptive Custom error (safe because
    /// the retry layer is never installed on the replay path).
    /// Batch misses return `BackendGone`.
    #[tokio::test]
    async fn test_replay_transport_cache_miss() {
        let mut transport =
            ReplayTransport::new(PathBuf::from("/tmp/test.cache.json"), TransportCache::new());

        // poll_ready should be Ready(Ok(())).
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(transport.poll_ready(&mut cx).is_ready());

        // Single request: cache miss should include method and fixture path.
        let req =
            alloy_json_rpc::Request::new("eth_blockNumber", alloy_json_rpc::Id::Number(1), ())
                .serialize()
                .expect("serialize request");
        let result = transport.call(RequestPacket::Single(req)).await;
        assert!(result.is_err(), "cache miss must error");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("cache miss"), "error should say 'cache miss': {msg}");
        assert!(msg.contains("eth_blockNumber"), "error should include method: {msg}");
        assert!(msg.contains("test.cache.json"), "error should include fixture path: {msg}");
    }

    /// A success with a non-null result is cached and served on the next call.
    #[tokio::test]
    async fn test_caching_transport_caches_non_null_success() {
        let inner = FixedResponseTransport::new(success_response(r#""0x42""#));
        let calls = Arc::clone(&inner.calls);
        let cache = TransportCache::new();
        let mut transport = CachingTransport::new(inner, cache.clone());

        let first = transport.call(single_request("eth_blockNumber")).await.expect("first call");
        assert!(matches!(first, ResponsePacket::Single(_)));
        assert_eq!(cache.len(), 1, "non-null success must be cached");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call must hit the cache and not touch the inner transport.
        let second = transport.call(single_request("eth_blockNumber")).await.expect("cache hit");
        assert!(matches!(second, ResponsePacket::Single(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "cache hit must not call inner");
    }

    /// A success with `"result": null` is returned to the caller but never put
    /// in the cache — absent entry stays absent.
    #[tokio::test]
    async fn test_caching_transport_skips_null_result() {
        let inner = FixedResponseTransport::new(success_response("null"));
        let calls = Arc::clone(&inner.calls);
        let cache = TransportCache::new();
        let mut transport = CachingTransport::new(inner, cache.clone());

        let response = transport.call(single_request("eth_getTransactionByHash")).await;
        let response = response.expect("null result is still a transport success");
        match response {
            ResponsePacket::Single(resp) => {
                assert!(is_null_success_result(&resp), "caller must receive the null unchanged");
            }
            other => panic!("expected single response, got {other:?}"),
        }
        assert_eq!(cache.len(), 0, "null result must not be cached");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A second call is still a miss and re-forwards — never a cached null.
        let _ = transport.call(single_request("eth_getTransactionByHash")).await;
        assert_eq!(cache.len(), 0, "repeated nulls must still leave the cache empty");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "null must not create a cache hit");
    }

    /// A null response must never reach `put`, so an existing non-null entry
    /// for the same key is preserved even if the live endpoint later returns null.
    ///
    /// The production path usually serves the existing entry as a hit before the
    /// network is consulted; this test forces the miss→null path after a prior
    /// put by clearing nothing and re-issuing through a transport whose inner
    /// now returns null, after first caching a non-null under the same key via
    /// a separate `CachingTransport` sharing the cache.
    #[tokio::test]
    async fn test_caching_transport_null_does_not_overwrite_existing_entry() {
        let cache = TransportCache::new();

        // Seed a non-null entry under eth_blockNumber.
        let seed_inner = FixedResponseTransport::new(success_response(r#""0x42""#));
        let mut seeder = CachingTransport::new(seed_inner, cache.clone());
        seeder.call(single_request("eth_blockNumber")).await.expect("seed");
        assert_eq!(cache.len(), 1);

        // A second CachingTransport sharing the same cache: on a hit the null
        // never reaches put. Serve the hit and assert the entry is unchanged.
        let null_inner = FixedResponseTransport::new(success_response("null"));
        let null_calls = Arc::clone(&null_inner.calls);
        let mut transport = CachingTransport::new(null_inner, cache.clone());
        let served = transport.call(single_request("eth_blockNumber")).await.expect("hit");
        match served {
            ResponsePacket::Single(resp) => {
                assert!(
                    !is_null_success_result(&resp),
                    "existing non-null entry must be served, not overwritten by a live null",
                );
            }
            other => panic!("expected single response, got {other:?}"),
        }
        assert_eq!(null_calls.load(Ordering::SeqCst), 0, "cache hit must not call inner");
        assert_eq!(cache.len(), 1, "null path must never drop the existing entry");

        // Inspect the cached payload is still the non-null success.
        let key = transport_cache_key("eth_blockNumber", None);
        let cached = cache.get(&key).expect("entry present");
        let cached_resp: Response = serde_json::from_str(&cached).expect("cached JSON");
        assert!(!is_null_success_result(&cached_resp));
    }

    /// JSON-RPC error responses are still skipped (regression pin).
    #[tokio::test]
    async fn test_caching_transport_skips_error_response() {
        let error = Response { id: Id::Number(1), payload: ResponsePayload::internal_error() };
        let inner = FixedResponseTransport::new(error);
        let cache = TransportCache::new();
        let mut transport = CachingTransport::new(inner, cache.clone());

        let _ = transport.call(single_request("eth_blockNumber")).await;
        assert_eq!(cache.len(), 0, "error responses must not be cached");
    }

    #[test]
    fn test_is_null_success_result_detects_null_only() {
        assert!(is_null_success_result(&success_response("null")));
        assert!(!is_null_success_result(&success_response(r#""0x1""#)));
        assert!(!is_null_success_result(&success_response("0")));
        assert!(!is_null_success_result(&success_response("{}")));
        let error = Response { id: Id::Number(1), payload: ResponsePayload::internal_error() };
        assert!(!is_null_success_result(&error));
    }

    // ── Capacity and eviction ───────────────────────────────────────────────

    /// The default cache is unbounded: capture and replay hold whole fixtures
    /// and must not drop entries behind the caller's back.
    #[test]
    fn test_transport_cache_default_is_unbounded() {
        let cache = TransportCache::new();
        assert_eq!(configured_capacity(&cache), None, "the default cache has no ceiling");

        for i in 0..1_000 {
            cache.put(key(&format!("entry-{i}")), format!("value-{i}"), true);
        }
        assert_eq!(cache.len(), 1_000, "an unbounded cache keeps every entry");
        assert_eq!(cache.get(&key("entry-0")).as_deref(), Some("value-0"));
    }

    /// A bounded cache holds at most `N` entries and drops the least recently
    /// used one on overflow.
    #[test]
    fn test_transport_cache_evicts_the_least_recently_used_entry() {
        let cache = TransportCache::with_max_entries(2);
        assert_eq!(configured_capacity(&cache), Some(2));

        cache.put(key("a"), "A".to_string(), true);
        cache.put(key("b"), "B".to_string(), true);
        cache.put(key("c"), "C".to_string(), true);

        assert_eq!(cache.len(), 2, "the ceiling is exact, not approximate");
        assert_eq!(cache.get(&key("a")), None, "the oldest entry is the one evicted");
        assert_eq!(cache.get(&key("b")).as_deref(), Some("B"));
        assert_eq!(cache.get(&key("c")).as_deref(), Some("C"));

        // An evicted entry is gone from the envelope too.
        let entries = cache.to_value();
        let entries = entries.as_array().expect("cache serializes to an array");
        assert_eq!(entries.len(), 2);
    }

    /// A read promotes its entry, so the next eviction takes a different one.
    #[test]
    fn test_transport_cache_get_promotes_the_entry() {
        let cache = TransportCache::with_max_entries(2);
        cache.put(key("a"), "A".to_string(), true);
        cache.put(key("b"), "B".to_string(), true);

        assert_eq!(cache.get(&key("a")).as_deref(), Some("A"), "read makes 'a' the most recent");
        cache.put(key("c"), "C".to_string(), true);

        assert_eq!(cache.get(&key("a")).as_deref(), Some("A"), "the promoted entry survives");
        assert_eq!(cache.get(&key("b")), None, "the entry not read since insertion is evicted");
        assert_eq!(cache.get(&key("c")).as_deref(), Some("C"));
    }

    /// Overwriting a key updates it in place: the entry count cannot grow, and
    /// the rewrite also counts as a use.
    #[test]
    fn test_transport_cache_overwriting_a_key_does_not_evict() {
        let cache = TransportCache::with_max_entries(2);
        cache.put(key("a"), "A".to_string(), true);
        cache.put(key("a"), "A2".to_string(), true);
        cache.put(key("b"), "B".to_string(), true);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&key("a")).as_deref(), Some("A2"), "the newer value wins");
        assert_eq!(cache.get(&key("b")).as_deref(), Some("B"));
    }

    /// `0` keeps the published `--rpc.cache-max-entries 0` meaning: a 2^20-entry
    /// guard rail rather than a silently ignored request.
    #[test]
    fn test_transport_cache_zero_max_entries_uses_the_published_cap() {
        let cache = TransportCache::with_max_entries(0);
        assert_eq!(configured_capacity(&cache), Some(1_048_576));
        assert_eq!(
            configured_capacity(&cache),
            Some(super::super::cache_max_entries_capacity(0) as usize),
            "the memory cache and the flag share one mapping",
        );
    }

    /// Construction allocates nothing, and filling a cache built with an
    /// enormous ceiling allocates exactly as much as the same entries in a tiny
    /// one. This is what keeps `--rpc.cache-max-entries <huge>` from ballooning
    /// RSS the way a preallocating index does.
    #[test]
    fn test_transport_cache_does_not_preallocate_for_a_huge_capacity() {
        let started = std::time::Instant::now();

        let huge = TransportCache::with_max_entries(u32::MAX);
        let unlimited = TransportCache::with_max_entries(0);
        let small = TransportCache::with_max_entries(8);
        assert_eq!(allocated_slots(&huge), 0, "construction must not allocate a single slot");
        assert_eq!(allocated_slots(&unlimited), 0);
        assert_eq!(allocated_slots(&small), 0);

        for cache in [&huge, &unlimited, &small] {
            for i in 0..4 {
                cache.put(key(&format!("entry-{i}")), format!("value-{i}"), true);
            }
            assert_eq!(cache.get(&key("entry-3")).as_deref(), Some("value-3"), "readable back");
        }

        let (huge_slots, unlimited_slots, small_slots) =
            (allocated_slots(&huge), allocated_slots(&unlimited), allocated_slots(&small));
        assert_eq!(huge_slots, small_slots, "allocation follows the entries, not the ceiling");
        assert_eq!(unlimited_slots, small_slots);
        assert!(huge_slots < 64, "four entries must not reserve more than a handful of slots");

        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "a preallocating index would spend minutes here; took {elapsed:?}",
        );
        println!(
            "capacity u32::MAX -> {huge_slots} slots, 0 (2^20) -> {unlimited_slots} slots, \
             8 -> {small_slots} slots after 4 entries; elapsed {elapsed:?}",
        );
    }

    // ── Persistence of individual entries ───────────────────────────────────

    /// Only a `Persist` decision may reach the envelope; a memory-only entry is
    /// admitted to the cache but held back from it.
    #[test]
    fn test_cache_decision_persistability() {
        assert!(CacheDecision::Persist.persistable());
        assert!(!CacheDecision::MemoryOnly.persistable());
        assert!(!CacheDecision::Skip(SkipReason::NullResult).persistable());
    }

    /// A memory-only entry answers this process and never reaches the envelope.
    #[test]
    fn test_transport_cache_memory_only_entry_is_served_but_not_persisted() {
        let cache = TransportCache::new();
        cache.put(key("mem"), "in-memory".to_string(), false);
        cache.put(key("disk"), "on-disk".to_string(), true);

        assert_eq!(cache.get(&key("mem")).as_deref(), Some("in-memory"), "served from memory");
        assert_eq!(cache.len(), 2, "both entries occupy the cache");

        let entries = cache.to_value();
        let entries = entries.as_array().expect("cache serializes to an array");
        assert_eq!(entries.len(), 1, "only the persistable entry is written out");
        assert_eq!(entries[0]["value"], "on-disk");
    }

    /// Entries that came from an envelope stay persistable, so loading and
    /// writing back is lossless.
    #[test]
    fn test_transport_cache_entries_from_an_envelope_stay_persistable() {
        let seeded = serde_json::json!([
            {"key": key("a"), "value": "A"},
            {"key": key("b"), "value": "B"},
        ]);

        let loaded = TransportCache::from_value(&seeded).expect("from_value");
        assert_eq!(loaded.to_value(), seeded, "a load/save round trip is lossless");

        let merged = TransportCache::new();
        merged.put(key("a"), "fresh".to_string(), true);
        merged.merge(&seeded).expect("merge");
        assert_eq!(merged.get(&key("a")).as_deref(), Some("fresh"), "merge keeps fresh entries");
        assert_eq!(merged.get(&key("b")).as_deref(), Some("B"));
        let entries = merged.to_value();
        assert_eq!(entries.as_array().expect("array").len(), 2, "merged entries are persistable");
    }

    /// A committed fixture survives a load/save round trip unchanged: same
    /// entries, same order, same values. This is what keeps a re-capture from
    /// rewriting fixtures that no one intended to touch.
    #[test]
    fn test_committed_fixture_round_trips_unchanged() {
        let path =
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay_offline_smoke.cache.json");
        let envelope: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read fixture"))
                .expect("fixture is JSON");
        let entries = &envelope["cache"];
        assert!(entries.as_array().is_some_and(|e| e.len() > 1), "fixture has entries to compare");

        let loaded = TransportCache::from_value(entries).expect("from_value");
        assert_eq!(&loaded.to_value(), entries, "load/save must not alter a committed fixture");
    }

    // ── Policy rows ─────────────────────────────────────────────────────────

    /// JSON-RPC error bodies are never cached, in any role; the same call with a
    /// success body is.
    #[test]
    fn test_policy_skips_error_responses_in_every_role() {
        let error = Response { id: Id::Number(1), payload: ResponsePayload::internal_error() };
        let success = success_response(r#""0x10e6""#);
        for role in [CacheRole::Online, CacheRole::Capture] {
            assert_eq!(
                role.decide("eth_chainId", "null", &error),
                CacheDecision::Skip(SkipReason::JsonRpcError),
                "{role:?} must not cache an error body",
            );
            assert_eq!(
                role.decide("eth_chainId", "null", &success),
                CacheDecision::Persist,
                "{role:?} must cache the successful answer to the same call",
            );
        }
    }

    /// `"result": null` is never cached, in any role; a non-null result is.
    #[test]
    fn test_policy_skips_null_results_in_every_role() {
        let null = success_response("null");
        let found = mined_transaction_response();
        for role in [CacheRole::Online, CacheRole::Capture] {
            assert_eq!(
                role.decide("eth_getTransactionReceipt", r#"["0x01"]"#, &null),
                CacheDecision::Skip(SkipReason::NullResult),
                "{role:?} must not cache a null result",
            );
            assert_eq!(
                role.decide("eth_getTransactionReceipt", r#"["0x01"]"#, &found),
                CacheDecision::Persist,
                "{role:?} must cache a real result for the same call",
            );
        }
    }

    /// Online: a method that reports the chain tip is never cached; a method
    /// whose answer is fixed for the chain is.
    #[test]
    fn test_policy_online_skips_the_chain_tip_method() {
        let answer = success_response(r#""0x42""#);
        assert_eq!(
            CacheRole::Online.decide("eth_blockNumber", "null", &answer),
            CacheDecision::Skip(SkipReason::ChainTipMethod),
        );
        assert_eq!(
            CacheRole::Online.decide("eth_chainId", "null", &answer),
            CacheDecision::Persist,
            "a stable method with the same answer shape must still be cached",
        );
    }

    /// Online: params naming a moving block are not cached; the same request
    /// pinned to a concrete height is.
    #[test]
    fn test_policy_online_skips_block_tag_params() {
        let balance = success_response(r#""0x1""#);
        assert_eq!(
            CacheRole::Online.decide("eth_getBalance", r#"["0xabc","latest"]"#, &balance),
            CacheDecision::Skip(SkipReason::BlockTagParams),
        );
        assert_eq!(
            CacheRole::Online.decide("eth_getBalance", r#"["0xabc","0x1234"]"#, &balance),
            CacheDecision::Persist,
            "a request pinned to a block number is stable and must be cached",
        );
    }

    /// Online: pending transaction metadata is not cached; the same lookup for a
    /// mined transaction is.
    #[test]
    fn test_policy_online_skips_pending_transaction_metadata() {
        assert_eq!(
            CacheRole::Online.decide(
                "eth_getTransactionByHash",
                r#"["0x01"]"#,
                &pending_transaction_response(),
            ),
            CacheDecision::Skip(SkipReason::PendingTransaction),
        );
        assert_eq!(
            CacheRole::Online.decide(
                "eth_getTransactionByHash",
                r#"["0x01"]"#,
                &mined_transaction_response(),
            ),
            CacheDecision::Persist,
            "a mined transaction's metadata never changes again",
        );
    }

    /// Capture keeps the three unstable rows: an offline replay of a pending
    /// transaction needs its metadata and the chain-tip read that follows it,
    /// so a fixture that dropped them could not answer the run it exists for.
    /// The rows that are never cached stay skipped.
    #[test]
    fn test_policy_capture_keeps_the_unstable_rows() {
        let role = CacheRole::Capture;
        assert_eq!(
            role.decide("eth_blockNumber", "null", &success_response(r#""0x42""#)),
            CacheDecision::Persist,
        );
        assert_eq!(
            role.decide("eth_getBalance", r#"["0xabc","latest"]"#, &success_response(r#""0x1""#)),
            CacheDecision::Persist,
        );
        assert_eq!(
            role.decide("eth_getTransactionByHash", r#"["0x01"]"#, &pending_transaction_response()),
            CacheDecision::Persist,
        );
        assert_eq!(
            role.decide("eth_getTransactionByHash", r#"["0x01"]"#, &success_response("null")),
            CacheDecision::Skip(SkipReason::NullResult),
            "the always-skipped rows still apply to capture",
        );
    }

    /// The tag scan reaches tags wherever the grammar puts them, and treats an
    /// unreadable request as a moving target.
    #[test]
    fn test_params_carry_block_tag_scans_nested_values() {
        assert!(params_carry_block_tag(r#"["0xabc","latest"]"#));
        assert!(params_carry_block_tag(r#"["0xabc","pending"]"#));
        assert!(params_carry_block_tag(r#"["0xabc","earliest"]"#));
        assert!(params_carry_block_tag(r#"["0xabc","safe"]"#));
        assert!(params_carry_block_tag(r#"["0xabc","finalized"]"#));
        assert!(
            params_carry_block_tag(r#"[{"fromBlock":"0x1","toBlock":"latest"}]"#),
            "a tag inside a filter object is still a tag",
        );

        assert!(!params_carry_block_tag(r#"["0xabc","0x1234"]"#));
        assert!(!params_carry_block_tag(r#"[{"fromBlock":"0x1","toBlock":"0x2"}]"#));
        assert!(!params_carry_block_tag("null"), "a request without params names no block");
        assert!(!params_carry_block_tag("[]"));

        assert!(params_carry_block_tag("not json"), "an unreadable request is treated as a tag");
    }

    /// Pending detection is scoped to transaction lookups and to the fields that
    /// actually mark a transaction as unmined.
    #[test]
    fn test_is_pending_transaction_metadata_scope() {
        let pending = pending_transaction_response();
        assert!(is_pending_transaction_metadata("eth_getTransactionByHash", &pending));
        assert!(
            !is_pending_transaction_metadata("eth_getTransactionReceipt", &pending),
            "only the transaction lookup carries this shape of metadata",
        );
        assert!(!is_pending_transaction_metadata(
            "eth_getTransactionByHash",
            &mined_transaction_response(),
        ));
        assert!(
            is_pending_transaction_metadata(
                "eth_getTransactionByHash",
                &success_response(r#"{"hash":"0x01","blockNumber":"0x10"}"#),
            ),
            "a missing inclusion field is as unmined as a null one",
        );
        assert!(
            !is_pending_transaction_metadata(
                "eth_getTransactionByHash",
                &success_response(r#""0x1""#),
            ),
            "a non-object result carries no inclusion metadata",
        );
    }

    // ── Policy rows through the transport ───────────────────────────────────

    /// Online: every chain-tip read reaches the endpoint, and nothing is cached.
    #[tokio::test]
    async fn test_caching_transport_online_role_does_not_cache_the_chain_tip() {
        let inner = FixedResponseTransport::new(success_response(r#""0x42""#));
        let calls = Arc::clone(&inner.calls);
        let cache = TransportCache::new();
        let mut transport = CachingTransport::with_role(inner, cache.clone(), CacheRole::Online);

        transport.call(single_request("eth_blockNumber")).await.expect("first call");
        assert_eq!(cache.len(), 0, "the chain tip must not enter the cache");

        transport.call(single_request("eth_blockNumber")).await.expect("second call");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "every tip read must reach the endpoint");
        assert_eq!(cache.len(), 0);
    }

    /// Online: a stable answer is cached and served on the next call.
    #[tokio::test]
    async fn test_caching_transport_online_role_caches_a_stable_response() {
        let inner = FixedResponseTransport::new(success_response(r#""0x10e6""#));
        let calls = Arc::clone(&inner.calls);
        let cache = TransportCache::new();
        let mut transport = CachingTransport::with_role(inner, cache.clone(), CacheRole::Online);

        transport.call(single_request("eth_chainId")).await.expect("first call");
        assert_eq!(cache.len(), 1, "a stable answer is cached");

        transport.call(single_request("eth_chainId")).await.expect("cache hit");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second call is served from the cache");
    }

    /// Online: a tag request is refused by the cache while the same request
    /// pinned to a height is kept.
    #[tokio::test]
    async fn test_caching_transport_online_role_does_not_cache_a_block_tag_request() {
        let inner = FixedResponseTransport::new(success_response(r#""0x1""#));
        let cache = TransportCache::new();
        let mut transport = CachingTransport::with_role(inner, cache.clone(), CacheRole::Online);

        transport
            .call(single_request_with_params(
                "eth_getBalance",
                serde_json::json!(["0xabc", "latest"]),
            ))
            .await
            .expect("tag request");
        assert_eq!(cache.len(), 0, "a 'latest' read must not be cached");

        transport
            .call(single_request_with_params(
                "eth_getBalance",
                serde_json::json!(["0xabc", "0x1234"]),
            ))
            .await
            .expect("pinned request");
        assert_eq!(cache.len(), 1, "the same read pinned to a height is cached");
    }

    /// Capture keeps what the online role refuses — the fixture has to answer
    /// the offline rerun, including its chain-tip and pending reads — and what
    /// it keeps is written out.
    #[tokio::test]
    async fn test_caching_transport_capture_role_keeps_the_unstable_rows() {
        let inner = FixedResponseTransport::new(success_response(r#""0x42""#));
        let cache = TransportCache::new();
        let mut transport = CachingTransport::new(inner, cache.clone());

        transport.call(single_request("eth_blockNumber")).await.expect("call");

        assert_eq!(cache.len(), 1, "capture keeps the chain-tip answer");
        let entries = cache.to_value();
        assert_eq!(entries.as_array().expect("array").len(), 1, "and writes it to the envelope",);
    }

    /// A batch request neither reads nor writes the cache, even when its
    /// requests are individually warm.
    #[tokio::test]
    async fn test_caching_transport_batch_bypasses_the_cache() {
        let inner = FixedResponseTransport::new(success_response(r#""0x42""#));
        let calls = Arc::clone(&inner.calls);
        let cache = TransportCache::new();
        let mut transport = CachingTransport::new(inner, cache.clone());

        // Warm the exact key the batch below carries.
        transport.call(single_request("eth_blockNumber")).await.expect("seed");
        assert_eq!(cache.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let batch = RequestPacket::Batch(vec![serialized_request("eth_blockNumber")]);
        transport.call(batch).await.expect("batch is forwarded");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a batch reaches the endpoint even when the same single request is cached",
        );
        assert_eq!(cache.len(), 1, "a batch response must not enter the cache");
    }

    /// Offline replay refuses batches outright: `BackendGone` rather than a
    /// Custom error, so no retry layer can spin on it.
    #[tokio::test]
    async fn test_replay_transport_batch_miss_is_backend_gone() {
        let mut transport =
            ReplayTransport::new(PathBuf::from("/tmp/test.cache.json"), TransportCache::new());

        let batch = RequestPacket::Batch(vec![serialized_request("eth_blockNumber")]);
        let err = transport.call(batch).await.expect_err("a batch can never be served offline");
        assert!(
            matches!(err, RpcError::Transport(TransportErrorKind::BackendGone)),
            "batch misses must be BackendGone, got: {err:?}",
        );
    }
}
