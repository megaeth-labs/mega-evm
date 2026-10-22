//! Unit tests extracted from `crates/mega-evm/src/system/oracle.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/system/oracle.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use crate::MegaHardforkConfig;

    use super::*;
    use alloy_primitives::keccak256;
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    #[test]
    fn test_deploy_oracle_contract_on_fresh_db() {
        // Create a fresh in-memory database
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        // with_all_activated() includes Rex5, so we expect Rex5 (v2.0.0) bytecode
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        // Deploy the oracle contract
        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        // Verify that state changes were returned
        assert_eq!(result.len(), 1, "Should have state changes for one account");
        assert!(
            result.contains_key(&ORACLE_CONTRACT_ADDRESS),
            "State changes should contain oracle contract address"
        );

        // Verify the account in the state changes
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert!(account.is_touched(), "Account should be marked as touched");
        assert!(account.is_created(), "Account should be marked as created");

        // Verify the account info (Rex5 = v2.0.0 bytecode)
        let info = &account.info;
        assert_eq!(
            info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX5,
            "Code hash should match the v2.0.0 value"
        );
        assert!(info.code.is_some(), "Code should be set");

        let code = info.code.as_ref().unwrap();
        assert_eq!(
            code.original_bytes(),
            ORACLE_CONTRACT_CODE_REX5,
            "Code bytes should match the v2.0.0 value"
        );
    }

    #[test]
    fn test_deploy_oracle_contract_idempotent() {
        // Create a database with the oracle contract already deployed correctly (Rex5 = v2.0.0)
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            ORACLE_CONTRACT_ADDRESS,
            AccountInfo {
                balance: revm::primitives::U256::ZERO,
                nonce: 0,
                code_hash: ORACLE_CONTRACT_CODE_HASH_REX5,
                code: Some(Bytecode::new_raw(ORACLE_CONTRACT_CODE_REX5)),
            },
        );

        let mut state = State::builder().with_database(&mut db).build();
        // with_all_activated() includes Rex5
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        // Deploy should return state with the account marked as read (no update needed)
        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");
        assert_eq!(
            result.len(),
            1,
            "Deployment should return state with account marked as read when contract is already correctly deployed"
        );

        // Verify the account is in the result
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert_eq!(
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX5,
            "Code hash should match v2.0.0 bytecode"
        );
    }

    #[test]
    fn test_deploy_oracle_contract_with_wrong_code_hash() {
        // Create a database with the oracle address already having different code
        let mut db = InMemoryDB::default();

        // Insert an account with wrong code hash at the oracle address
        let wrong_code = bytes!("0x6000");
        let wrong_code_hash = keccak256(&wrong_code);

        db.insert_account_info(
            ORACLE_CONTRACT_ADDRESS,
            AccountInfo {
                balance: revm::primitives::U256::ZERO,
                nonce: 0,
                code_hash: wrong_code_hash,
                code: Some(Bytecode::new_raw(wrong_code)),
            },
        );

        let mut state = State::builder().with_database(&mut db).build();
        // with_all_activated() includes Rex2
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        // Deploy should update the contract with correct code (Rex5 = v2.0.0 bytecode)
        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        // Verify that state changes were returned (contract was updated)
        assert_eq!(result.len(), 1, "Should have state changes to update the contract");

        // Verify the updated account has the correct code hash
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert_eq!(
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX5,
            "Code hash should be updated to v2.0.0 bytecode"
        );
    }

    #[test]
    fn test_deploy_oracle_contract_marks_account_as_touched() {
        // Create a fresh in-memory database
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        // Deploy the oracle contract
        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        // Get the account from result
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist in result");

        // Verify the account is marked as touched (required for state changes to be committed)
        assert!(
            account.is_touched(),
            "Deployed account must be marked as touched for state changes to take effect"
        );
        assert!(account.is_created(), "Account should be marked as created");
    }

    #[test]
    fn test_deploy_oracle_contract_rex5() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        // Activate all including Rex5
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert_eq!(
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX5,
            "Should deploy v2.0.0 bytecode on Rex5"
        );
    }

    #[test]
    fn test_high_precision_timestamp_oracle_deployment() {
        // Create a fresh in-memory database
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        // Deploy the high precision timestamp oracle contract
        let result = transact_deploy_high_precision_timestamp_oracle(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        // Verify that state changes were returned
        assert_eq!(result.len(), 1, "Should have state changes for one account");
        assert!(
            result.contains_key(&HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS),
            "State changes should contain high precision timestamp oracle address"
        );

        // Verify the account in the state changes
        let account =
            result.get(&HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS).expect("Account should exist");
        assert!(account.is_touched(), "Account should be marked as touched");
        assert!(account.is_created(), "Account should be marked as created");

        // Verify the account info
        let info = &account.info;
        assert_eq!(
            info.code_hash, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH,
            "Code hash should match the expected value"
        );
        assert!(info.code.is_some(), "Code should be set");

        // Verify code matches
        let code = info.code.as_ref().unwrap();
        assert_eq!(
            code.original_bytes(),
            HIGH_PRECISION_TIMESTAMP_ORACLE_CODE,
            "Code bytes should match the expected value"
        );

        // Verify code hash matches computed hash
        let computed_hash = keccak256(&HIGH_PRECISION_TIMESTAMP_ORACLE_CODE);
        assert_eq!(
            computed_hash, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH,
            "Code hash constant should match computed hash"
        );
    }

    /// Covers the `account_existed = true` branch in
    /// `transact_deploy_high_precision_timestamp_oracle`: when the account already exists with
    /// wrong code, the deploy updates the code without marking the account as created.
    #[test]
    fn test_high_precision_timestamp_oracle_existing_account_not_marked_created() {
        let wrong_code = alloy_primitives::bytes!("0x6000");
        let wrong_code_hash = alloy_primitives::keccak256(&wrong_code);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
            AccountInfo {
                balance: revm::primitives::U256::ZERO,
                nonce: 0,
                code_hash: wrong_code_hash,
                code: Some(Bytecode::new_raw(wrong_code)),
            },
        );

        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result = transact_deploy_high_precision_timestamp_oracle(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        let account =
            result.get(&HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS).expect("Account should exist");
        assert_eq!(account.info.code_hash, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH);
        assert!(account.is_touched());
        assert!(
            !account.is_created(),
            "existing account must not be marked as created on code update"
        );
    }
}
