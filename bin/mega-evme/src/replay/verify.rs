//! Compare a local replay against the transaction's on-chain receipt.
//!
//! `mega-evme replay --verify-receipt` fetches the on-chain receipt of every
//! replayed target and checks that the local execution reproduces it. The
//! comparison is a pure function over [`ReceiptFacts`] — the consensus facts
//! both sides carry — so it is independent of how either receipt was obtained
//! and testable without a provider.
//!
//! Anything that prevents the comparison from running at all (a receipt the
//! endpoint cannot serve, a receipt describing a different transaction than the
//! one requested, or a receipt describing a different inclusion than the
//! replayed block) is an infrastructure failure, never a mismatch: a target that
//! could not be verified must not be reported as a divergence.

use core::fmt;

use alloy_consensus::TxReceipt;
use alloy_network::ReceiptResponse;
use alloy_primitives::{keccak256, Address, Bytes, Log, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{Log as RpcLog, TransactionReceipt};
use mega_evm::{alloy_consensus::transaction::SignerRecoverable, alloy_eips::Encodable2718};
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use serde::Serialize;

use super::{ReplayError, Result};

/// The consensus facts compared between the on-chain receipt and the receipt
/// the local replay produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReceiptFacts {
    /// Whether the transaction succeeded.
    pub status: bool,
    /// Gas the transaction used.
    pub gas_used: u64,
    /// The consensus logs the transaction emitted, in order.
    pub logs: Vec<Log>,
}

impl ReceiptFacts {
    /// Extract the compared facts from a receipt envelope.
    ///
    /// Both sides go through this one accessor set — the on-chain side is the
    /// RPC receipt's inner envelope, the local side the envelope the replay
    /// built — so neither side can be read with different semantics.
    pub(super) fn from_receipt<T>(receipt: &TransactionReceipt<T>) -> Self
    where
        T: TxReceipt<Log = RpcLog>,
    {
        Self {
            status: receipt.inner.status(),
            gas_used: receipt.gas_used,
            logs: receipt.logs().iter().map(|log| log.inner.clone()).collect(),
        }
    }
}

/// The verdict for one verified transaction.
///
/// Three shapes on the wire:
/// - compared and equal: `{"match": true}`
/// - compared and diverged: `{"match": false, "diff": …}`
/// - receipt question unanswered: `{"error": "…"}` — the target still replayed; only the comparison
///   could not run (transport, pruned, reorg).
///
/// Serialize is hand-written so an unavailable outcome never emits a false
/// `match` that a consumer would read as a divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VerificationOutcome {
    /// Whether the local replay reproduced the on-chain receipt.
    ///
    /// Meaningless when [`Self::error`] is set (kept for a simple bool check
    /// on the compared path); the wire shape omits `match` in that case.
    pub matched: bool,
    /// The mismatched dimensions; absent when the replay matched or when the
    /// comparison never ran.
    pub diff: Option<VerificationDiff>,
    /// Why the on-chain receipt could not be compared, when the target still
    /// produced a local result. Mutually exclusive with a real match/diff.
    pub error: Option<String>,
}

impl VerificationOutcome {
    /// A completed comparison against an on-chain receipt.
    pub(super) fn compared(matched: bool, diff: Option<VerificationDiff>) -> Self {
        Self { matched, diff, error: None }
    }

    /// The target replayed, but the on-chain receipt question went unanswered.
    pub(super) fn unavailable(message: impl Into<String>) -> Self {
        Self { matched: false, diff: None, error: Some(message.into()) }
    }

    /// Whether this outcome is an unanswered receipt fetch, not a comparison.
    pub(super) const fn is_unavailable(&self) -> bool {
        self.error.is_some()
    }

    /// The one-line human verdict printed for a verified transaction.
    pub(super) fn verdict_line(&self) -> String {
        if let Some(error) = &self.error {
            format!("verification: FAILED ({error})")
        } else if let Some(diff) = &self.diff {
            format!("verification: MISMATCH ({})", diff.describe())
        } else {
            "verification: MATCH".to_string()
        }
    }
}

impl Serialize for VerificationOutcome {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
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

/// The mismatched dimensions of a verification. Dimensions that agree are
/// absent, so a diff never has to be scanned for "everything equal" entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(super) struct VerificationDiff {
    /// Present when the success flags differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Mismatch<bool>>,
    /// Present when the gas used differs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_used: Option<Mismatch<u64>>,
    /// Present when the emitted logs differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<LogsDiff>,
}

impl VerificationDiff {
    /// Whether every compared dimension agreed.
    fn is_empty(&self) -> bool {
        self.status.is_none() && self.gas_used.is_none() && self.logs.is_none()
    }

    /// Render every mismatched dimension as one comma-separated line.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(m) = &self.status {
            parts.push(format!("status: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if let Some(m) = &self.gas_used {
            parts.push(format!("gas_used: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if let Some(logs) = &self.logs {
            if let Some(m) = &logs.count {
                parts.push(format!("logs_count: onchain {} vs replay {}", m.onchain, m.replay));
            }
            if let Some(m) = &logs.first_mismatch {
                parts.push(format!(
                    "logs[{}].{}: onchain {} vs replay {}",
                    m.index,
                    m.field.as_str(),
                    m.onchain,
                    m.replay,
                ));
            }
        }
        parts.join(", ")
    }
}

/// One dimension's two values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Mismatch<T> {
    /// The value the on-chain receipt reports.
    pub onchain: T,
    /// The value the local replay produced.
    pub replay: T,
}

/// How the emitted logs differ.
///
/// A differing log count and a differing log field are independent findings:
/// both are reported when both apply, so truncated logs and rewritten logs are
/// distinguishable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(super) struct LogsDiff {
    /// Present when the two sides emitted a different number of logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<Mismatch<usize>>,
    /// The first log both sides emitted whose contents differ, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_mismatch: Option<LogFieldMismatch>,
}

impl LogsDiff {
    /// Whether the logs agreed.
    fn is_empty(&self) -> bool {
        self.count.is_none() && self.first_mismatch.is_none()
    }
}

/// The first differing field of the first differing log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct LogFieldMismatch {
    /// Position of the log in the transaction's log list.
    pub index: usize,
    /// Which field of the log differs.
    pub field: LogField,
    /// That field's value in the on-chain receipt.
    pub onchain: LogFieldValue,
    /// That field's value in the local replay.
    pub replay: LogFieldValue,
}

/// The log field a [`LogFieldMismatch`] reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum LogField {
    /// The emitting contract's address.
    Address,
    /// The indexed topics.
    Topics,
    /// The unindexed data payload.
    Data,
}

impl LogField {
    /// Wire name, shared by the JSON diff and the human verdict line.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Address => "address",
            Self::Topics => "topics",
            Self::Data => "data",
        }
    }
}

/// The value of the log field named by a [`LogFieldMismatch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub(super) enum LogFieldValue {
    /// An emitting contract address.
    Address(Address),
    /// A topic list.
    Topics(Vec<B256>),
    /// A data payload.
    Data(Bytes),
}

impl fmt::Display for LogFieldValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address(address) => write!(f, "{address}"),
            Self::Topics(topics) => {
                write!(f, "[")?;
                for (index, topic) in topics.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{topic}")?;
                }
                write!(f, "]")
            }
            Self::Data(data) => write!(f, "{data}"),
        }
    }
}

/// Compare the on-chain receipt against the local replay's receipt.
pub(super) fn compare(onchain: &ReceiptFacts, replay: &ReceiptFacts) -> VerificationOutcome {
    let mut diff = VerificationDiff::default();

    if onchain.status != replay.status {
        diff.status = Some(Mismatch { onchain: onchain.status, replay: replay.status });
    }
    if onchain.gas_used != replay.gas_used {
        diff.gas_used = Some(Mismatch { onchain: onchain.gas_used, replay: replay.gas_used });
    }
    let logs = compare_logs(&onchain.logs, &replay.logs);
    if !logs.is_empty() {
        diff.logs = Some(logs);
    }

    if diff.is_empty() {
        VerificationOutcome::compared(true, None)
    } else {
        VerificationOutcome::compared(false, Some(diff))
    }
}

/// Compare two log lists: their length, and the contents of the logs both sides
/// emitted.
fn compare_logs(onchain: &[Log], replay: &[Log]) -> LogsDiff {
    let count = (onchain.len() != replay.len())
        .then_some(Mismatch { onchain: onchain.len(), replay: replay.len() });
    // Only the logs both sides emitted can be compared field by field; a length
    // difference is already reported by `count`.
    let first_mismatch = onchain
        .iter()
        .zip(replay)
        .enumerate()
        .find_map(|(index, (onchain, replay))| compare_log(index, onchain, replay));
    LogsDiff { count, first_mismatch }
}

/// Report the first differing field of one log, if any.
fn compare_log(index: usize, onchain: &Log, replay: &Log) -> Option<LogFieldMismatch> {
    if onchain.address != replay.address {
        return Some(LogFieldMismatch {
            index,
            field: LogField::Address,
            onchain: LogFieldValue::Address(onchain.address),
            replay: LogFieldValue::Address(replay.address),
        });
    }
    if onchain.topics() != replay.topics() {
        return Some(LogFieldMismatch {
            index,
            field: LogField::Topics,
            onchain: LogFieldValue::Topics(onchain.topics().to_vec()),
            replay: LogFieldValue::Topics(replay.topics().to_vec()),
        });
    }
    if onchain.data.data != replay.data.data {
        return Some(LogFieldMismatch {
            index,
            field: LogField::Data,
            onchain: LogFieldValue::Data(onchain.data.data.clone()),
            replay: LogFieldValue::Data(replay.data.data.clone()),
        });
    }
    None
}

/// Fetch a transaction's on-chain receipt.
///
/// Uses the same call shape as the `--dump-fixture` path, so a run with
/// `--rpc.capture-file` records the receipt and a later offline run verifies
/// without network access.
///
/// A receipt the endpoint cannot serve — a transport failure, or a receipt
/// pruned below the endpoint's retention height — is an [`ReplayError::RpcError`]
/// so the target is reported as unverified rather than as a mismatch. So is a
/// receipt that describes a different transaction than the one requested: the
/// identity check runs here, at the one seam every mode fetches through, so no
/// caller can compare against or anchor to a receipt it never asked for.
pub(super) async fn fetch_receipt<P>(provider: &P, tx_hash: B256) -> Result<OpTransactionReceipt>
where
    P: Provider<op_alloy_network::Optimism>,
{
    let receipt = provider
        .get_transaction_receipt(tx_hash)
        .await
        .map_err(|e| ReplayError::RpcError(format!("Failed to fetch receipt: {e}")))?
        .ok_or_else(|| {
            ReplayError::RpcError(format!(
                "No on-chain receipt for transaction {tx_hash}: the transaction is unknown to \
                 the endpoint, or the endpoint has pruned its receipt"
            ))
        })?;
    check_transaction_identity(receipt.inner.transaction_hash, tx_hash)
        .map_err(ReplayError::RpcError)?;
    Ok(receipt)
}

/// A target's on-chain receipt, fetched once and admitted for every consumer of
/// this replay.
///
/// `--dump-fixture` anchors its fidelity gate to the receipt and
/// `--verify-receipt` compares the replay against it. A run asking for both must
/// answer them from one and the same receipt: two fetches let a reorg, or a
/// load-balanced endpoint serving divergent views, hand the two consumers
/// different receipts, and the fixture would then be anchored to one on-chain
/// execution while the verdict describes another. Admission — fetching the
/// receipt, checking it describes the requested transaction, and anchoring it to
/// the replayed block — therefore happens once, here, and the consumers read the
/// admitted receipt instead of asking again.
pub(super) struct ReceiptEvidence {
    /// The admitted receipt.
    receipt: OpTransactionReceipt,
}

impl ReceiptEvidence {
    /// Fetch the receipt of `tx_hash` and admit it for a replay of the block
    /// `replayed_block_hash`.
    ///
    /// Every way the receipt question can go unanswered — a transport failure, a
    /// pruned receipt, a receipt describing another transaction, a receipt
    /// describing another inclusion — is a [`ReplayError::RpcError`], so a target
    /// that could not be evidenced is reported as unverified rather than as a
    /// divergence.
    pub(super) async fn admit<P>(
        provider: &P,
        tx_hash: B256,
        replayed_block_hash: B256,
    ) -> Result<Self>
    where
        P: Provider<op_alloy_network::Optimism>,
    {
        let receipt = fetch_receipt(provider, tx_hash).await?;
        check_inclusion(receipt.block_hash(), replayed_block_hash)
            .map_err(ReplayError::RpcError)?;
        Ok(Self { receipt })
    }

    /// The admitted receipt, for a consumer that only reads it.
    pub(super) const fn receipt(&self) -> &OpTransactionReceipt {
        &self.receipt
    }

    /// The admitted receipt, for the consumer that keeps it past this replay.
    pub(super) fn into_receipt(self) -> OpTransactionReceipt {
        self.receipt
    }
}

/// Check that a fetched receipt describes the transaction it was requested for.
///
/// `eth_getTransactionReceipt` is asked by transaction hash, but nothing in the
/// answer forces the endpoint to honour it: an inconsistent backend, or a
/// tampered offline capture, can serve another transaction's receipt. Comparing
/// against it would report a verdict about the wrong transaction — a mismatch
/// blamed on the replay, or a spurious match when the two transactions happen to
/// share their consensus facts — and the dump path would anchor a fixture to it.
/// Returns the explanatory message so each mode can wrap it in the error shape it
/// reports.
pub(super) fn check_transaction_identity(
    receipt_tx_hash: B256,
    requested_tx_hash: B256,
) -> std::result::Result<(), String> {
    if receipt_tx_hash == requested_tx_hash {
        return Ok(());
    }
    Err(format!(
        "receipt is for transaction {receipt_tx_hash}, but transaction {requested_tx_hash} was \
         requested: the endpoint served the receipt of a different transaction (an inconsistent \
         backend, or a tampered capture); the transaction is unverified"
    ))
}

/// Check that a fetched transaction is the one it was requested for.
///
/// `eth_getTransactionByHash` is asked by transaction hash, but nothing in the
/// answer forces the endpoint to honour it: an inconsistent backend, or a
/// tampered offline capture, can serve another transaction under the requested
/// hash — and the replay would execute it, advancing the block state on the
/// wrong transaction or reporting another transaction's outcome under the
/// target's name. The served envelope is authenticated against the request
/// rather than trusted: the transaction hash is recomputed from the served
/// consensus encoding (the response's own `hash` field is as unauthenticated as
/// the rest of it), and the sender is re-derived from the signature, since the
/// served `from` field is not covered by the hash of a signed transaction (a
/// deposit's `from` is part of its encoding, so the hash already covers it).
/// Returns the explanatory message so each call site can wrap it in the error
/// shape it reports.
pub(super) fn authenticate_transaction(
    tx: &Transaction,
    requested_tx_hash: B256,
) -> std::result::Result<(), String> {
    let envelope = tx.inner.inner.inner();
    // Hash the consensus encoding directly: `trie_hash()`/`tx_hash()` return
    // the envelope's *cached* hash, which an RPC deserialization seeds from the
    // response's own `hash` field — the very value being authenticated.
    let computed = keccak256(envelope.encoded_2718());
    if computed != requested_tx_hash {
        return Err(format!(
            "the served transaction hashes to {computed}, but transaction {requested_tx_hash} \
             was requested: the endpoint served a different transaction (an inconsistent \
             backend, or a tampered capture)"
        ));
    }
    let recovered = envelope.recover_signer().map_err(|e| {
        format!(
            "transaction {requested_tx_hash}: the served transaction's signature does not \
             recover a signer ({e}): the endpoint served a corrupted transaction (an \
             inconsistent backend, or a tampered capture)"
        )
    })?;
    let served = tx.inner.inner.signer();
    if recovered != served {
        return Err(format!(
            "transaction {requested_tx_hash}: the served `from` address {served} does not match \
             the signer {recovered} recovered from the signature: the endpoint served an \
             inconsistent transaction (a corrupted backend, or a tampered capture)"
        ));
    }
    Ok(())
}

/// Check that a fetched receipt describes the block the replay executed.
///
/// Across a reorg, or against a load-balanced endpoint serving divergent views,
/// the receipt can describe a different inclusion than the block the replay ran,
/// which would compare the replay against the wrong on-chain execution. Returns
/// the explanatory message so each mode can wrap it in the error shape it
/// reports — a hard error in single-transaction mode, an `rpc` error entry in
/// batch mode.
pub(super) fn check_inclusion(
    receipt_block_hash: Option<B256>,
    replayed_block_hash: B256,
) -> std::result::Result<(), String> {
    match receipt_block_hash {
        Some(hash) if hash == replayed_block_hash => Ok(()),
        Some(hash) => Err(format!(
            "receipt block hash {hash} != replayed block hash {replayed_block_hash}: the receipt \
             describes a different inclusion than the replayed block (reorg in progress, or a \
             load-balanced endpoint serving divergent views); the transaction is unverified, \
             retry once the chain settles"
        )),
        // A receipt with no inclusion hash cannot be anchored to the replayed
        // block, so it is the same class of failure as a mismatched hash.
        None => Err(format!(
            "receipt has no block hash: cannot anchor the receipt to the replayed block \
             {replayed_block_hash} (reorg in progress, or a load-balanced endpoint serving \
             divergent views); the transaction is unverified, retry once the chain settles"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, LogData};

    const ADDR_A: Address = address!("0x00000000000000000000000000000000000000aa");
    const ADDR_B: Address = address!("0x00000000000000000000000000000000000000bb");
    const TOPIC_A: B256 =
        b256!("0x000000000000000000000000000000000000000000000000000000000000000a");
    const TOPIC_B: B256 =
        b256!("0x000000000000000000000000000000000000000000000000000000000000000b");

    /// Parse a log's hex-encoded data payload.
    fn data(hex: &str) -> Bytes {
        hex.parse().expect("valid hex payload")
    }

    /// Build a log from its three compared fields.
    fn log(address: Address, topics: &[B256], data: Bytes) -> Log {
        Log {
            address,
            data: LogData::new(topics.to_vec(), data).expect("topic count within bounds"),
        }
    }

    /// A successful 21,000-gas receipt emitting the given logs.
    fn facts(logs: Vec<Log>) -> ReceiptFacts {
        ReceiptFacts { status: true, gas_used: 21_000, logs }
    }

    /// The `diff` of an outcome that must be a mismatch.
    fn diff_of(outcome: &VerificationOutcome) -> &VerificationDiff {
        assert!(!outcome.matched, "expected a mismatch, got {outcome:?}");
        outcome.diff.as_ref().expect("a mismatch always carries a diff")
    }

    /// Serialize an outcome the way the JSON output does.
    fn json(outcome: &VerificationOutcome) -> serde_json::Value {
        serde_json::to_value(outcome).expect("outcome is serializable")
    }

    #[test]
    fn test_compare_equal_receipts_match() {
        let onchain = facts(vec![log(ADDR_A, &[TOPIC_A], data("0xdeadbeef"))]);
        let replay = onchain.clone();

        let outcome = compare(&onchain, &replay);

        assert!(outcome.matched);
        assert!(outcome.diff.is_none(), "a match carries no diff");
        assert_eq!(json(&outcome), serde_json::json!({ "match": true }));
        assert_eq!(outcome.verdict_line(), "verification: MATCH");
    }

    /// An unanswered receipt serializes as `{"error": …}` with no `match` field,
    /// so consumers never read it as a false mismatch.
    #[test]
    fn test_unavailable_outcome_serializes_as_error_only() {
        let outcome = VerificationOutcome::unavailable("receipt pruned below retention");
        assert!(outcome.is_unavailable());
        assert_eq!(
            json(&outcome),
            serde_json::json!({ "error": "receipt pruned below retention" })
        );
        assert_eq!(outcome.verdict_line(), "verification: FAILED (receipt pruned below retention)");
    }

    #[test]
    fn test_compare_empty_logs_on_both_sides_match() {
        let outcome = compare(&facts(vec![]), &facts(vec![]));

        assert!(outcome.matched);
        assert_eq!(json(&outcome), serde_json::json!({ "match": true }));
    }

    #[test]
    fn test_compare_reports_status_flip() {
        let onchain = facts(vec![]);
        let replay = ReceiptFacts { status: false, ..facts(vec![]) };

        let outcome = compare(&onchain, &replay);

        let diff = diff_of(&outcome);
        assert_eq!(diff.status, Some(Mismatch { onchain: true, replay: false }));
        assert!(diff.gas_used.is_none(), "gas agreed, so it must be absent: {diff:?}");
        assert!(diff.logs.is_none(), "logs agreed, so they must be absent: {diff:?}");
        assert_eq!(
            json(&outcome),
            serde_json::json!({
                "match": false,
                "diff": { "status": { "onchain": true, "replay": false } },
            })
        );
        assert_eq!(
            outcome.verdict_line(),
            "verification: MISMATCH (status: onchain true vs replay false)"
        );
    }

    #[test]
    fn test_compare_reports_gas_delta() {
        let onchain = facts(vec![]);
        let replay = ReceiptFacts { gas_used: 22_000, ..facts(vec![]) };

        let outcome = compare(&onchain, &replay);

        let diff = diff_of(&outcome);
        assert_eq!(diff.gas_used, Some(Mismatch { onchain: 21_000, replay: 22_000 }));
        assert!(diff.status.is_none(), "status agreed, so it must be absent: {diff:?}");
        assert_eq!(
            json(&outcome),
            serde_json::json!({
                "match": false,
                "diff": { "gas_used": { "onchain": 21000, "replay": 22000 } },
            })
        );
        assert_eq!(
            outcome.verdict_line(),
            "verification: MISMATCH (gas_used: onchain 21000 vs replay 22000)"
        );
    }

    #[test]
    fn test_compare_reports_log_count_delta() {
        let entry = log(ADDR_A, &[TOPIC_A], data("0x"));
        let onchain = facts(vec![entry.clone(), entry.clone()]);
        let replay = facts(vec![entry]);

        let outcome = compare(&onchain, &replay);

        let logs = diff_of(&outcome).logs.as_ref().expect("logs differ");
        assert_eq!(logs.count, Some(Mismatch { onchain: 2, replay: 1 }));
        assert!(
            logs.first_mismatch.is_none(),
            "the shared prefix is identical, so no field mismatch: {logs:?}"
        );
        assert_eq!(
            json(&outcome),
            serde_json::json!({
                "match": false,
                "diff": { "logs": { "count": { "onchain": 2, "replay": 1 } } },
            })
        );
    }

    #[test]
    fn test_compare_reports_log_address_delta() {
        let onchain = facts(vec![log(ADDR_A, &[TOPIC_A], data("0x"))]);
        let replay = facts(vec![log(ADDR_B, &[TOPIC_A], data("0x"))]);

        let outcome = compare(&onchain, &replay);

        let logs = diff_of(&outcome).logs.as_ref().expect("logs differ");
        assert!(logs.count.is_none(), "both sides emitted one log: {logs:?}");
        assert_eq!(
            logs.first_mismatch,
            Some(LogFieldMismatch {
                index: 0,
                field: LogField::Address,
                onchain: LogFieldValue::Address(ADDR_A),
                replay: LogFieldValue::Address(ADDR_B),
            })
        );
        assert_eq!(
            json(&outcome)["diff"]["logs"]["first_mismatch"],
            serde_json::json!({
                "index": 0,
                "field": "address",
                "onchain": "0x00000000000000000000000000000000000000aa",
                "replay": "0x00000000000000000000000000000000000000bb",
            })
        );
    }

    #[test]
    fn test_compare_reports_log_topics_delta() {
        let onchain = facts(vec![log(ADDR_A, &[TOPIC_A], data("0x"))]);
        let replay = facts(vec![log(ADDR_A, &[TOPIC_A, TOPIC_B], data("0x"))]);

        let outcome = compare(&onchain, &replay);

        let first = diff_of(&outcome).logs.as_ref().and_then(|l| l.first_mismatch.clone());
        assert_eq!(
            first,
            Some(LogFieldMismatch {
                index: 0,
                field: LogField::Topics,
                onchain: LogFieldValue::Topics(vec![TOPIC_A]),
                replay: LogFieldValue::Topics(vec![TOPIC_A, TOPIC_B]),
            })
        );
        assert_eq!(json(&outcome)["diff"]["logs"]["first_mismatch"]["field"], "topics");
    }

    #[test]
    fn test_compare_reports_log_data_delta() {
        let onchain = facts(vec![log(ADDR_A, &[TOPIC_A], data("0xdeadbeef"))]);
        let replay = facts(vec![log(ADDR_A, &[TOPIC_A], data("0xfeedface"))]);

        let outcome = compare(&onchain, &replay);

        let first = diff_of(&outcome).logs.as_ref().and_then(|l| l.first_mismatch.clone());
        assert_eq!(
            first,
            Some(LogFieldMismatch {
                index: 0,
                field: LogField::Data,
                onchain: LogFieldValue::Data(data("0xdeadbeef")),
                replay: LogFieldValue::Data(data("0xfeedface")),
            })
        );
        assert_eq!(
            json(&outcome)["diff"]["logs"]["first_mismatch"],
            serde_json::json!({
                "index": 0,
                "field": "data",
                "onchain": "0xdeadbeef",
                "replay": "0xfeedface",
            })
        );
    }

    /// The reported log mismatch is the first differing one, and a later
    /// difference does not displace it.
    #[test]
    fn test_compare_reports_the_first_differing_log() {
        let same = log(ADDR_A, &[TOPIC_A], data("0x"));
        let onchain = facts(vec![same.clone(), same.clone(), same.clone()]);
        let replay = facts(vec![
            same,
            log(ADDR_B, &[TOPIC_A], data("0x")),
            log(ADDR_A, &[TOPIC_A], data("0xff")),
        ]);

        let outcome = compare(&onchain, &replay);

        let first = diff_of(&outcome).logs.as_ref().and_then(|l| l.first_mismatch.clone());
        assert_eq!(first.map(|m| (m.index, m.field)), Some((1, LogField::Address)));
    }

    /// Every mismatched dimension is reported at once — a status flip does not
    /// hide the gas delta or the log difference behind it.
    #[test]
    fn test_compare_reports_all_mismatched_dimensions() {
        let onchain = facts(vec![log(ADDR_A, &[TOPIC_A], data("0x"))]);
        let replay = ReceiptFacts {
            status: false,
            gas_used: 30_000,
            logs: vec![log(ADDR_B, &[TOPIC_A], data("0x")), log(ADDR_A, &[], data("0x"))],
        };

        let outcome = compare(&onchain, &replay);

        let diff = diff_of(&outcome);
        assert!(diff.status.is_some() && diff.gas_used.is_some());
        let logs = diff.logs.as_ref().expect("logs differ");
        assert_eq!(logs.count, Some(Mismatch { onchain: 1, replay: 2 }));
        assert_eq!(logs.first_mismatch.as_ref().map(|m| m.field), Some(LogField::Address));
        assert_eq!(
            outcome.verdict_line(),
            format!(
                "verification: MISMATCH (status: onchain true vs replay false, \
                 gas_used: onchain 21000 vs replay 30000, logs_count: onchain 1 vs replay 2, \
                 logs[0].address: onchain {ADDR_A} vs replay {ADDR_B})"
            )
        );
    }

    #[test]
    fn test_check_inclusion_accepts_the_replayed_block() {
        let hash = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");

        assert!(check_inclusion(Some(hash), hash).is_ok());
    }

    #[test]
    fn test_check_inclusion_rejects_a_different_inclusion() {
        let message = check_inclusion(
            Some(b256!("0x1111111111111111111111111111111111111111111111111111111111111111")),
            b256!("0x2222222222222222222222222222222222222222222222222222222222222222"),
        )
        .expect_err("a receipt from another block must be rejected");

        assert!(
            message.contains("different inclusion") && message.contains("unverified"),
            "message must explain the reorg and that the target is unverified: {message}"
        );
    }

    #[test]
    fn test_check_transaction_identity_accepts_the_requested_transaction() {
        let hash = b256!("0x3333333333333333333333333333333333333333333333333333333333333333");

        assert!(check_transaction_identity(hash, hash).is_ok());
    }

    /// A receipt for another transaction is rejected, and the message names both
    /// hashes so the served/requested confusion is diagnosable from the error
    /// alone.
    #[test]
    fn test_check_transaction_identity_rejects_another_transactions_receipt() {
        let served = b256!("0x3333333333333333333333333333333333333333333333333333333333333333");
        let requested = b256!("0x4444444444444444444444444444444444444444444444444444444444444444");

        let message = check_transaction_identity(served, requested)
            .expect_err("a receipt for another transaction must be rejected");

        assert!(
            message.contains(&format!("{served}")) &&
                message.contains(&format!("{requested}")) &&
                message.contains("different transaction") &&
                message.contains("unverified"),
            "message must name both hashes and explain the target is unverified: {message}"
        );
    }

    #[test]
    fn test_check_inclusion_rejects_a_missing_block_hash() {
        let replayed = b256!("0x2222222222222222222222222222222222222222222222222222222222222222");
        let message = check_inclusion(None, replayed)
            .expect_err("a receipt without a block hash must be rejected");

        assert!(
            message.contains("no block hash") &&
                message.contains("unverified") &&
                message.contains(&format!("{replayed}")),
            "message must explain the missing anchor and name the replayed block: {message}"
        );
    }
}
