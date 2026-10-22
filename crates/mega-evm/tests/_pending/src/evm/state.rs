//! Unit tests extracted from `crates/mega-evm/src/evm/state.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/state.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, B256, U256};
    use revm::{
        database::{InMemoryDB, State},
        state::{AccountInfo, EvmStorageSlot},
    };

    const TEST_ADDRESS: Address = address!("1000000000000000000000000000000000000001");

    fn account_with_status(status: AccountStatus) -> Account {
        Account {
            info: AccountInfo {
                balance: U256::from(1),
                nonce: 1,
                code_hash: B256::ZERO,
                code: None,
            },
            transaction_id: 0,
            storage: Default::default(),
            status,
        }
    }

    #[test]
    fn test_state_exposes_accessed_block_hashes() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        state.block_hashes.insert(1, B256::ZERO);
        state.block_hashes.insert(2, B256::from([2_u8; 32]));

        let hashes = state.get_accessed_block_hashes();
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes.get(&1), Some(&B256::ZERO));
        assert_eq!(hashes.get(&2), Some(&B256::from([2_u8; 32])));
    }

}
