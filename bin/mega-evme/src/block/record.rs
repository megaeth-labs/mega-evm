//! What a replayed block is reported as, and how it compares with the chain.
//!
//! One record per transaction and one per block. The columns every engine has come first; a
//! Satin record adds the `satin` object and nothing else, so a legacy record's columns are a
//! subset of a Satin record's.

use alloy_primitives::{Bytes, B256};
use serde::Serialize;

use super::{
    exec::{ExecutedBlock, TxResult},
    inputs::{BlockInputs, ChainReceipt},
};
use crate::{common::SatinReport, engine::Engine};

/// What the chain recorded for a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ChainView {
    /// `success` or `failure`: a receipt records whether the transaction succeeded, not how it
    /// failed.
    pub status: &'static str,
    /// Gas used.
    pub gas_used: u64,
    /// The block's gas used after the transaction.
    pub cumulative_gas_used: u64,
    /// Number of logs.
    pub logs: usize,
}

/// One replayed transaction.
#[derive(Debug, Clone, Serialize)]
pub struct TxRecord {
    /// Always `tx`.
    pub kind: &'static str,
    /// The block number.
    pub block: u64,
    /// The transaction's index in the block.
    pub index: usize,
    /// The transaction's hash.
    pub hash: B256,
    /// The engine it ran on.
    pub engine: &'static str,
    /// The spec it ran under.
    pub spec: String,
    /// `success`, `revert`, `halt`, or `refused` (the engine did not include it).
    pub status: &'static str,
    /// Gas used; zero when refused.
    pub gas_used: u64,
    /// The block's gas used after the transaction.
    pub cumulative_gas_used: u64,
    /// Number of logs.
    pub logs: usize,
    /// Why it reverted, halted or was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What the chain recorded.
    pub chain: ChainView,
    /// The fields that differ from the chain's: `refused`, `status`, `gas_used`,
    /// `cumulative_gas_used`, `logs`.
    pub differs: Vec<&'static str>,
    /// What Satin counted; only on a Satin record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub satin: Option<SatinReport>,
}

/// One replayed block.
#[derive(Debug, Clone, Serialize)]
pub struct BlockRecord {
    /// Always `block`.
    pub kind: &'static str,
    /// The block number.
    pub block: u64,
    /// The block's hash.
    pub hash: B256,
    /// The engine it ran on.
    pub engine: &'static str,
    /// The spec it ran under.
    pub spec: String,
    /// Number of transactions.
    pub transactions: usize,
    /// Transactions the engine refused.
    pub refused: usize,
    /// Transactions whose record differs from the chain's.
    pub differing: usize,
    /// The block's gas used as replayed.
    pub gas_used: u64,
    /// The block's gas used as the chain recorded it.
    pub chain_gas_used: u64,
    /// The receipts root of the replayed receipts.
    pub receipts_root: B256,
    /// The receipts root in the block's header.
    pub chain_receipts_root: B256,
    /// Whether every transaction and the receipts root match the chain.
    pub matches_chain: bool,
    /// Reads the block's pre-state did not hold, served by the parent block over RPC.
    pub rpc_reads: usize,
    /// What Satin counted over the block's included transactions; only on a Satin record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub satin: Option<SatinReport>,
}

/// The receipts root of `encoded` receipts: the ordered trie of their EIP-2718 encodings.
pub fn receipts_root(encoded: &[Bytes]) -> B256 {
    alloy_trie::root::ordered_trie_root_with_encoder(encoded, |receipt, buf| {
        buf.extend_from_slice(receipt)
    })
}

/// The records of `executed`, compared with the chain's receipts in `inputs`.
pub fn records(
    inputs: &BlockInputs,
    executed: &ExecutedBlock,
    engine: Engine,
    spec: &str,
    rpc_reads: usize,
) -> (Vec<TxRecord>, BlockRecord) {
    let number = inputs.header.number;
    let mut previous = 0;
    let mut txs = Vec::with_capacity(executed.transactions.len());
    for (index, ((tx, result), chain)) in
        inputs.transactions.iter().zip(&executed.transactions).zip(&inputs.receipts).enumerate()
    {
        let chain_view = chain_view(chain);
        let mut record = TxRecord {
            kind: "tx",
            block: number,
            index,
            hash: tx.hash,
            engine: engine.name(),
            spec: spec.to_string(),
            status: "refused",
            gas_used: 0,
            cumulative_gas_used: previous,
            logs: 0,
            reason: None,
            chain: chain_view,
            differs: Vec::new(),
            satin: None,
        };
        match result {
            TxResult::Refused { reason } => {
                record.reason = Some(reason.clone());
                record.differs.push("refused");
            }
            TxResult::Included { status, reason, receipt, satin } => {
                record.status = status.name();
                record.gas_used = receipt.cumulative_gas_used.saturating_sub(previous);
                record.cumulative_gas_used = receipt.cumulative_gas_used;
                record.logs = receipt.logs.len();
                record.reason.clone_from(reason);
                record.satin = *satin;
                previous = receipt.cumulative_gas_used;
                if receipt.success != chain.success() {
                    record.differs.push("status");
                }
                if record.gas_used != chain_view.gas_used {
                    record.differs.push("gas_used");
                }
                if record.cumulative_gas_used != chain_view.cumulative_gas_used {
                    record.differs.push("cumulative_gas_used");
                }
                if !chain.logs_equal(&receipt.logs) {
                    record.differs.push("logs");
                }
            }
        }
        txs.push(record);
    }

    let encoded: Vec<Bytes> = executed.receipts().map(|r| r.encoded.clone()).collect();
    let root = receipts_root(&encoded);
    let differing = txs.iter().filter(|tx| !tx.differs.is_empty()).count();
    let satin = (engine == Engine::Satin).then(|| sum_satin(txs.iter().filter_map(|t| t.satin)));
    let block = BlockRecord {
        kind: "block",
        block: number,
        hash: inputs.header.hash,
        engine: engine.name(),
        spec: spec.to_string(),
        transactions: txs.len(),
        refused: txs.iter().filter(|tx| tx.status == "refused").count(),
        differing,
        gas_used: previous,
        chain_gas_used: inputs.receipts.last().map_or(0, |r| r.cumulative_gas_used.to()),
        receipts_root: root,
        chain_receipts_root: inputs.header.receipts_root,
        matches_chain: differing == 0 && root == inputs.header.receipts_root,
        rpc_reads,
        satin,
    };
    (txs, block)
}

fn chain_view(chain: &ChainReceipt) -> ChainView {
    ChainView {
        status: if chain.success() { "success" } else { "failure" },
        gas_used: chain.gas_used.to(),
        cumulative_gas_used: chain.cumulative_gas_used.to(),
        logs: chain.logs.len(),
    }
}

/// The sum of `reports` over a block: every ledger and count added up; the reservoir left and
/// the floor are per transaction and summed as they are; no limit stop.
fn sum_satin(reports: impl Iterator<Item = SatinReport>) -> SatinReport {
    reports.fold(SatinReport::default(), |acc, r| SatinReport {
        regular_gas: acc.regular_gas + r.regular_gas,
        state_gas: acc.state_gas + r.state_gas,
        history_gas: acc.history_gas + r.history_gas,
        history_bytes: acc.history_bytes + r.history_bytes,
        reservoir_remaining: acc.reservoir_remaining + r.reservoir_remaining,
        floor_gas: acc.floor_gas + r.floor_gas,
        data_size: acc.data_size + r.data_size,
        write_records: acc.write_records + r.write_records,
        limit_exceeded: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{b256, bytes};

    /// No receipts give the empty trie's root; the root depends on the order of the receipts.
    #[test]
    fn test_receipts_root() {
        assert_eq!(
            receipts_root(&[]),
            b256!("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421")
        );
        let a = bytes!("01");
        let b = bytes!("02");
        assert_ne!(receipts_root(&[a.clone(), b.clone()]), receipts_root(&[b, a]));
    }
}
