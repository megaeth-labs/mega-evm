//! Integration tests for the on-chain receipt evidence the single-transaction
//! replay path builds.
//!
//! `--dump-fixture` and `--verify-receipt` both need the target's on-chain
//! receipt: the dump anchors its fidelity gate to it, the verification compares
//! against it. Both must be answered by one and the same receipt — asking the
//! endpoint twice lets a reorg, or a load-balanced endpoint serving divergent
//! views, hand the two consumers different receipts, so the fixture would be
//! anchored to one on-chain execution while the verdict is derived from another.
//!
//! These tests run against a mock JSON-RPC endpoint and count the requests it
//! receives. An offline capture cannot show this at all: identical requests are
//! served from the same keyed entry, so one fetch and two fetches are
//! indistinguishable there.
//!
//! What the count sees is what crosses the process boundary. The provider's
//! in-memory LRU also serves a repeated identical request, so these tests pin
//! that a run asks the endpoint for the receipt exactly as often as it needs it
//! — never more, and never zero when a consumer needs one. That the two
//! consumers read one and the same admitted receipt is a property of the
//! evidence type they share, not something an endpoint can observe.

use serde_json::json;

mod common;
use common::{
    mock_chain::{mined_tx_json, mock_chain_serving, tx_identity},
    MockRpcServer, Run,
};

/// A mock endpoint holding one mined transaction, its block, its parent block,
/// and its receipt.
async fn mock_chain() -> MockRpcServer {
    mock_chain_serving(mined_tx_json()).await
}

/// Replay the mock's mined transaction with the given extra flags.
fn replay(server: &MockRpcServer, extra: &[&str]) -> Run {
    common::replay_online(&server.uri(), &tx_identity().0, extra)
}

/// Dumping a fixture and verifying the receipt in the same run fetches the
/// receipt exactly once, and both consumers are answered from it: the fixture is
/// written (its fidelity gate passed against the receipt's gas and status) and
/// the verification reports a match.
#[tokio::test(flavor = "multi_thread")]
async fn test_dump_and_verify_share_one_receipt_fetch() {
    let server = mock_chain().await;
    let scratch = tempfile::tempdir().expect("failed to create a scratch directory");
    let fixture_path = scratch.path().join("fixture.json");

    let run = replay(
        &server,
        &["--verify-receipt", "--dump-fixture", &fixture_path.display().to_string()],
    );

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        server.received_method_count("eth_getTransactionReceipt").await,
        1,
        "both consumers must share one fetched receipt:\n{}",
        run.stdout,
    );
    assert!(
        fixture_path.is_file(),
        "the fixture must be written:\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr,
    );
    assert_eq!(
        run.summary()["verification"],
        json!({ "match": true }),
        "the verification must be answered from the same receipt:\n{}",
        run.stdout,
    );
}

/// Verifying alone fetches the receipt exactly once too — the shared evidence
/// does not turn one consumer into two fetches.
#[tokio::test(flavor = "multi_thread")]
async fn test_verify_alone_fetches_the_receipt_once() {
    let server = mock_chain().await;

    let run = replay(&server, &["--verify-receipt"]);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        server.received_method_count("eth_getTransactionReceipt").await,
        1,
        "verification needs exactly one receipt:\n{}",
        run.stdout,
    );
}

/// A run that asks for neither the dump nor the verification never fetches a
/// receipt: the evidence is built only for the consumers that need it.
#[tokio::test(flavor = "multi_thread")]
async fn test_plain_replay_fetches_no_receipt() {
    let server = mock_chain().await;

    let run = replay(&server, &[]);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        server.received_method_count("eth_getTransactionReceipt").await,
        0,
        "a plain replay must not fetch the on-chain receipt:\n{}",
        run.stdout,
    );
}
