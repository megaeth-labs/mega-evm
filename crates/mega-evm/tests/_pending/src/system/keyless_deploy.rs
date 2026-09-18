//! Unit tests extracted from `crates/mega-evm/src/system/keyless_deploy.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/system/keyless_deploy.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MegaHardforkConfig;
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    #[test]
    fn test_deploy_keyless_deploy_contract_on_fresh_db() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result = transact_deploy_keyless_deploy_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        let account = result.get(&KEYLESS_DEPLOY_ADDRESS).expect("Account should exist");
        assert!(account.is_touched());
        assert!(account.is_created(), "fresh deploy must mark account as created");
        assert_eq!(account.info.code_hash, KEYLESS_DEPLOY_CODE_HASH);
    }

    /// Covers the `account_existed = true` branch: an account already exists with wrong code,
    /// so the deploy updates the code without marking the account as created.
    #[test]
    fn test_deploy_keyless_deploy_contract_existing_account_not_marked_created() {
        let wrong_code = alloy_primitives::bytes!("0x6000");
        let wrong_code_hash = alloy_primitives::keccak256(&wrong_code);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            KEYLESS_DEPLOY_ADDRESS,
            AccountInfo {
                balance: revm::primitives::U256::ZERO,
                nonce: 0,
                code_hash: wrong_code_hash,
                code: Some(Bytecode::new_raw(wrong_code)),
            },
        );

        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result = transact_deploy_keyless_deploy_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        let account = result.get(&KEYLESS_DEPLOY_ADDRESS).expect("Account should exist");
        assert_eq!(account.info.code_hash, KEYLESS_DEPLOY_CODE_HASH);
        assert!(account.is_touched());
        assert!(
            !account.is_created(),
            "in-place bytecode update of an existing account must not mark it as created"
        );
    }

}
