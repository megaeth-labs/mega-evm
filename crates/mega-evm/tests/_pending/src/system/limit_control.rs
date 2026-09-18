//! Unit tests extracted from `crates/mega-evm/src/system/limit_control.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/system/limit_control.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{keccak256, B256};
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    use crate::{MegaHardfork, MegaHardforkConfig};
    use alloy_hardforks::ForkCondition;

    #[test]
    fn test_limit_control_contract_code_hash_matches() {
        let computed_hash = keccak256(&LIMIT_CONTROL_CODE);
        assert_eq!(computed_hash, LIMIT_CONTROL_CODE_HASH);
    }

    #[test]
    fn test_deploy_limit_control_contract_on_fresh_db() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result =
            transact_deploy_limit_control_contract(&hardforks, 0, &mut state).unwrap().unwrap();
        let account = result.get(&LIMIT_CONTROL_ADDRESS).unwrap();

        assert!(account.is_touched());
        assert!(account.is_created());
        assert_eq!(account.info.code_hash, LIMIT_CONTROL_CODE_HASH);
        assert_eq!(account.info.code.as_ref().unwrap().original_bytes(), LIMIT_CONTROL_CODE);
    }

    #[test]
    fn test_deploy_limit_control_contract_is_idempotent() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            LIMIT_CONTROL_ADDRESS,
            AccountInfo {
                balance: Default::default(),
                nonce: 0,
                code_hash: LIMIT_CONTROL_CODE_HASH,
                code: Some(Bytecode::new_raw(LIMIT_CONTROL_CODE)),
            },
        );
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result =
            transact_deploy_limit_control_contract(&hardforks, 0, &mut state).unwrap().unwrap();
        let account = result.get(&LIMIT_CONTROL_ADDRESS).unwrap();

        assert_eq!(account.info.code_hash, LIMIT_CONTROL_CODE_HASH);
        assert_eq!(account.info.code.as_ref().unwrap().original_bytes(), LIMIT_CONTROL_CODE);
        assert!(!account.is_created(), "idempotent deploy should not re-create the account");
    }

    #[test]
    fn test_deploy_limit_control_contract_at_later_timestamp() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks =
            MegaHardforkConfig::default().with(MegaHardfork::Rex4, ForkCondition::Timestamp(50));

        assert_eq!(
            transact_deploy_limit_control_contract(&hardforks, 49, &mut state).unwrap(),
            None,
            "should not deploy before Rex4 activation"
        );

        let result =
            transact_deploy_limit_control_contract(&hardforks, 50, &mut state).unwrap().unwrap();
        let account = result.get(&LIMIT_CONTROL_ADDRESS).unwrap();
        assert!(account.is_created());
        assert_eq!(account.info.code_hash, LIMIT_CONTROL_CODE_HASH);
    }

    #[test]
    fn test_deploy_limit_control_contract_overwrites_wrong_existing_code_hash() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            LIMIT_CONTROL_ADDRESS,
            AccountInfo {
                balance: Default::default(),
                nonce: 0,
                code_hash: B256::ZERO,
                code: Some(Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x60, 0x00]))),
            },
        );
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result =
            transact_deploy_limit_control_contract(&hardforks, 0, &mut state).unwrap().unwrap();
        let account = result.get(&LIMIT_CONTROL_ADDRESS).unwrap();

        assert_eq!(account.info.code_hash, LIMIT_CONTROL_CODE_HASH);
        assert_eq!(account.info.code.as_ref().unwrap().original_bytes(), LIMIT_CONTROL_CODE);
        assert!(account.is_touched());
        assert!(
            !account.is_created(),
            "in-place bytecode update of an existing account must not mark it as created"
        );
    }
}
