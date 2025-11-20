use std::collections::HashMap;

use mega_evm::revm::{
    database::{EmptyDB, State},
    primitives::{alloy_primitives::Bloom, Address, Log, B256},
};
use state_test::types::AccountInfo;

use crate::t8n::{Result, StateAlloc, T8nError};

/// Calculate state root from the final state
pub fn calculate_state_root(state: &State<EmptyDB>) -> B256 {
    state_test::utils::state_merkle_trie_root(state.cache.trie_account())
}

/// Calculate logs root from all transaction logs
pub fn calculate_logs_root(logs: &[Log]) -> B256 {
    state_test::utils::log_rlp_hash(logs)
}

/// Calculate bloom filter from all transaction logs
pub fn calculate_logs_bloom(logs: &[Log]) -> Bloom {
    let mut bloom = Bloom::default();
    for log in logs {
        bloom.accrue_log(log);
    }
    bloom
}

/// Extract post-state allocation from EVM state
pub fn extract_post_state_alloc_from_state(state: &State<EmptyDB>) -> StateAlloc {
    let mut post_alloc = HashMap::new();

    // Extract all accounts from the state
    for (address, account) in state.cache.trie_account() {
        let account_info = AccountInfo {
            nonce: account.info.nonce,
            balance: account.info.balance,
            code: account
                .info
                .code
                .as_ref()
                .map(|bytecode| bytecode.original_bytes())
                .unwrap_or_default(),
            storage: account
                .storage
                .iter()
                .map(|(k, v)| (*k, *v)) // v is already a U256 value
                .collect(),
        };
        post_alloc.insert(Address::from(*address), account_info);
    }

    post_alloc
}

/// Recover address from secret key
pub fn recover_address_from_secret_key(secret_key: &B256) -> Result<Address> {
    // Use the same recovery function as in state_test::utils
    state_test::utils::recover_address(secret_key.as_slice()).ok_or_else(|| {
        T8nError::InvalidTransaction(format!(
            "Failed to recover address from secret key: {:?}",
            secret_key
        ))
    })
}
