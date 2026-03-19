//! The `MegaLimitControl` system contract for the `MegaETH` EVM.
//!
//! This contract currently provides a read-only query to return remaining compute gas
//! of the current call.
//! The runtime result is produced by EVM interception, not by executing on-chain bytecode.

use alloy_evm::Database;
use alloy_primitives::{address, Address};
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState},
};

use crate::MegaHardforks;

/// The address of the `MegaLimitControl` system contract.
pub const LIMIT_CONTROL_ADDRESS: Address = address!("0x6342000000000000000000000000000000000005");

/// The code of the `MegaLimitControl` contract (version 1.0.0).
pub use mega_system_contracts::limit_control::V1_0_0_CODE as LIMIT_CONTROL_CODE;

/// The code hash of the `MegaLimitControl` contract (version 1.0.0).
pub use mega_system_contracts::limit_control::V1_0_0_CODE_HASH as LIMIT_CONTROL_CODE_HASH;

pub use mega_system_contracts::limit_control::IMegaLimitControl;

/// Ensures the `MegaLimitControl` contract is deployed in the designated address and returns
/// the state changes.
/// The caller is responsible for committing the returned `EvmState` changes to the database.
pub fn transact_deploy_limit_control_contract<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_rex_4_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // Load the MegaLimitControl contract account from the cache.
    let acc = db.load_cache_account(LIMIT_CONTROL_ADDRESS)?;

    // If already deployed with the same code hash, return early and mark as read.
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == LIMIT_CONTROL_CODE_HASH {
            return Ok(Some(EvmState::from_iter([(
                LIMIT_CONTROL_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Update account with target bytecode.
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = LIMIT_CONTROL_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(LIMIT_CONTROL_CODE));

    // Convert back and mark as touched/created.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    Ok(Some(EvmState::from_iter([(LIMIT_CONTROL_ADDRESS, revm_acc)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{keccak256, B256};
    use revm::{database::InMemoryDB, state::AccountInfo};

    use crate::MegaHardforkConfig;
    use alloy_hardforks::ForkCondition;

    use crate::MegaHardfork;

    #[test]
    fn test_limit_control_contract_code_hash_matches() {
        let computed_hash = keccak256(&LIMIT_CONTROL_CODE);
        assert_eq!(computed_hash, LIMIT_CONTROL_CODE_HASH);
    }

    #[test]
    fn test_deploy_limit_control_contract_requires_rex4() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let result =
            transact_deploy_limit_control_contract(MegaHardforkConfig::default(), 0, &mut state)
                .unwrap();
        assert_eq!(result, None);
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
        assert!(account.is_created());
        assert!(account.is_touched());
    }
}
