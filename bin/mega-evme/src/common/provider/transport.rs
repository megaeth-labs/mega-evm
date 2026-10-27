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

/// HTTP with the same RPC/status handling as alloy's transport, but without URL-bearing
/// tracing spans or reqwest errors. Endpoint paths and queries commonly hold credentials.
#[derive(Clone)]
pub(super) struct RpcHttp {
    client: alloy_transport_http::reqwest::Client,
    url: alloy_transport_http::reqwest::Url,
}

impl std::fmt::Debug for RpcHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcHttp")
            .field("origin", &super::display_rpc_url(self.url.as_str()))
            .finish()
    }
}

impl RpcHttp {
    pub(super) fn new(url: alloy_transport_http::reqwest::Url) -> Self {
        Self { client: alloy_transport_http::reqwest::Client::new(), url }
    }
}

impl tower::Service<RequestPacket> for RpcHttp {
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
        let this = self.clone();
        Box::pin(async move {
            let response = this
                .client
                .post(this.url)
                .json(&req)
                .headers(req.headers())
                .send()
                .await
                .map_err(|e| TransportErrorKind::custom(e.without_url()))?;
            let status = response.status();
            let body =
                response.bytes().await.map_err(|e| TransportErrorKind::custom(e.without_url()))?;
            let parsed = serde_json::from_slice::<ResponsePacket>(&body);
            if !status.is_success() && !parsed.as_ref().is_ok_and(ResponsePacket::is_error) {
                return Err(TransportErrorKind::http_error(
                    status.as_u16(),
                    "<redacted HTTP response body>".to_string(),
                ));
            }
            let mut parsed = parsed.map_err(|error| {
                TransportError::deser_err(
                    // A serde error can quote an offending JSON string, including its URL.
                    serde_json::Error::io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "invalid RPC response at line {}, column {}",
                            error.line(),
                            error.column()
                        ),
                    )),
                    "<redacted HTTP response body>",
                )
            })?;
            // Preserve successful results verbatim; only errors are displayed as diagnostics.
            match &mut parsed {
                ResponsePacket::Single(response) => redact_error(response),
                ResponsePacket::Batch(responses) => responses.iter_mut().for_each(redact_error),
            }
            Ok(parsed)
        })
    }
}

fn redact_error(response: &mut alloy_json_rpc::Response) {
    if let alloy_json_rpc::ResponsePayload::Failure(error) = &mut response.payload {
        error.message = super::redact_rpc_urls_in_text(&error.message).into();
        if let Some(data) = error.data.take() {
            // RawValue accepts values (large numbers or deep nesting) Value cannot represent.
            // Omit data we cannot inspect, rather than panic or echo it without redaction.
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data.get()) {
                redact_error_data(&mut value);
                error.data = serde_json::value::to_raw_value(&value).ok();
            }
        }
    }
}

fn redact_error_data(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = super::redact_rpc_urls_in_text(text),
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact_error_data),
        serde_json::Value::Object(values) => {
            *values = std::mem::take(values)
                .into_iter()
                .map(|(key, mut value)| {
                    redact_error_data(&mut value);
                    (super::redact_rpc_urls_in_text(&key), value)
                })
                .collect();
        }
        _ => {}
    }
}

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

            // Cache miss: forward to inner, cache successful responses only.
            // JSON-RPC error bodies (e.g. transient rate-limit errors that the
            // endpoint surfaces via `error` instead of an HTTP status) must not
            // be baked into the fixture — otherwise replay would replay the
            // error forever.
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
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_rpc_error_data_that_cannot_be_inspected_is_omitted() {
        for data in ["1e400".to_string(), format!("{}0{}", "[".repeat(200), "]".repeat(200))] {
            let mut response = alloy_json_rpc::Response {
                id: alloy_json_rpc::Id::Number(0),
                payload: alloy_json_rpc::ResponsePayload::Failure(alloy_json_rpc::ErrorPayload {
                    code: -32603,
                    message: "upstream error".into(),
                    data: Some(serde_json::value::RawValue::from_string(data).unwrap()),
                }),
            };
            redact_error(&mut response);
            let alloy_json_rpc::ResponsePayload::Failure(error) = response.payload else {
                panic!("error")
            };
            assert_eq!(error.code, -32603);
            assert_eq!(error.message, "upstream error");
            assert!(error.data.is_none());
        }
    }

    /// The replay transport is always ready.
    /// Single-request cache misses return a descriptive Custom error (safe because
    /// the retry layer is never installed on the replay path).
    /// Batch misses return `BackendGone`.
    #[tokio::test]
    async fn test_replay_transport_cache_miss() {
        use tower::Service;

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
}
