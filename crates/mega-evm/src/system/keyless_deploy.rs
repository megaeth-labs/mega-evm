//! The keyless deploy system contract for the `MegaETH` EVM.
//!
//! This contract enables deploying contracts using pre-EIP-155 transactions (Nick's Method)
//! with custom gas limits, solving the problem of contracts failing to deploy on `MegaETH`
//! due to the different gas model.
//!
//! ## Nick's Method Overview
//!
//! Nick's Method allows deterministic contract deployment across different EVM chains without
//! needing the deployer's private key:
//!
//! 1. Create a contract deployment transaction (to = null, nonce = 0)
//! 2. Generate a random signature (v, r, s) with v = 27 or 28 (pre-EIP-155)
//! 3. Recover the signer address from the signature (nobody knows this private key)
//! 4. Fund the signer address with enough ETH for gas
//! 5. Broadcast the signed transaction
//!
//! The deployment address is deterministic: `keccak256(rlp([signer, 0]))[12:]`

use alloy_evm::Database;
use alloy_primitives::{address, Address};
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState},
};

use crate::MegaHardforks;

// Re-export error types and transaction functions from sandbox
pub use crate::sandbox::{
    calculate_keyless_deploy_address, decode_keyless_tx, encode_error_result, recover_signer,
    KeylessDeployError,
};

/// The address of the keyless deploy system contract.
pub const KEYLESS_DEPLOY_ADDRESS: Address = address!("0x6342000000000000000000000000000000000003");

/// The code of the keyless deploy contract (version 1.0.0).
pub use mega_system_contracts::keyless_deploy::V1_0_0_CODE as KEYLESS_DEPLOY_CODE;

/// The code hash of the keyless deploy contract (version 1.0.0).
pub use mega_system_contracts::keyless_deploy::V1_0_0_CODE_HASH as KEYLESS_DEPLOY_CODE_HASH;

pub use mega_system_contracts::keyless_deploy::IKeylessDeploy;

/// Ensures the keyless deploy contract is deployed in the designated address and returns the
/// state changes. Note that the database `db` is not modified in this function. The caller is
/// responsible to commit the changes to database.
pub fn transact_deploy_keyless_deploy_contract<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_rex_2_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // Load the keyless deploy contract account from the cache
    let acc = db.load_cache_account(KEYLESS_DEPLOY_ADDRESS)?;

    // If the contract is already deployed with the correct code, return early
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == KEYLESS_DEPLOY_CODE_HASH {
            // Although we do not need to update the account, we need to mark it as read
            return Ok(Some(EvmState::from_iter([(
                KEYLESS_DEPLOY_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Update the account info with the contract code
    let account_existed = acc.account_info().is_some();
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = KEYLESS_DEPLOY_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(KEYLESS_DEPLOY_CODE));

    // Convert the cache account back into a revm account and mark it as touched.
    // Only mark it as created when the account did not previously exist; an in-place
    // bytecode upgrade of an existing account must not clear its storage.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();
    if !account_existed {
        revm_acc.mark_created();
    }

    Ok(Some(EvmState::from_iter([(KEYLESS_DEPLOY_ADDRESS, revm_acc)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MegaHardfork, MegaHardforkConfig};
    use revm::{database::InMemoryDB, state::AccountInfo};

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

    #[test]
    fn test_deploy_keyless_deploy_contract_requires_rex2() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks =
            MegaHardforkConfig::default().with_all_activated().without(MegaHardfork::Rex2);

        let result = transact_deploy_keyless_deploy_contract(&hardforks, 0, &mut state)
            .expect("Should succeed");
        assert_eq!(result, None, "should not deploy before Rex2 activation");
    }
}
