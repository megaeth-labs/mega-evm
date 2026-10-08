//! What either engine hands back for a block: plain data, the same shape from both.
//!
//! An executor takes the block's header fields, its transactions as EIP-2718 envelopes and a
//! [`BlockState`](super::state::BlockState), and returns every transaction's receipt as its
//! EIP-2718 encoding and the fields a comparison reads. No type of either revm line crosses.

use alloy_primitives::{Bytes, Log};

use mega_evm::ProtocolLimits;

use crate::common::SatinReport;

/// How an included transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecStatus {
    /// It succeeded.
    Success,
    /// It reverted.
    Revert,
    /// It halted.
    Halt,
}

impl ExecStatus {
    /// The status's name in output.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Revert => "revert",
            Self::Halt => "halt",
        }
    }
}

/// A receipt as the engine built it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptData {
    /// The receipt's EIP-2718 encoding, which the receipts root is built from.
    pub encoded: Bytes,
    /// Whether the transaction succeeded.
    pub success: bool,
    /// The block's gas used after the transaction.
    pub cumulative_gas_used: u64,
    /// The logs.
    pub logs: Vec<Log>,
}

/// What happened to one transaction of the block.
#[derive(Debug, Clone)]
pub enum TxResult {
    /// The transaction ran and its receipt is in the block.
    Included {
        /// How it ended.
        status: ExecStatus,
        /// The halt reason or the decoded revert, when it did not succeed.
        reason: Option<String>,
        /// Its receipt.
        receipt: ReceiptData,
        /// What Satin counted, on the Satin engine.
        satin: Option<SatinReport>,
    },
    /// The engine refused the transaction; it is not in the block and changed nothing.
    Refused {
        /// Why.
        reason: String,
    },
}

/// What executing a block produced.
#[derive(Debug, Clone)]
pub struct ExecutedBlock {
    /// One result per transaction, in block order.
    pub transactions: Vec<TxResult>,
    /// The protocol limits a Satin block ran under when `--override.limits` replaced the
    /// schedule's; `None` otherwise, and on the legacy engine.
    pub limits_override: Option<ProtocolLimits>,
}

impl ExecutedBlock {
    /// The receipts of the included transactions, in order.
    pub fn receipts(&self) -> impl Iterator<Item = &ReceiptData> {
        self.transactions.iter().filter_map(|tx| match tx {
            TxResult::Included { receipt, .. } => Some(receipt),
            TxResult::Refused { .. } => None,
        })
    }
}
