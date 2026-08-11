//! Transport-level RPC cache and the two transports that wrap it.
//!
//! Unlike alloy's provider-level `CacheLayer` (which only caches a subset of
//! methods it explicitly overrides), [`TransportCache`] captures single JSON-RPC
//! request/response pairs that pass through the transport — making it the
//! substrate for complete offline replay fixtures backing `--rpc.capture-file` /
//! `--rpc.replay-file`.

use std::{
    collections::HashMap,
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
    entries: Arc<RwLock<HashMap<B256, String>>>,
}

impl TransportCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn get(&self, key: &B256) -> Option<String> {
        self.entries.read().expect("cache lock poisoned").get(key).cloned()
    }

    fn put(&self, key: B256, value: String) {
        self.entries.write().expect("cache lock poisoned").insert(key, value);
    }

    pub(super) fn len(&self) -> usize {
        self.entries.read().expect("cache lock poisoned").len()
    }

    /// Merge entries from a serialized cache value. Existing entries are NOT
    /// overwritten — this preserves fresh responses (e.g. `eth_chainId`)
    /// that were fetched before the merge.
    pub(super) fn merge(&self, value: &serde_json::Value) -> Result<()> {
        let entries: Vec<TransportCacheEntry> =
            serde_json::from_value(value.clone()).map_err(|e| {
                EvmeError::RpcError(format!("Failed to parse transport cache entries: {e}"))
            })?;
        let mut map = self.entries.write().expect("cache lock poisoned");
        for entry in entries {
            map.entry(entry.key).or_insert(entry.value);
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

    /// Serialize all entries to a JSON value for the envelope.
    /// Sorted by key for deterministic output (avoids noisy diffs on committed fixtures).
    pub(super) fn to_value(&self) -> serde_json::Value {
        let mut entries: Vec<TransportCacheEntry> = self
            .entries
            .read()
            .expect("cache lock poisoned")
            .iter()
            .map(|(k, v)| TransportCacheEntry { key: *k, value: v.clone() })
            .collect();
        entries.sort_by_key(|e| e.key);
        serde_json::to_value(entries).expect("TransportCacheEntry is always serializable")
    }

    /// Deserialize from the envelope's `cache` field.
    pub(super) fn from_value(value: &serde_json::Value) -> Result<Self> {
        let entries: Vec<TransportCacheEntry> =
            serde_json::from_value(value.clone()).map_err(|e| {
                EvmeError::RpcError(format!("Failed to parse transport cache entries: {e}"))
            })?;
        let cache = Self::new();
        {
            let mut map = cache.entries.write().expect("cache lock poisoned");
            for entry in entries {
                map.insert(entry.key, entry.value);
            }
        }
        Ok(cache)
    }
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

/// Transport wrapper that records all JSON-RPC responses into a
/// [`TransportCache`]. Used in capture mode: cache hits are served locally,
/// misses are forwarded to the inner transport and the response is cached.
#[derive(Debug, Clone)]
pub(super) struct CachingTransport<T> {
    inner: T,
    cache: TransportCache,
}

impl<T> CachingTransport<T> {
    pub(super) fn new(inner: T, cache: TransportCache) -> Self {
        Self { inner, cache }
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

            // Cache miss: forward to inner, cache non-null successful responses
            // only.
            //
            // - JSON-RPC error bodies (e.g. transient rate-limit errors that the endpoint surfaces
            //   via `error` instead of an HTTP status) must not be baked into the fixture —
            //   otherwise replay would replay the error forever.
            // - `"result": null` is likewise skipped: a transient null (e.g.
            //   eth_getTransactionByHash for a briefly-invisible tx) would otherwise freeze "not
            //   found" into the fixture with no in-tool recovery (`--rpc.clear-cache` conflicts
            //   with capture mode). Offline, a cache miss names the exact request instead.
            let cache = self.cache.clone();
            let method = r.method().to_string();
            let fut = self.inner.call(req);
            return Box::pin(async move {
                let response = fut.await?;
                if let ResponsePacket::Single(ref resp) = response {
                    if resp.is_error() {
                        tracing::warn!(
                            method = %method,
                            "Skipping cache for JSON-RPC error response",
                        );
                    } else if is_null_success_result(resp) {
                        tracing::warn!(
                            method = %method,
                            "Skipping cache for JSON-RPC null result",
                        );
                    } else if let Ok(serialized) = serde_json::to_string(resp) {
                        cache.put(key, serialized);
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

    fn single_request(method: &'static str) -> RequestPacket {
        RequestPacket::Single(
            alloy_json_rpc::Request::new(method, Id::Number(1), ())
                .serialize()
                .expect("serialize request"),
        )
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
}
