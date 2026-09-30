//! Compare a whole-block replay against the header of the block it replayed.
//!
//! `mega-evme replay --verify-header` executes every transaction of a block and
//! checks that the block the replay produced commits to the same execution
//! outputs as the header the chain sealed: the transactions root, the receipts
//! root, the logs bloom, the gas used, the blob gas used, and the (empty)
//! EIP-7685 requests. The header is the authenticated one — the replay rejects a
//! served header that does not hash to the hash it was served under — so a match
//! ties the replay to the chain's own commitment rather than to answers the
//! endpoint could have served independently of it.
//!
//! The state root and the withdrawals root are not compared: both commit to the
//! post-block state trie, which a replay over a forked RPC state never builds.
//!
//! Like receipt verification, the comparison is a pure function over a
//! [`BlockSummary`], independent of how either side was obtained. A block whose
//! body did not execute in full has no summary; that is an infrastructure
//! outcome reported as unavailable, never as a mismatch.

use alloy_consensus::{proofs::ordered_trie_root_with_encoder, BlockHeader, TxReceipt};
use alloy_eips::{eip7685::Requests, Encodable2718};
use alloy_primitives::{Bloom, Bytes, B256};
use op_alloy_consensus::OpReceiptEnvelope;
use serde::Serialize;

use super::verify::Mismatch;

/// The header commitments a replayed block produced.
///
/// Built by the execution kernel from the finished block, and only when every
/// transaction of the body executed and committed: a partial walk commits to a
/// different block than the one the header describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BlockSummary {
    /// Root of the ordered trie over the executed transactions' EIP-2718 encodings.
    pub transactions_root: B256,
    /// Root of the ordered trie over the produced receipts' EIP-2718 encodings.
    pub receipts_root: B256,
    /// Union of every produced receipt's bloom.
    pub logs_bloom: Bloom,
    /// Gas the block used, as the block executor accounted it.
    pub gas_used: u64,
    /// Blob gas the block used, as the block executor accounted it.
    pub blob_gas_used: u64,
    /// EIP-7685 requests the block executor produced.
    pub requests: Requests,
}

impl BlockSummary {
    /// Summarize a finished block from its executed transactions and the block
    /// executor's result.
    pub(super) fn new(
        raw_transactions: &[Bytes],
        receipts: &[OpReceiptEnvelope],
        gas_used: u64,
        blob_gas_used: u64,
        requests: Requests,
    ) -> Self {
        Self {
            transactions_root: ordered_trie_root_with_encoder(raw_transactions, |raw, buf| {
                buf.extend_from_slice(raw)
            }),
            receipts_root: receipts_root(receipts),
            logs_bloom: logs_bloom(receipts),
            gas_used,
            blob_gas_used,
            requests,
        }
    }
}

/// Root of the ordered trie over the receipts' EIP-2718 encodings — the value a
/// header's `receiptsRoot` commits to.
pub(super) fn receipts_root(receipts: &[OpReceiptEnvelope]) -> B256 {
    ordered_trie_root_with_encoder(receipts, |receipt, buf| receipt.encode_2718(buf))
}

/// Union of the receipts' blooms — the value a header's `logsBloom` commits to.
fn logs_bloom(receipts: &[OpReceiptEnvelope]) -> Bloom {
    receipts.iter().fold(Bloom::ZERO, |mut bloom, receipt| {
        bloom |= receipt.bloom();
        bloom
    })
}

/// The verdict for one verified block header.
///
/// Three shapes on the wire, mirroring the per-transaction receipt verdict:
/// - compared and equal: `{"match": true}`
/// - compared and diverged: `{"match": false, "diff": …}`
/// - comparison could not run: `{"error": "…"}` — the block body did not execute in full, so there
///   is no summary to compare.
///
/// Serialize is hand-written so an unavailable verdict never emits a false
/// `match` that a consumer would read as a divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HeaderVerification {
    /// Whether the replay reproduced every compared header commitment.
    ///
    /// Meaningless when [`Self::error`] is set; the wire shape omits `match`.
    pub matched: bool,
    /// The mismatched commitments; absent on a match or when the comparison
    /// never ran.
    pub diff: Option<HeaderDiff>,
    /// Why the comparison could not run.
    pub error: Option<String>,
}

impl HeaderVerification {
    /// A completed comparison.
    const fn compared(diff: Option<HeaderDiff>) -> Self {
        Self { matched: diff.is_none(), diff, error: None }
    }

    /// The comparison could not run.
    pub(super) fn unavailable(message: impl Into<String>) -> Self {
        Self { matched: false, diff: None, error: Some(message.into()) }
    }

    /// Whether this verdict is an unavailable comparison.
    pub(super) const fn is_unavailable(&self) -> bool {
        self.error.is_some()
    }

    /// The one-line human verdict printed for a verified block.
    pub(super) fn verdict_line(&self) -> String {
        if let Some(error) = &self.error {
            format!("header verification: FAILED ({error})")
        } else if let Some(diff) = &self.diff {
            format!("header verification: MISMATCH ({})", diff.describe())
        } else {
            "header verification: MATCH".to_string()
        }
    }
}

impl Serialize for HeaderVerification {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        if let Some(error) = &self.error {
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry("error", error)?;
            return map.end();
        }
        let fields = 1 + usize::from(self.diff.is_some());
        let mut map = serializer.serialize_map(Some(fields))?;
        map.serialize_entry("match", &self.matched)?;
        if let Some(diff) = &self.diff {
            map.serialize_entry("diff", diff)?;
        }
        map.end()
    }
}

/// The header commitments the replay did not reproduce. Commitments that agree
/// are absent. `onchain` is the header's value, `replay` the replayed block's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(super) struct HeaderDiff {
    /// Present when the transactions roots differ: the executed body is not the
    /// body the header commits to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transactions_root: Option<Mismatch<B256>>,
    /// Present when the receipts roots differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipts_root: Option<Mismatch<B256>>,
    /// Present when the logs blooms differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs_bloom: Option<Mismatch<Bloom>>,
    /// Present when the block gas used differs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_used: Option<Mismatch<u64>>,
    /// Present when the blob gas used differs (`null` for a header without the
    /// field).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob_gas_used: Option<Mismatch<Option<u64>>>,
    /// Present when the EIP-7685 requests differ from what the header commits
    /// to (`null` for a header without a requests hash).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_hash: Option<Mismatch<Option<B256>>>,
}

impl HeaderDiff {
    /// Whether every compared commitment agreed.
    const fn is_empty(&self) -> bool {
        self.transactions_root.is_none() &&
            self.receipts_root.is_none() &&
            self.logs_bloom.is_none() &&
            self.gas_used.is_none() &&
            self.blob_gas_used.is_none() &&
            self.requests_hash.is_none()
    }

    /// Render every mismatched commitment as one comma-separated line.
    ///
    /// A bloom is 256 bytes, so the line only names it; the JSON diff carries
    /// both values.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(m) = &self.transactions_root {
            parts.push(format!("transactions_root: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if let Some(m) = &self.receipts_root {
            parts.push(format!("receipts_root: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if self.logs_bloom.is_some() {
            parts.push("logs_bloom differs".to_string());
        }
        if let Some(m) = &self.gas_used {
            parts.push(format!("gas_used: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if let Some(m) = &self.blob_gas_used {
            parts.push(format!(
                "blob_gas_used: onchain {} vs replay {}",
                display_optional(m.onchain.as_ref()),
                display_optional(m.replay.as_ref())
            ));
        }
        if let Some(m) = &self.requests_hash {
            parts.push(format!(
                "requests_hash: onchain {} vs replay {}",
                display_optional(m.onchain.as_ref()),
                display_optional(m.replay.as_ref())
            ));
        }
        parts.join(", ")
    }
}

/// Render an optional header field for the human verdict line.
fn display_optional<T: core::fmt::Display>(value: Option<&T>) -> String {
    value.map_or_else(|| "none".to_string(), ToString::to_string)
}

/// Compare the replayed block's summary against the header of the block.
///
/// The requests commitment follows EIP-7685: a header that carries a
/// `requestsHash` must carry the hash of the produced requests, and a header
/// without one must come from a block that produced none.
pub(super) fn compare<H: BlockHeader>(header: &H, summary: &BlockSummary) -> HeaderVerification {
    let mut diff = HeaderDiff::default();

    if header.transactions_root() != summary.transactions_root {
        diff.transactions_root = Some(Mismatch {
            onchain: header.transactions_root(),
            replay: summary.transactions_root,
        });
    }
    if header.receipts_root() != summary.receipts_root {
        diff.receipts_root =
            Some(Mismatch { onchain: header.receipts_root(), replay: summary.receipts_root });
    }
    if header.logs_bloom() != summary.logs_bloom {
        diff.logs_bloom =
            Some(Mismatch { onchain: header.logs_bloom(), replay: summary.logs_bloom });
    }
    if header.gas_used() != summary.gas_used {
        diff.gas_used = Some(Mismatch { onchain: header.gas_used(), replay: summary.gas_used });
    }
    if header.blob_gas_used() != Some(summary.blob_gas_used) {
        diff.blob_gas_used =
            Some(Mismatch { onchain: header.blob_gas_used(), replay: Some(summary.blob_gas_used) });
    }
    let replay_requests_hash = summary.requests.requests_hash();
    let requests_agree = match header.requests_hash() {
        Some(onchain) => onchain == replay_requests_hash,
        None => summary.requests.iter().all(|request| request.len() <= 1),
    };
    if !requests_agree {
        diff.requests_hash =
            Some(Mismatch { onchain: header.requests_hash(), replay: Some(replay_requests_hash) });
    }

    HeaderVerification::compared((!diff.is_empty()).then_some(diff))
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Eip658Value, Header, Receipt, ReceiptWithBloom, EMPTY_ROOT_HASH};
    use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
    use alloy_primitives::{address, b256, bytes, Log, LogData};
    use op_alloy_consensus::{OpDepositReceipt, OpDepositReceiptWithBloom};

    use super::*;

    /// A successful receipt of `kind` with the given cumulative gas and logs.
    fn receipt(cumulative_gas_used: u64, logs: Vec<Log>) -> OpReceiptEnvelope {
        let receipt = Receipt { status: Eip658Value::Eip658(true), cumulative_gas_used, logs };
        OpReceiptEnvelope::Eip1559(ReceiptWithBloom::from(receipt))
    }

    /// A deposit receipt, the kind every block starts with.
    fn deposit_receipt(cumulative_gas_used: u64) -> OpReceiptEnvelope {
        let inner =
            Receipt { status: Eip658Value::Eip658(true), cumulative_gas_used, logs: vec![] };
        OpReceiptEnvelope::Deposit(OpDepositReceiptWithBloom::from(OpDepositReceipt {
            inner,
            deposit_nonce: Some(7),
            deposit_receipt_version: Some(1),
        }))
    }

    fn log() -> Log {
        Log {
            address: address!("0x00000000000000000000000000000000000000aa"),
            data: LogData::new(
                vec![b256!("0x000000000000000000000000000000000000000000000000000000000000000a")],
                bytes!("deadbeef"),
            )
            .expect("topic count within bounds"),
        }
    }

    /// Two transactions, a deposit and a call emitting one log.
    fn summary() -> BlockSummary {
        let raw = [bytes!("7e01"), bytes!("02c0")];
        let receipts = [deposit_receipt(40_000), receipt(61_000, vec![log()])];
        BlockSummary::new(&raw, &receipts, 61_000, 0, Requests::default())
    }

    /// A header that commits to exactly what `summary` holds, with an
    /// Isthmus-shaped empty requests hash.
    fn header_for(summary: &BlockSummary) -> Header {
        Header {
            transactions_root: summary.transactions_root,
            receipts_root: summary.receipts_root,
            logs_bloom: summary.logs_bloom,
            gas_used: summary.gas_used,
            blob_gas_used: Some(summary.blob_gas_used),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            ..Default::default()
        }
    }

    fn json(verification: &HeaderVerification) -> serde_json::Value {
        serde_json::to_value(verification).expect("verdict is serializable")
    }

    #[test]
    fn test_summary_of_an_empty_block_commits_to_the_empty_roots() {
        let summary = BlockSummary::new(&[], &[], 0, 0, Requests::default());

        assert_eq!(summary.transactions_root, EMPTY_ROOT_HASH);
        assert_eq!(summary.receipts_root, EMPTY_ROOT_HASH);
        assert_eq!(summary.logs_bloom, Bloom::ZERO);
        assert_eq!(summary.requests.requests_hash(), EMPTY_REQUESTS_HASH);
    }

    /// The summary uses the consensus encodings: the receipts root agrees with
    /// alloy's own receipt-root helper and the bloom with the receipts' blooms.
    #[test]
    fn test_summary_uses_the_consensus_encodings() {
        let receipts = [deposit_receipt(40_000), receipt(61_000, vec![log()])];
        let summary = BlockSummary::new(&[], &receipts, 61_000, 0, Requests::default());

        assert_eq!(
            summary.receipts_root,
            alloy_consensus::proofs::calculate_receipt_root(&receipts)
        );
        assert_eq!(summary.logs_bloom, receipts[1].bloom());
        assert_ne!(summary.logs_bloom, Bloom::ZERO);
    }

    #[test]
    fn test_compare_matching_header_matches() {
        let summary = summary();

        let verification = compare(&header_for(&summary), &summary);

        assert!(verification.matched, "{verification:?}");
        assert_eq!(json(&verification), serde_json::json!({ "match": true }));
        assert_eq!(verification.verdict_line(), "header verification: MATCH");
    }

    /// A header without a requests hash (pre-Prague shape) matches a block that
    /// produced no requests.
    #[test]
    fn test_compare_header_without_requests_hash_matches_empty_requests() {
        let summary = summary();
        let header = Header { requests_hash: None, ..header_for(&summary) };

        assert!(compare(&header, &summary).matched);
    }

    /// A gas change shows up in every commitment it touches: the block gas and
    /// the receipts root both diverge, while the body and the bloom still agree.
    #[test]
    fn test_compare_reports_gas_and_receipts_root_deltas() {
        let summary = summary();
        let header = header_for(&summary);
        let diverged = BlockSummary::new(
            &[bytes!("7e01"), bytes!("02c0")],
            &[deposit_receipt(40_000), receipt(62_000, vec![log()])],
            62_000,
            0,
            Requests::default(),
        );

        let verification = compare(&header, &diverged);

        assert!(!verification.matched);
        let diff = verification.diff.as_ref().expect("a mismatch carries a diff");
        assert_eq!(diff.gas_used, Some(Mismatch { onchain: 61_000, replay: 62_000 }));
        assert_eq!(
            diff.receipts_root,
            Some(Mismatch { onchain: summary.receipts_root, replay: diverged.receipts_root })
        );
        assert!(diff.transactions_root.is_none() && diff.logs_bloom.is_none(), "{diff:?}");
        assert_eq!(
            json(&verification)["diff"]["gas_used"],
            serde_json::json!({ "onchain": 61_000, "replay": 62_000 })
        );
        assert_eq!(
            verification.verdict_line(),
            format!(
                "header verification: MISMATCH (receipts_root: onchain {} vs replay {}, \
                 gas_used: onchain 61000 vs replay 62000)",
                summary.receipts_root, diverged.receipts_root
            )
        );
    }

    #[test]
    fn test_compare_reports_a_different_body() {
        let summary = summary();
        let header = header_for(&summary);
        let other_body = BlockSummary { transactions_root: B256::repeat_byte(1), ..summary };

        let diff = compare(&header, &other_body).diff.expect("bodies differ");

        assert_eq!(diff.transactions_root.map(|m| m.replay), Some(B256::repeat_byte(1)));
    }

    #[test]
    fn test_compare_reports_logs_bloom_blob_gas_and_requests_deltas() {
        let summary = summary();
        let header = Header {
            logs_bloom: Bloom::ZERO,
            blob_gas_used: None,
            requests_hash: Some(B256::repeat_byte(2)),
            ..header_for(&summary)
        };

        let verification = compare(&header, &summary);

        let diff = verification.diff.as_ref().expect("a mismatch carries a diff");
        assert_eq!(
            diff.logs_bloom,
            Some(Mismatch { onchain: Bloom::ZERO, replay: summary.logs_bloom })
        );
        assert_eq!(diff.blob_gas_used, Some(Mismatch { onchain: None, replay: Some(0) }));
        assert_eq!(
            diff.requests_hash,
            Some(Mismatch {
                onchain: Some(B256::repeat_byte(2)),
                replay: Some(EMPTY_REQUESTS_HASH)
            })
        );
        assert_eq!(
            verification.verdict_line(),
            format!(
                "header verification: MISMATCH (logs_bloom differs, blob_gas_used: onchain none \
                 vs replay 0, requests_hash: onchain {} vs replay {EMPTY_REQUESTS_HASH})",
                B256::repeat_byte(2)
            )
        );
    }

    /// A block that produced requests cannot match a header that commits to none.
    #[test]
    fn test_compare_rejects_requests_against_a_header_without_requests_hash() {
        let mut summary = summary();
        summary.requests = Requests::new(vec![bytes!("00aa")]);
        let header = Header { requests_hash: None, ..header_for(&summary) };

        let diff = compare(&header, &summary).diff.expect("requests were produced");

        assert_eq!(diff.requests_hash.map(|m| m.onchain), Some(None));
    }

    #[test]
    fn test_unavailable_verification_serializes_as_error_only() {
        let verification = HeaderVerification::unavailable("block body did not execute in full");

        assert!(verification.is_unavailable());
        assert_eq!(
            json(&verification),
            serde_json::json!({ "error": "block body did not execute in full" })
        );
        assert_eq!(
            verification.verdict_line(),
            "header verification: FAILED (block body did not execute in full)"
        );
    }
}
