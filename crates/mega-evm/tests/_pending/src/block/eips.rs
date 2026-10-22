//! Unit tests extracted from `crates/mega-evm/src/block/eips.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/block/eips.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use revm::{database::InMemoryDB, state::AccountInfo, DatabaseCommit};

    #[test]
    fn test_balance_increment_commit_equivalence() {
        // This is the most important test: verify that our approach is equivalent
        // to the original increment_balances + balance_increment_state approach

        // Setup: create two identical database states
        let addr1 = address!("0x1000000000000000000000000000000000000001");
        let addr2 = address!("0x2000000000000000000000000000000000000002");
        let addr3 = address!("0x3000000000000000000000000000000000000003");

        // Fund only addr1 and addr2, but leave addr3 empty (non-existent)
        let setup_db = |db: &mut InMemoryDB| {
            for (addr, balance, nonce) in [(addr1, 1000u64, 5u64), (addr2, 2000u64, 10u64)] {
                db.insert_account_info(
                    addr,
                    AccountInfo {
                        balance: U256::from(balance),
                        nonce,
                        code_hash: alloy_primitives::B256::ZERO,
                        code: None,
                    },
                );
            }
        };

        // Setup database 1: will use original approach (increment_balances)
        let mut db1 = InMemoryDB::default();
        setup_db(&mut db1);
        let mut state1 = State::builder().with_database(&mut db1).build();

        // Setup database 2: will use our approach (transact_balance_increments + commit)
        let mut db2 = InMemoryDB::default();
        setup_db(&mut db2);
        let mut state2 = State::builder().with_database(&mut db2).build();

        // Apply balance increments
        let balance_increments = vec![(addr1, 100u128), (addr2, 200u128), (addr3, 300u128)];

        // Method 1: Original approach
        state1
            .increment_balances(balance_increments.clone())
            .expect("increment_balances should succeed");

        // Method 2: Refactored approach (transact + commit, matching the actual usage pattern)
        // The refactoring separates increment_balances into two steps:
        // 1. transact_balance_increments() - produces EvmState delta
        // 2. commit() - integrates the delta and fixes status transitions
        // This allows extracting the intermediate state for system_caller.on_state() hooks
        let result_state = transact_balance_increments(balance_increments.clone(), &mut state2)
            .expect("transact_balance_increments should succeed")
            .expect("Should return state");

        // Commit the state changes (this applies proper status transitions via apply_evm_state)
        state2.commit(result_state);

        // Verify final states match after both approaches
        for (addr, _expected_increment) in balance_increments {
            let account1 = state1.load_cache_account(addr).expect("Should load from state1");
            let account2 = state2.load_cache_account(addr).expect("Should load from state2");

            let info1 = account1.account_info().expect("Should have account info");
            let info2 = account2.account_info().expect("Should have account info");

            assert_eq!(
                info1.balance, info2.balance,
                "Balance for {:?} should be identical after both methods",
                addr
            );
            assert_eq!(info1.nonce, info2.nonce, "Nonce for {:?} should be identical", addr);
            assert_eq!(
                info1.code_hash, info2.code_hash,
                "Code hash for {:?} should be identical",
                addr
            );
            assert_eq!(
                account1.status, account2.status,
                "Account status for {:?} should be identical after both methods",
                addr
            );
        }
    }
}
