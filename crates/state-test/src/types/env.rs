use std::collections::BTreeMap;

use alloy_eips::eip4895::Withdrawal;
use revm::primitives::{Address, B256, U256};
use serde::Deserialize;

/// Environment variables
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Env {
    /// Chain ID for the current execution
    #[serde(rename = "currentChainID")]
    pub current_chain_id: Option<U256>,
    /// Block coinbase address (miner/validator)
    pub current_coinbase: Address,
    /// Block difficulty (pre-merge) or prevrandao (post-merge)
    #[serde(default)]
    pub current_difficulty: U256,
    /// Block gas limit
    pub current_gas_limit: U256,
    /// Current block number
    pub current_number: U256,
    /// Current block timestamp
    pub current_timestamp: U256,
    /// EIP-1559 base fee per gas
    pub current_base_fee: Option<U256>,
    /// Previous block hash
    pub previous_hash: Option<B256>,
    /// Parent block timestamp
    pub parent_timestamp: Option<U256>,
    /// Parent block gas used
    pub parent_gas_used: Option<U256>,
    /// Parent block gas limit
    pub parent_gas_limit: Option<U256>,
    /// Parent block base fee
    pub parent_base_fee: Option<U256>,
    /// Parent block hash
    pub parent_hash: Option<B256>,
    /// Parent block uncle hash
    pub parent_uncle_hash: Option<B256>,
    /// Parent block beacon block root
    pub parent_beacon_block_root: Option<B256>,
    /// Parent block difficulty
    pub parent_difficulty: Option<U256>,

    /// Block hashes
    pub block_hashes: Option<BTreeMap<U256, B256>>,
    /// Ommers
    pub ommers: Option<Vec<B256>>,
    /// Withdrawals
    pub withdrawals: Option<Vec<Withdrawal>>,

    /// Current block randomness (EIP-4399 prevrandao)
    pub current_random: Option<U256>,
    /// Current beacon chain root (EIP-4788)
    pub current_beacon_root: Option<B256>,
    /// Current withdrawals root
    pub current_withdrawals_root: Option<B256>,

    /// Parent block blob gas used (EIP-4844)
    pub parent_blob_gas_used: Option<U256>,
    /// Parent block excess blob gas (EIP-4844)
    pub parent_excess_blob_gas: Option<U256>,
    /// Parent block target blobs per block (EIP-4844)
    pub parent_target_blobs_per_block: Option<U256>,
    /// Current block excess blob gas (EIP-4844)
    pub current_excess_blob_gas: Option<U256>,
    /// Current block blob gas used (EIP-4844)
    pub current_blob_gas_used: Option<U256>,
}
