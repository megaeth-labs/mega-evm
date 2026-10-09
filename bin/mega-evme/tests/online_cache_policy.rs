//! What the online `--rpc.cache-dir` cache carries from one run to the next.
//!
//! The cache lives at the transport, so it sees every RPC method rather than
//! the handful a provider-level cache overrides. That makes two questions
//! observable from outside the process, and this file answers both by counting
//! the requests a mock endpoint receives across two runs that share one cache
//! directory:
//!
//! - **What must not survive a run.** An answer that is only true for "now" — a still-pending
//!   transaction's metadata, the chain tip — would otherwise be frozen into the file and served to
//!   every later run, with no in-tool recovery short of `--rpc.clear-cache`.
//! - **What must survive it.** Everything anchored to a fixed block: the block body, its parent,
//!   the target's mined metadata, its receipt, and the state reads. A warm run asks the endpoint
//!   for none of them.
//!
//! Counting is the only way to see this. In-process assertions cannot
//! distinguish "served from the file" from "fetched again", and an offline
//! capture cannot show it at all: identical requests are served from the same
//! keyed entry, so one fetch and two fetches look the same there.

use std::path::Path;

mod common;
use common::{
    mock_chain::{mined_tx_json, mock_chain_serving, pending_tx_json, tx_identity, CHAIN_ID},
    MockRpcServer, Run,
};

/// The mock with the target reported as mined.
async fn mined_chain() -> MockRpcServer {
    mock_chain_serving(mined_tx_json()).await
}

/// The mock with the target reported as pending: no block number, no inclusion hash.
async fn pending_chain() -> MockRpcServer {
    mock_chain_serving(pending_tx_json()).await
}

/// Replay the mock's target through the on-disk cache at `cache_dir`.
fn replay(server: &MockRpcServer, cache_dir: &Path, extra: &[&str]) -> Run {
    let (tx_hash, _) = tx_identity();
    common::mega_evme()
        .args(["replay", &tx_hash, "--rpc", &server.uri()])
        .args(["--rpc.cache-dir", cache_dir.to_str().expect("utf-8 cache dir")])
        .args(["--rpc.max-retries", "0", "--rpc.backoff-ms", "1", "--json"])
        .args(extra)
        .output()
        .expect("failed to run mega-evme")
        .into()
}

/// Per-method request counts, for the methods these tests reason about.
struct Counts {
    chain_id: usize,
    tx_by_hash: usize,
    block_number: usize,
    block_by_number: usize,
    receipt: usize,
    total: usize,
}

impl Counts {
    async fn take(server: &MockRpcServer) -> Self {
        Self {
            chain_id: server.received_method_count("eth_chainId").await,
            tx_by_hash: server.received_method_count("eth_getTransactionByHash").await,
            block_number: server.received_method_count("eth_blockNumber").await,
            block_by_number: server.received_method_count("eth_getBlockByNumber").await,
            receipt: server.received_method_count("eth_getTransactionReceipt").await,
            total: server.received_request_count().await,
        }
    }

    /// What the second run added on top of the first.
    fn since(&self, before: &Self) -> Self {
        Self {
            chain_id: self.chain_id - before.chain_id,
            tx_by_hash: self.tx_by_hash - before.tx_by_hash,
            block_number: self.block_number - before.block_number,
            block_by_number: self.block_by_number - before.block_by_number,
            receipt: self.receipt - before.receipt,
            total: self.total - before.total,
        }
    }
}

/// Request sequence of a single online replay, warm and cold.
///
/// This is the baseline the transport-level cache establishes. The counts that
/// changed when online caching moved down from the provider layer are the block
/// fetches: a provider-level cache overrode neither `eth_getBlockByNumber` nor
/// `eth_chainId`, so a warm run re-fetched the block and its parent every time
/// (old warm `eth_getBlockByNumber` = 2, new = 0). Everything else was already
/// covered by the overridden methods and is unchanged (`eth_getTransactionByHash`,
/// `eth_getTransactionReceipt` and the state reads: warm = 0 before and after).
///
/// The chain-id probe is the one request that must stay live in both worlds: it
/// runs on a bare provider so the cache can never authenticate itself.
#[tokio::test(flavor = "multi_thread")]
async fn test_online_replay_request_sequence() {
    let server = mined_chain().await;
    let dir = tempfile::tempdir().expect("tempdir");

    let cold_run = replay(&server, dir.path(), &["--verify-receipt"]);
    assert_eq!(
        cold_run.code,
        Some(0),
        "the cold run must replay.\nstdout:\n{}\nstderr:\n{}",
        cold_run.stdout,
        cold_run.stderr,
    );
    let cold = Counts::take(&server).await;
    assert_eq!(cold.chain_id, 1, "one live chain-id probe");
    assert_eq!(cold.tx_by_hash, 1, "the target is fetched once");
    assert_eq!(cold.block_by_number, 2, "the block and its parent");
    assert_eq!(cold.receipt, 1, "one receipt for --verify-receipt");
    assert_eq!(cold.block_number, 0, "a mined target never asks for the chain tip");

    let cache_file = dir.path().join(format!("rpc-cache-{CHAIN_ID}.json"));
    assert!(cache_file.is_file(), "the cold run must persist the cache");

    let warm_run = replay(&server, dir.path(), &["--verify-receipt"]);
    assert_eq!(
        warm_run.code,
        Some(0),
        "the warm run must replay too.\nstdout:\n{}\nstderr:\n{}",
        warm_run.stdout,
        warm_run.stderr,
    );
    let warm = Counts::take(&server).await.since(&cold);

    assert_eq!(warm.chain_id, 1, "the chain-id probe is never served from the cache");
    assert_eq!(warm.tx_by_hash, 0, "a mined target's metadata never changes again");
    assert_eq!(warm.block_by_number, 0, "the block and its parent come from the cache");
    assert_eq!(warm.receipt, 0, "the receipt comes from the cache");
    assert_eq!(
        warm.total, 1,
        "the chain-id probe is the only request a warm replay makes; \
         got {} requests.\nstdout:\n{}\nstderr:\n{}",
        warm.total, warm_run.stdout, warm_run.stderr,
    );
}

/// A run that saw the target while it was still pending must not leave that
/// answer behind for the next one.
///
/// The first run fails — `--verify-receipt` has no on-chain receipt to compare
/// against — and a failing run still persists its cache, which is exactly when
/// the trap would be set: pending metadata is a perfectly ordinary non-null
/// result, so a cache that keeps it reports the transaction as pending forever,
/// including long after it lands. The second run must therefore ask the endpoint
/// again. The same holds for the chain tip the pending path reads to pick its
/// block: `eth_blockNumber` is the answer to "where is the chain now", and
/// freezing it pins every later run to the height of the first.
///
/// The block fetched at that height is the control: it *is* anchored to a fixed
/// block, so it is cached and the warm run does not ask for it.
#[tokio::test(flavor = "multi_thread")]
async fn test_online_cache_keeps_no_pending_or_chain_tip_answer() {
    let server = pending_chain().await;
    let dir = tempfile::tempdir().expect("tempdir");

    let first = replay(&server, dir.path(), &["--verify-receipt"]);
    assert_eq!(
        first.code,
        Some(1),
        "--verify-receipt cannot verify a pending target.\nstdout:\n{}\nstderr:\n{}",
        first.stdout,
        first.stderr,
    );
    let cold = Counts::take(&server).await;
    assert_eq!(cold.tx_by_hash, 1, "the target was looked up");
    assert_eq!(cold.block_number, 1, "the pending path asked for the chain tip");
    assert_eq!(cold.block_by_number, 1, "and fetched the block at that height");

    let cache_file = dir.path().join(format!("rpc-cache-{CHAIN_ID}.json"));
    assert!(cache_file.is_file(), "a failing run still persists its cache");
    let persisted = std::fs::read_to_string(&cache_file).expect("read cache");
    assert!(
        !persisted.contains("\"blockNumber\":null"),
        "no pending metadata may reach the file:\n{persisted}",
    );

    let second = replay(&server, dir.path(), &["--verify-receipt"]);
    assert_eq!(second.code, Some(1), "stdout:\n{}\nstderr:\n{}", second.stdout, second.stderr);
    let warm = Counts::take(&server).await.since(&cold);

    assert_eq!(
        warm.tx_by_hash, 1,
        "the pending target must be looked up again, not read off disk.\nstdout:\n{}\nstderr:\n{}",
        second.stdout, second.stderr,
    );
    assert_eq!(
        warm.block_number, 1,
        "the chain tip must be asked again, not read off disk.\nstdout:\n{}\nstderr:\n{}",
        second.stdout, second.stderr,
    );
    assert_eq!(
        warm.block_by_number, 0,
        "the block at a fixed height is cached — the control that shows the two \
         counts above are a policy decision, not a cache that never worked",
    );
}
