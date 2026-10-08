//! Compare a whole-block replay against the header of the block it replayed.
//!
//! `mega-evme replay --block <N> --verify-block` executes every transaction of
//! the block and checks that the block the replay produced commits to the same
//! execution outputs as the block's header: the receipts root, the logs bloom,
//! the gas used, the blob gas used, and the EIP-7685 requests.
//!
//! The header's transactions root is not one of them. It depends only on what
//! the endpoint served — every transaction is authenticated against its hash,
//! and their order is the body listing — never on execution, so a body the
//! header does not commit to is the endpoint contradicting itself rather than a
//! replay that diverged. The batch driver checks it first, through
//! [`super::coherence::require_committed_body`] over [`transactions_root`], and
//! compares the execution outputs only for a body the header commits to.
//!
//! The header compared against is the one the replay already authenticated: its
//! hash is recomputed from its own fields before anything reads it, and it links
//! to the parent block the state was forked from. A match therefore shows that
//! the replay reproduces the execution outputs committed to by a header that is
//! self-consistent and linked to its parent. Whether that header is the
//! canonical one is not something the replay can establish on its own: the
//! caller does, by pinning the block hash the run reports.
//!
//! The state root and the withdrawals root are not compared. `MegaETH` commits
//! to its state in a SALT trie rather than a Merkle-Patricia trie, and a replay
//! over forked RPC state holds no proofs to rebuild either root from.
//!
//! Like receipt verification, the comparison is a pure function over a
//! [`BlockSummary`], independent of how either side was obtained. A block whose
//! body did not execute in full has no summary; that is reported as an
//! unavailable verdict, never as a mismatch.

use alloy_consensus::{proofs::ordered_trie_root_with_encoder, BlockHeader, TxReceipt};
use alloy_eips::{eip7685::Requests, Encodable2718};
use alloy_primitives::{Bloom, Bytes, B256};
use op_alloy_consensus::OpReceiptEnvelope;
use serde::Serialize;

use super::{
    kernel::WholeBlock,
    verify::{display_optional, mismatch, DiffReport, Mismatch, Verdict},
};

/// The execution commitments a replayed block produced, in the form its header
/// carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BlockSummary {
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
    /// Summarize a block the replay executed in full.
    pub(super) fn of(block: &WholeBlock) -> Self {
        Self::new(&block.receipts, block.gas_used, block.blob_gas_used, block.requests.clone())
    }

    /// Summarize a block whose body lists no transaction.
    ///
    /// Nothing has to execute to know it: the block executor's receipts are one
    /// per transaction, its gas used is the last receipt's cumulative gas, and it
    /// produces no EIP-7685 requests and no blob gas, so a body without
    /// transactions commits to the empty receipts root, an empty bloom and zero
    /// gas whatever the pre-block system calls did.
    pub(super) fn of_empty_body() -> Self {
        Self::new(&[], 0, 0, Requests::default())
    }

    /// Summarize a block from what the block executor produced for it.
    fn new(
        receipts: &[OpReceiptEnvelope],
        gas_used: u64,
        blob_gas_used: u64,
        requests: Requests,
    ) -> Self {
        Self {
            receipts_root: receipts_root(receipts),
            logs_bloom: logs_bloom(receipts),
            gas_used,
            blob_gas_used,
            requests,
        }
    }
}

/// Root of the ordered trie over the transactions' EIP-2718 encodings, in body
/// order — the value a header's `transactionsRoot` commits to.
pub(super) fn transactions_root(raw_transactions: &[Bytes]) -> B256 {
    ordered_trie_root_with_encoder(raw_transactions, |raw, buf| buf.extend_from_slice(raw))
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

/// The verdict for one verified block, in the same three wire shapes as a
/// receipt verdict ([`Verdict`]). The comparison could not run when the block
/// body did not execute in full, or when the endpoint served a body the header
/// does not commit to.
pub(super) type BlockVerification = Verdict<BlockDiff>;

/// The commitments the replay did not reproduce. Commitments that agree are
/// absent. `onchain` is the header's value, `replay` the replayed block's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(super) struct BlockDiff {
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

impl BlockDiff {
    /// Whether every compared commitment agreed.
    const fn is_empty(&self) -> bool {
        self.receipts_root.is_none() &&
            self.logs_bloom.is_none() &&
            self.gas_used.is_none() &&
            self.blob_gas_used.is_none() &&
            self.requests_hash.is_none()
    }
}

impl DiffReport for BlockDiff {
    const LABEL: &'static str = "block verification";

    /// A bloom is 256 bytes, so the line only names it; the JSON diff carries
    /// both values.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
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

/// Compare the replayed block's execution outputs against the header of the
/// block.
///
/// Only meaningful for a body the header commits to; the caller checks the
/// transactions root first.
///
/// The requests commitment follows EIP-7685: a header that carries a
/// `requestsHash` must carry the hash of the produced requests, and a header
/// without one must come from a block that produced none. A request whose
/// encoding is only its type byte carries no data and is left out of the hash
/// by the same rule, so it does not count as produced.
pub(super) fn compare<H: BlockHeader>(header: &H, summary: &BlockSummary) -> BlockVerification {
    let replay_requests_hash = summary.requests.requests_hash();
    let requests_agree = match header.requests_hash() {
        Some(onchain) => onchain == replay_requests_hash,
        None => summary.requests.iter().all(|request| request.len() <= 1),
    };
    let diff = BlockDiff {
        receipts_root: mismatch(header.receipts_root(), summary.receipts_root),
        logs_bloom: mismatch(header.logs_bloom(), summary.logs_bloom),
        gas_used: mismatch(header.gas_used(), summary.gas_used),
        blob_gas_used: mismatch(header.blob_gas_used(), Some(summary.blob_gas_used)),
        requests_hash: (!requests_agree).then(|| Mismatch {
            onchain: header.requests_hash(),
            replay: Some(replay_requests_hash),
        }),
    };
    BlockVerification::compared((!diff.is_empty()).then_some(diff))
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Eip658Value, Header, Receipt, ReceiptWithBloom, EMPTY_ROOT_HASH};
    use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
    use alloy_primitives::{address, b256, bytes, Log, LogData};
    use op_alloy_consensus::{OpDepositReceipt, OpDepositReceiptWithBloom};

    use super::*;

    /// A successful EIP-1559 receipt with the given cumulative gas and logs.
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
        let receipts = [deposit_receipt(40_000), receipt(61_000, vec![log()])];
        BlockSummary::new(&receipts, 61_000, 0, Requests::default())
    }

    /// A header that commits to exactly what `summary` holds, with an
    /// Isthmus-shaped empty requests hash.
    fn header_for(summary: &BlockSummary) -> Header {
        Header {
            receipts_root: summary.receipts_root,
            logs_bloom: summary.logs_bloom,
            gas_used: summary.gas_used,
            blob_gas_used: Some(summary.blob_gas_used),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            ..Default::default()
        }
    }

    fn json(verification: &BlockVerification) -> serde_json::Value {
        serde_json::to_value(verification).expect("verdict is serializable")
    }

    /// A body without transactions commits to the empty roots, an empty bloom,
    /// zero gas and no requests.
    #[test]
    fn test_summary_of_an_empty_body_commits_to_the_empty_roots() {
        let summary = BlockSummary::of_empty_body();

        assert_eq!(summary, BlockSummary::new(&[], 0, 0, Requests::default()));
        assert_eq!(transactions_root(&[]), EMPTY_ROOT_HASH);
        assert_eq!(summary.receipts_root, EMPTY_ROOT_HASH);
        assert_eq!(summary.gas_used, 0);
        assert_eq!(summary.blob_gas_used, 0);
        assert_eq!(summary.logs_bloom, Bloom::ZERO);
        assert_eq!(summary.requests.requests_hash(), EMPTY_REQUESTS_HASH);
    }

    /// The summary uses the consensus encodings: the receipts root agrees with
    /// alloy's own receipt-root helper and the bloom with the receipts' blooms.
    #[test]
    fn test_summary_uses_the_consensus_encodings() {
        let receipts = [deposit_receipt(40_000), receipt(61_000, vec![log()])];
        let summary = BlockSummary::new(&receipts, 61_000, 0, Requests::default());

        assert_eq!(
            summary.receipts_root,
            alloy_consensus::proofs::calculate_receipt_root(&receipts)
        );
        assert_eq!(summary.logs_bloom, receipts[1].bloom());
        assert_ne!(summary.logs_bloom, Bloom::ZERO);
    }

    /// The transactions root is the ordered trie over the raw encodings, exactly
    /// as the bytes were authenticated: no re-encoding happens on the way in.
    #[test]
    fn test_transactions_root_is_over_the_raw_encodings() {
        let raw = [bytes!("7e01"), bytes!("02c0")];

        let expected = ordered_trie_root_with_encoder(&raw, |raw, buf| buf.extend_from_slice(raw));
        assert_eq!(transactions_root(&raw), expected);
        assert_ne!(transactions_root(&raw), EMPTY_ROOT_HASH);
        assert_ne!(
            transactions_root(&[bytes!("02c0"), bytes!("7e01")]),
            transactions_root(&raw),
            "order is committed"
        );
    }

    #[test]
    fn test_compare_matching_header_matches() {
        let summary = summary();

        let verification = compare(&header_for(&summary), &summary);

        assert!(verification.matched, "{verification:?}");
        assert!(!verification.is_unavailable());
        assert_eq!(json(&verification), serde_json::json!({ "match": true }));
        assert_eq!(verification.verdict_line(), "block verification: MATCH");
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
    /// the receipts root both diverge, while the bloom still agrees.
    #[test]
    fn test_compare_reports_gas_and_receipts_root_deltas() {
        let summary = summary();
        let header = header_for(&summary);
        let diverged = BlockSummary::new(
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
        assert!(diff.logs_bloom.is_none(), "{diff:?}");
        assert_eq!(
            json(&verification),
            serde_json::json!({
                "match": false,
                "diff": {
                    "receipts_root": {
                        "onchain": summary.receipts_root,
                        "replay": diverged.receipts_root,
                    },
                    "gas_used": { "onchain": 61_000, "replay": 62_000 },
                }
            })
        );
        assert_eq!(
            verification.verdict_line(),
            format!(
                "block verification: MISMATCH (receipts_root: onchain {} vs replay {}, \
                 gas_used: onchain 61000 vs replay 62000)",
                summary.receipts_root, diverged.receipts_root
            )
        );
    }

    /// The transactions root is not compared here: a header committing to
    /// another body still matches on the execution outputs alone.
    #[test]
    fn test_compare_ignores_the_transactions_root() {
        let summary = summary();
        let header = Header { transactions_root: B256::repeat_byte(1), ..header_for(&summary) };

        assert!(compare(&header, &summary).matched);
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
            json(&verification)["diff"]["blob_gas_used"],
            serde_json::json!({ "onchain": null, "replay": 0 })
        );
        assert_eq!(
            verification.verdict_line(),
            format!(
                "block verification: MISMATCH (logs_bloom differs, blob_gas_used: onchain none \
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

    /// A request that is only its type byte carries no data, so a header
    /// without a requests hash still matches a block that produced one.
    #[test]
    fn test_compare_ignores_empty_requests_against_a_header_without_requests_hash() {
        let mut summary = summary();
        summary.requests = Requests::new(vec![bytes!("00")]);
        let header = Header { requests_hash: None, ..header_for(&summary) };

        assert!(compare(&header, &summary).matched);
    }

    #[test]
    fn test_unavailable_verification_serializes_as_error_only() {
        let verification = BlockVerification::unavailable("block body did not execute in full");

        assert!(verification.is_unavailable());
        assert_eq!(
            json(&verification),
            serde_json::json!({ "error": "block body did not execute in full" })
        );
        assert_eq!(
            verification.verdict_line(),
            "block verification: FAILED (block body did not execute in full)"
        );
    }
}
