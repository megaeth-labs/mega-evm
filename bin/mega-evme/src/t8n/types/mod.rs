mod transaction;
pub use transaction::*;

mod receipt;
pub use receipt::*;

use std::collections::HashMap;

use mega_evm::revm::primitives::{alloy_primitives::Bloom, Address, B256, U256};
use state_test::types::{AccountInfo, Env};

/// Input data for state transition
#[derive(Debug)]
pub struct TransitionInputs {
    /// Pre-state allocation of accounts
    pub alloc: StateAlloc,
    /// Block environment configuration
    pub env: Env,
    /// List of transactions to execute
    pub txs: Vec<Transaction>,
}

/// Results from state transition (internal)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionResults {
    /// Final state root hash
    pub state_root: B256,
    /// Transaction trie root hash
    pub tx_root: B256,
    /// Receipts trie root hash
    pub receipts_root: B256,
    /// Hash of all logs
    pub logs_hash: B256,
    /// Bloom filter of all logs
    pub logs_bloom: Bloom,
    /// Transaction receipts
    pub receipts: Vec<TransactionReceipt>,
    /// List of rejected transactions
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedTx>,
    /// Current block difficulty
    #[serde(rename = "currentDifficulty")]
    pub difficulty: U256,
    /// Total gas used in block
    #[serde(rename = "gasUsed", with = "alloy_serde::quantity")]
    pub gas_used: u64,
    /// Current base fee per gas
    #[serde(rename = "currentBaseFee")]
    pub base_fee: U256,
    /// Post-state allocation (not serialized here, moved to `T8nOutput`)
    #[serde(skip)]
    pub post_state_alloc: StateAlloc,
}

/// T8N tool output format expected by execution-spec-tests
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct T8nOutput {
    /// Post-state allocation
    pub alloc: StateAlloc,
    /// Transition results
    pub result: TransitionResults,
}

/// Information about a rejected transaction
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RejectedTx {
    /// Index of the rejected transaction
    #[serde(with = "alloy_serde::quantity")]
    pub index: u64,
    /// Error message explaining why the transaction was rejected
    pub error: String,
}

/// Prestate account allocation (address -> account info mapping)
pub type StateAlloc = HashMap<Address, AccountInfo>;

/// Combined stdin input format
#[derive(Debug, serde::Deserialize)]
pub struct StdinInput {
    /// Pre-state allocation of accounts
    pub alloc: StateAlloc,
    /// Block environment configuration
    pub env: Env,
    /// List of transactions to execute
    pub txs: Vec<Transaction>,
}
