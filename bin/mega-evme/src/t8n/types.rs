use std::collections::HashMap;

use revm::{
    context_interface::transaction::{AccessList, SignedAuthorization},
    primitives::{alloy_primitives::Bloom, Address, Bytes, B256, U256},
};
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

/// Transaction data for t8n (individual signed transaction)
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, 3=EIP-4844, 4=EIP-7702)
    #[serde(rename = "type", default, with = "alloy_serde::quantity::opt")]
    pub tx_type: Option<u8>,
    /// Chain ID
    pub chain_id: Option<U256>,
    /// Transaction nonce
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// Gas price (legacy/EIP-2930)
    #[serde(with = "alloy_serde::quantity::opt")]
    pub gas_price: Option<u64>,
    /// Maximum fee per gas (EIP-1559)
    #[serde(rename = "gasFeeCap", with = "alloy_serde::quantity::opt", default)]
    pub max_fee_per_gas: Option<u64>,
    /// Maximum priority fee per gas (EIP-1559)
    #[serde(rename = "gasTipCap", with = "alloy_serde::quantity::opt", default)]
    pub max_priority_fee_per_gas: Option<u64>,
    /// Gas limit
    #[serde(default, with = "alloy_serde::quantity")]
    pub gas: u64,
    /// Recipient address (None for contract creation)
    pub to: Option<Address>,
    /// Ether value to transfer
    pub value: U256,
    /// Transaction data/input
    #[serde(default, alias = "input")]
    pub data: Bytes,
    /// Access list (EIP-2930, EIP-1559)
    pub access_list: Option<AccessList>,
    /// Authorization list (EIP-7702)
    pub authorization_list: Option<Vec<SignedAuthorization>>,
    /// Maximum fee per blob gas (EIP-4844)
    pub max_fee_per_blob_gas: Option<U256>,
    /// Blob versioned hashes (EIP-4844)
    #[serde(default)]
    pub blob_versioned_hashes: Vec<B256>,
    /// Signature v component
    pub v: U256,
    /// Signature r component
    pub r: U256,
    /// Signature s component
    pub s: U256,
    /// Secret key (for unsigned transactions)
    pub secret_key: Option<B256>,
}

/// Transaction log entry
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionLog {
    /// Address that generated this log
    pub address: Address,
    /// Indexed topics of the log
    pub topics: Vec<B256>,
    /// Log data
    pub data: Bytes,
    /// Block number where this log was generated
    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,
    /// Hash of the transaction that generated this log
    pub transaction_hash: B256,
    /// Index of the transaction in the block
    #[serde(with = "alloy_serde::quantity")]
    pub transaction_index: u64,
    /// Hash of the block containing this log
    pub block_hash: B256,
    /// Index of this log within the transaction
    #[serde(with = "alloy_serde::quantity")]
    pub log_index: u64,
    /// Whether this log was removed due to a chain reorganization
    pub removed: bool,
}

/// Receipt delegation entry for EIP-7702 set-code transactions
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptDelegation {
    /// Address that delegated code execution
    #[serde(rename = "from")]
    pub from_address: Address,
    /// Nonce used for the delegation
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// Target address that provides the code
    pub target: Address,
}

/// Transaction receipt containing execution results
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionReceipt {
    /// Hash of the transaction
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_hash: Option<B256>,
    /// Gas used by this transaction
    #[serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub gas_used: Option<u64>,
    /// State root after this transaction (pre-Byzantium)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<B256>,
    /// Status of transaction execution (post-Byzantium: 1=success, 0=failure)
    #[serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub status: Option<u64>,
    /// Cumulative gas used in the block up to and including this transaction
    #[serde(with = "alloy_serde::quantity")]
    pub cumulative_gas_used: u64,
    /// Bloom filter for logs in this transaction
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs_bloom: Option<Bloom>,
    /// Array of log entries generated by this transaction
    pub logs: Vec<TransactionLog>,
    /// Address of created contract (for contract creation transactions)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_address: Option<Address>,
    /// Effective gas price paid for this transaction
    #[serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub effective_gas_price: Option<u64>,
    /// Hash of the block containing this transaction
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<B256>,
    /// Index of this transaction within the block
    #[serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub transaction_index: Option<u64>,
    /// Gas used by blobs in this transaction (EIP-4844)
    #[serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub blob_gas_used: Option<u64>,
    /// Price per unit of blob gas (EIP-4844)
    #[serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub blob_gas_price: Option<u64>,
    /// List of code delegations in this transaction (EIP-7702)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegations: Option<Vec<ReceiptDelegation>>,
}
