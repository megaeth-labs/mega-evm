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

use alloy_network::ReceiptResponse;
use alloy_primitives::{keccak256, Address, Bytes, Log, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Log as RpcLog;
use mega_evm::{alloy_consensus::transaction::SignerRecoverable, alloy_eips::Encodable2718};
use op_alloy_consensus::OpReceiptEnvelope;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use serde::Serialize;

use crate::common::OpTxReceipt;

use super::{coherence, ReplayError, Result};

/// The consensus facts compared between the on-chain receipt and the receipt
/// the local replay produced.
///
/// Every field of the consensus receipt encoding is covered — the envelope type,
/// the status, the block-cumulative gas, the logs, and a deposit receipt's nonce
/// and version — so a matching receipt is the one the block's `receiptsRoot`
/// commits to, plus the transaction's own gas used. The bloom is covered through
/// the logs: an on-chain receipt is only admitted when its bloom is the bloom of
/// its own logs ([`fetch_receipt`]), and the replay's bloom is built from its
/// logs, so equal logs mean equal blooms. The RPC-only fields
/// (`contractAddress`, `effectiveGasPrice`, the L1 fee fields) are not part of
/// the consensus encoding and are not compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReceiptFacts {
    /// Whether the transaction succeeded.
    pub status: bool,
    /// Gas the transaction used.
    pub gas_used: u64,
    /// Gas the block had used up to and including this transaction.
    pub cumulative_gas_used: u64,
    /// EIP-2718 type of the receipt envelope.
    pub tx_type: u8,
    /// Deposit nonce of a deposit receipt; `None` for any other type.
    pub deposit_nonce: Option<u64>,
    /// Deposit receipt version of a deposit receipt; `None` for any other type.
    pub deposit_receipt_version: Option<u64>,
    /// The consensus logs the transaction emitted, in order.
    pub logs: Vec<Log>,
}

impl ReceiptFacts {
    /// Extract the compared facts from the receipt the local replay built.
    pub(super) fn from_receipt(receipt: &OpTxReceipt) -> Self {
        Self::from_envelope(&receipt.inner, receipt.gas_used)
    }

    /// Extract the compared facts from an on-chain RPC receipt.
    pub(super) fn from_onchain(receipt: &OpTransactionReceipt) -> Self {
        Self::from_envelope(&onchain_envelope(receipt), receipt.inner.gas_used)
    }

    /// Extract the compared facts from a receipt envelope.
    ///
    /// Both sides go through this one accessor set — the on-chain side is the
    /// RPC receipt's envelope, the local side the envelope the replay built —
    /// so neither side can be read with different semantics.
    fn from_envelope(envelope: &OpReceiptEnvelope<RpcLog>, gas_used: u64) -> Self {
        Self {
            status: envelope.status(),
            gas_used,
            cumulative_gas_used: envelope.cumulative_gas_used(),
            tx_type: envelope.tx_type() as u8,
            deposit_nonce: envelope.deposit_nonce(),
            deposit_receipt_version: envelope.deposit_receipt_version(),
            logs: envelope.logs().iter().map(|log| log.inner.clone()).collect(),
        }
    }
}

/// The receipt envelope an on-chain RPC receipt carries, with its RPC logs.
fn onchain_envelope(receipt: &OpTransactionReceipt) -> OpReceiptEnvelope<RpcLog> {
    receipt.inner.inner.clone().into()
}

/// The consensus receipt an on-chain RPC receipt describes, as the block's
/// `receiptsRoot` commits to it: the served envelope (bloom included) with its
/// logs stripped to their consensus fields.
pub(super) fn consensus_receipt(receipt: &OpTransactionReceipt) -> OpReceiptEnvelope {
    onchain_envelope(receipt).map_logs(|log| log.inner)
}

/// The verdict of one verification: of a transaction against its on-chain
/// receipt, or of a block against its header.
///
/// Three shapes on the wire:
/// - compared and equal: `{"match": true}`
/// - compared and diverged: `{"match": false, "diff": …}`
/// - the comparison could not run: `{"error": "…"}` — the receipt question went unanswered
///   (transport, pruned, reorg), or the block did not execute in full.
///
/// Serialize is hand-written so an unavailable verdict never emits a false
/// `match` that a consumer would read as a divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Verdict<D> {
    /// Whether the replay reproduced every compared dimension.
    ///
    /// Meaningless when [`Self::error`] is set (kept for a simple bool check
    /// on the compared path); the wire shape omits `match` in that case.
    pub matched: bool,
    /// The mismatched dimensions; absent when the replay matched or when the
    /// comparison never ran.
    pub diff: Option<D>,
    /// Why the comparison could not run. Mutually exclusive with a real
    /// match/diff.
    pub error: Option<String>,
}

/// What a [`Verdict`]'s diff contributes to the human verdict line.
pub(super) trait DiffReport: Serialize {
    /// Label the human verdict line starts with.
    const LABEL: &'static str;

    /// Render every mismatched dimension as one comma-separated line.
    fn describe(&self) -> String;
}

/// The verdict for one verified transaction.
pub(super) type VerificationOutcome = Verdict<VerificationDiff>;

impl<D: DiffReport> Verdict<D> {
    /// A completed comparison: a match when nothing diverged.
    pub(super) fn compared(diff: Option<D>) -> Self {
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

    /// The one-line human verdict.
    pub(super) fn verdict_line(&self) -> String {
        let label = D::LABEL;
        if let Some(error) = &self.error {
            format!("{label}: FAILED ({error})")
        } else if let Some(diff) = &self.diff {
            format!("{label}: MISMATCH ({})", diff.describe())
        } else {
            format!("{label}: MATCH")
        }
    }
}

impl<D: Serialize> Serialize for Verdict<D> {
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
    /// Present when the block-cumulative gas at this transaction differs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_gas_used: Option<Mismatch<u64>>,
    /// Present when the receipt envelope types differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_type: Option<Mismatch<u8>>,
    /// Present when the deposit nonces differ (`null` on a non-deposit side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deposit_nonce: Option<Mismatch<Option<u64>>>,
    /// Present when the deposit receipt versions differ (`null` on a
    /// non-deposit side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deposit_receipt_version: Option<Mismatch<Option<u64>>>,
    /// Present when the emitted logs differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<LogsDiff>,
}

impl VerificationDiff {
    /// Whether every compared dimension agreed.
    fn is_empty(&self) -> bool {
        self.status.is_none() &&
            self.gas_used.is_none() &&
            self.cumulative_gas_used.is_none() &&
            self.tx_type.is_none() &&
            self.deposit_nonce.is_none() &&
            self.deposit_receipt_version.is_none() &&
            self.logs.is_none()
    }
}

impl DiffReport for VerificationDiff {
    const LABEL: &'static str = "verification";

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(m) = &self.status {
            parts.push(format!("status: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if let Some(m) = &self.gas_used {
            parts.push(format!("gas_used: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if let Some(m) = &self.cumulative_gas_used {
            parts
                .push(format!("cumulative_gas_used: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if let Some(m) = &self.tx_type {
            parts.push(format!("tx_type: onchain {} vs replay {}", m.onchain, m.replay));
        }
        if let Some(m) = &self.deposit_nonce {
            parts.push(format!(
                "deposit_nonce: onchain {} vs replay {}",
                display_optional(m.onchain.as_ref()),
                display_optional(m.replay.as_ref())
            ));
        }
        if let Some(m) = &self.deposit_receipt_version {
            parts.push(format!(
                "deposit_receipt_version: onchain {} vs replay {}",
                display_optional(m.onchain.as_ref()),
                display_optional(m.replay.as_ref())
            ));
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

/// Render an optional field (a deposit field, a header field) for the human
/// verdict line: its value, or `none` for a side that lacks it.
pub(super) fn display_optional<T: fmt::Display>(value: Option<&T>) -> String {
    value.map_or_else(|| "none".to_string(), ToString::to_string)
}

/// One dimension's two values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Mismatch<T> {
    /// The value the on-chain side reports.
    pub onchain: T,
    /// The value the local replay produced.
    pub replay: T,
}

/// The two values of one dimension, when they differ.
pub(super) fn mismatch<T: PartialEq>(onchain: T, replay: T) -> Option<Mismatch<T>> {
    (onchain != replay).then_some(Mismatch { onchain, replay })
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
    let logs = compare_logs(&onchain.logs, &replay.logs);
    let diff = VerificationDiff {
        status: mismatch(onchain.status, replay.status),
        gas_used: mismatch(onchain.gas_used, replay.gas_used),
        cumulative_gas_used: mismatch(onchain.cumulative_gas_used, replay.cumulative_gas_used),
        tx_type: mismatch(onchain.tx_type, replay.tx_type),
        deposit_nonce: mismatch(onchain.deposit_nonce, replay.deposit_nonce),
        deposit_receipt_version: mismatch(
            onchain.deposit_receipt_version,
            replay.deposit_receipt_version,
        ),
        logs: (!logs.is_empty()).then_some(logs),
    };
    VerificationOutcome::compared((!diff.is_empty()).then_some(diff))
}

/// Compare two log lists: their length, and the contents of the logs both sides
/// emitted.
fn compare_logs(onchain: &[Log], replay: &[Log]) -> LogsDiff {
    let count = mismatch(onchain.len(), replay.len());
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
/// caller can compare against or anchor to a receipt it never asked for. And so
/// is a receipt whose logs bloom is not the bloom of its own logs, checked here
/// for the same reason: a receipt that contradicts itself describes no
/// execution, and its logs could otherwise match while its bloom is forged.
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
    let envelope = onchain_envelope(&receipt);
    let logs: Vec<Log> = envelope.logs().iter().map(|log| log.inner.clone()).collect();
    coherence::require_receipt_bloom(tx_hash, *envelope.logs_bloom(), &logs)
        .map_err(|incoherence| ReplayError::RpcError(incoherence.to_string()))?;
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
///
/// The served `hash` field is checked against the recomputed value too, rather
/// than merely ignored. An RPC deserialization seeds the envelope's cached hash
/// from that field, so it is what every later `tx_hash()` read returns — the
/// name a dumped fixture is filed and keyed under, and the identity a result
/// line carries. A payload that honestly hashes to the requested transaction
/// paired with a `hash` field naming another one would pass a request-only
/// comparison and then misfile everything downstream reads that accessor for.
/// Because the check runs here, at the one seam every fetched transaction is
/// admitted through, `tx_hash()` on an authenticated transaction is a verified
/// value and no consumer has to recompute it.
///
/// Returns the EIP-2718 encoding the hash was recomputed from, so a caller that
/// needs the authenticated bytes (the transactions root of an executed block)
/// does not encode the transaction a second time.
pub(super) fn authenticate_transaction(
    tx: &Transaction,
    requested_tx_hash: B256,
) -> std::result::Result<Bytes, String> {
    let envelope = tx.inner.inner.inner();
    // Hash the consensus encoding directly: `trie_hash()`/`tx_hash()` return
    // the envelope's *cached* hash, which an RPC deserialization seeds from the
    // response's own `hash` field — the very value being authenticated.
    let encoded = envelope.encoded_2718();
    let computed = keccak256(&encoded);
    if computed != requested_tx_hash {
        return Err(format!(
            "the served transaction hashes to {computed}, but transaction {requested_tx_hash} \
             was requested: the endpoint served a different transaction (an inconsistent \
             backend, or a tampered capture)"
        ));
    }
    // The cached hash every downstream `tx_hash()` reads, which is the served
    // `hash` field rather than a property of the body beside it.
    let served_hash = tx.inner.inner.tx_hash();
    if served_hash != computed {
        return Err(format!(
            "transaction {requested_tx_hash}: the served `hash` field {served_hash} does not \
             match the {computed} its own consensus encoding hashes to: the endpoint served an \
             inconsistent transaction (a corrupted backend, or a tampered capture)"
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
    Ok(encoded.into())
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

    /// A successful 21,000-gas EIP-1559 receipt emitting the given logs, second
    /// in its block.
    fn facts(logs: Vec<Log>) -> ReceiptFacts {
        ReceiptFacts {
            status: true,
            gas_used: 21_000,
            cumulative_gas_used: 42_000,
            tx_type: 2,
            deposit_nonce: None,
            deposit_receipt_version: None,
            logs,
        }
    }

    /// A deposit receipt's facts: type `0x7e` with its nonce and version.
    fn deposit_facts(deposit_nonce: Option<u64>, version: Option<u64>) -> ReceiptFacts {
        ReceiptFacts {
            tx_type: 0x7e,
            deposit_nonce,
            deposit_receipt_version: version,
            ..facts(vec![])
        }
    }

    /// An on-chain RPC receipt as an endpoint serves it: a successful base with
    /// `fields` merged over it.
    fn onchain_receipt(fields: serde_json::Value) -> OpTransactionReceipt {
        let mut receipt = serde_json::json!({
            "status": "0x1",
            "cumulativeGasUsed": "0xb",
            "logs": [],
            "logsBloom": format!("0x{}", "0".repeat(512)),
            "transactionHash": b256!("0x1111111111111111111111111111111111111111111111111111111111111111"),
            "transactionIndex": "0x0",
            "blockHash": b256!("0x2222222222222222222222222222222222222222222222222222222222222222"),
            "blockNumber": "0x1",
            "gasUsed": "0xa",
            "effectiveGasPrice": "0x0",
            "from": ADDR_A,
            "to": ADDR_B,
            "contractAddress": null
        });
        for (key, value) in fields.as_object().expect("fields are an object") {
            receipt[key] = value.clone();
        }
        serde_json::from_value(receipt).expect("a well-formed receipt")
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
            ..facts(vec![])
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

    /// The block-cumulative gas is compared even when the transaction's own gas
    /// agrees: a preceding transaction that used different gas shifts it.
    #[test]
    fn test_compare_reports_cumulative_gas_delta() {
        let onchain = facts(vec![]);
        let replay = ReceiptFacts { cumulative_gas_used: 43_000, ..facts(vec![]) };

        let outcome = compare(&onchain, &replay);

        let diff = diff_of(&outcome);
        assert!(diff.gas_used.is_none(), "own gas agreed, so it must be absent: {diff:?}");
        assert_eq!(diff.cumulative_gas_used, Some(Mismatch { onchain: 42_000, replay: 43_000 }));
        assert_eq!(
            json(&outcome),
            serde_json::json!({
                "match": false,
                "diff": { "cumulative_gas_used": { "onchain": 42_000, "replay": 43_000 } }
            })
        );
        assert_eq!(
            outcome.verdict_line(),
            "verification: MISMATCH (cumulative_gas_used: onchain 42000 vs replay 43000)"
        );
    }

    #[test]
    fn test_compare_reports_tx_type_delta() {
        let onchain = facts(vec![]);
        let replay = ReceiptFacts { tx_type: 0, ..facts(vec![]) };

        let outcome = compare(&onchain, &replay);

        assert_eq!(diff_of(&outcome).tx_type, Some(Mismatch { onchain: 2, replay: 0 }));
        assert_eq!(
            json(&outcome),
            serde_json::json!({
                "match": false,
                "diff": { "tx_type": { "onchain": 2, "replay": 0 } }
            })
        );
        assert_eq!(
            outcome.verdict_line(),
            "verification: MISMATCH (tx_type: onchain 2 vs replay 0)"
        );
    }

    #[test]
    fn test_compare_equal_deposit_receipts_match() {
        let outcome = compare(&deposit_facts(Some(7), Some(1)), &deposit_facts(Some(7), Some(1)));

        assert!(outcome.matched, "{outcome:?}");
    }

    #[test]
    fn test_compare_reports_deposit_nonce_and_version_deltas() {
        let onchain = deposit_facts(Some(7), Some(1));
        let replay = deposit_facts(Some(8), None);

        let outcome = compare(&onchain, &replay);

        let diff = diff_of(&outcome);
        assert_eq!(diff.deposit_nonce, Some(Mismatch { onchain: Some(7), replay: Some(8) }));
        assert_eq!(diff.deposit_receipt_version, Some(Mismatch { onchain: Some(1), replay: None }));
        assert_eq!(
            json(&outcome)["diff"],
            serde_json::json!({
                "deposit_nonce": { "onchain": 7, "replay": 8 },
                "deposit_receipt_version": { "onchain": 1, "replay": null }
            })
        );
        assert_eq!(
            outcome.verdict_line(),
            "verification: MISMATCH (deposit_nonce: onchain 7 vs replay 8, \
             deposit_receipt_version: onchain 1 vs replay none)"
        );
    }

    /// A deposit receipt compared against a non-deposit one reports the type and
    /// both deposit fields, the non-deposit side as `none`.
    #[test]
    fn test_compare_reports_a_deposit_against_a_non_deposit() {
        let onchain = deposit_facts(Some(3), Some(1));
        let replay = facts(vec![]);

        let outcome = compare(&onchain, &replay);

        let diff = diff_of(&outcome);
        assert_eq!(diff.tx_type, Some(Mismatch { onchain: 0x7e, replay: 2 }));
        assert_eq!(diff.deposit_nonce, Some(Mismatch { onchain: Some(3), replay: None }));
        assert_eq!(diff.deposit_receipt_version, Some(Mismatch { onchain: Some(1), replay: None }));
        assert_eq!(
            outcome.verdict_line(),
            "verification: MISMATCH (tx_type: onchain 126 vs replay 2, deposit_nonce: onchain 3 \
             vs replay none, deposit_receipt_version: onchain 1 vs replay none)"
        );
    }

    /// An on-chain deposit receipt is read with its envelope type, cumulative
    /// gas, nonce and version — the fields its consensus encoding commits to.
    #[test]
    fn test_from_onchain_reads_the_deposit_receipt_fields() {
        let receipt = onchain_receipt(serde_json::json!({
            "type": "0x7e",
            "depositNonce": "0x5",
            "depositReceiptVersion": "0x1"
        }));

        assert_eq!(
            ReceiptFacts::from_onchain(&receipt),
            ReceiptFacts {
                status: true,
                gas_used: 10,
                cumulative_gas_used: 11,
                tx_type: 0x7e,
                deposit_nonce: Some(5),
                deposit_receipt_version: Some(1),
                logs: vec![],
            }
        );
    }

    /// A non-deposit on-chain receipt carries no deposit fields.
    #[test]
    fn test_from_onchain_reads_a_non_deposit_receipt() {
        let receipt = onchain_receipt(serde_json::json!({ "type": "0x2", "status": "0x0" }));

        assert_eq!(
            ReceiptFacts::from_onchain(&receipt),
            ReceiptFacts {
                status: false,
                gas_used: 10,
                cumulative_gas_used: 11,
                tx_type: 2,
                deposit_nonce: None,
                deposit_receipt_version: None,
                logs: vec![],
            }
        );
    }

    /// The local receipt is read through the same envelope accessors: the type,
    /// the cumulative gas and the deposit fields come from the envelope the
    /// replay built, the transaction's own gas from the receipt beside it.
    #[test]
    fn test_from_receipt_reads_the_local_deposit_envelope() {
        let envelope = OpReceiptEnvelope::Deposit(
            op_alloy_consensus::OpDepositReceipt {
                inner: alloy_consensus::Receipt {
                    status: alloy_consensus::Eip658Value::Eip658(true),
                    cumulative_gas_used: 77,
                    logs: vec![log(ADDR_A, &[TOPIC_B], data("0x01"))],
                },
                deposit_nonce: Some(9),
                deposit_receipt_version: Some(1),
            }
            .with_bloom(),
        );
        let receipt = crate::common::op_receipt_to_tx_receipt(
            &envelope,
            1,
            0,
            ADDR_A,
            Some(ADDR_B),
            None,
            0,
            55,
            None,
            None,
            0,
            0,
        );

        assert_eq!(
            ReceiptFacts::from_receipt(&receipt),
            ReceiptFacts {
                status: true,
                gas_used: 55,
                cumulative_gas_used: 77,
                tx_type: 0x7e,
                deposit_nonce: Some(9),
                deposit_receipt_version: Some(1),
                logs: vec![log(ADDR_A, &[TOPIC_B], data("0x01"))],
            }
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
