//! The oracle system contract for the `MegaETH` EVM. The oracle contract is implemented in
//! `../../../system-contracts/contracts/Oracle.sol`.

use alloy_evm::Database;
use alloy_primitives::{address, b256, bytes, Address, Bytes, B256};
use alloy_sol_types::sol;
pub use alloy_sol_types::SolCall;
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState},
};

use crate::MegaHardforks;

/// The address of the oracle system contract.
pub const ORACLE_CONTRACT_ADDRESS: Address = address!("0x6342000000000000000000000000000000000001");

/// The code of the oracle contract (version 1.0.0, pre-Rex2).
pub use mega_system_contracts::oracle::V1_0_0_CODE as ORACLE_CONTRACT_CODE;

/// The code hash of the oracle contract (version 1.0.0, pre-Rex2).
pub use mega_system_contracts::oracle::V1_0_0_CODE_HASH as ORACLE_CONTRACT_CODE_HASH;

/// The code of the oracle contract (version 1.1.0, Rex2+).
/// This version includes the `sendHint` function for the oracle hint mechanism.
pub use mega_system_contracts::oracle::V1_1_0_CODE as ORACLE_CONTRACT_CODE_REX2;

/// The code hash of the oracle contract (version 1.1.0, Rex2+).
pub use mega_system_contracts::oracle::V1_1_0_CODE_HASH as ORACLE_CONTRACT_CODE_HASH_REX2;

sol! {
    /// The Solidity interface for the oracle contract.
    interface Oracle {
        function multiCall(bytes[] calldata data) external returns (bytes[] memory results);
        function getSlot(uint256 slot) external view returns (bytes32 value);
        function setSlot(uint256 slot, bytes32 value) external;
        function getSlots(uint256[] calldata slots) external view returns (bytes32[] memory values);
        function setSlots(uint256[] calldata slots, bytes32[] calldata values) external;
        function sendHint(bytes32 topic, bytes calldata data) external view;
        function emitLog(bytes32 topic, bytes calldata data) external;
        function emitLogs(bytes32 topic, bytes[] calldata dataVector) external;
    }
}

/// Ensures the oracle contract is deployed in the designated address and returns the state changes.
/// Note that the database `db` is not modified in this function. The caller is responsible to
/// commit the changes to database.
///
/// The deployed bytecode depends on the active hardfork:
/// - Pre-Rex2: v1.0.0 bytecode (without `sendHint` function)
/// - Rex2+: v1.1.0 bytecode (with `sendHint` function for oracle hints)
pub fn transact_deploy_oracle_contract<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_mini_rex_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // Select the appropriate bytecode based on hardfork
    let (target_code, target_code_hash) = if hardforks.is_rex_2_active_at_timestamp(block_timestamp)
    {
        (ORACLE_CONTRACT_CODE_REX2, ORACLE_CONTRACT_CODE_HASH_REX2)
    } else {
        (ORACLE_CONTRACT_CODE, ORACLE_CONTRACT_CODE_HASH)
    };

    // Load the oracle contract account from the cache
    let acc = db.load_cache_account(ORACLE_CONTRACT_ADDRESS)?;

    // If the contract is already deployed with the correct code, return early
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == target_code_hash {
            // Although we do not need to update the account, we need to mark it as read
            return Ok(Some(EvmState::from_iter([(
                ORACLE_CONTRACT_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Update the account info with the contract code
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = target_code_hash;
    acc_info.code = Some(Bytecode::new_raw(target_code));

    // Convert the cache account back into a revm account and mark it as touched.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    Ok(Some(EvmState::from_iter([(ORACLE_CONTRACT_ADDRESS, revm_acc)])))
}

/// The address of the high precision timestamp oracle contract.
pub const HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS: Address =
    address!("0x6342000000000000000000000000000000000002");

/// The code hash of the high precision timestamp oracle contract.
pub const HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH: B256 =
    b256!("0x1b2df8ca5350cd7106d67ed95532584526f97a5ca267c8eef73a1831f53720f2");

/// The code of the high precision timestamp oracle contract. The oracle contract address is
/// embedded in to the contract code.
pub const HIGH_PRECISION_TIMESTAMP_ORACLE_CODE: Bytes = bytes!("0x608060405234801561000f575f5ffd5b5060043610610064575f3560e01c806382ab890a1161004d57806382ab890a146100f55780638582b7bc1461010a578063b80777ea1461013f575f5ffd5b806328af7f3c146100685780633e6890fb146100a9575b5f5ffd5b61008f7f000000000000000000000000000000000000000000000000000000000000000881565b60405163ffffffff90911681526020015b60405180910390f35b6100d07f000000000000000000000000634200000000000000000000000000000000000181565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020016100a0565b6101086101033660046103b6565b610147565b005b6101317f000000000000000000000000000000000000000000000000000000000000000081565b6040519081526020016100a0565b610131610154565b6101515f82610165565b50565b5f5f61015f5f610245565b92915050565b8161016f8161031a565b73ffffffffffffffffffffffffffffffffffffffff7f00000000000000000000000063420000000000000000000000000000000000011663fbc0d0356101d5857f00000000000000000000000000000000000000000000000000000000000000006103cd565b60405160e083901b7fffffffff000000000000000000000000000000000000000000000000000000001681526004810191909152602481018590526044015f604051808303815f87803b15801561022a575f5ffd5b505af115801561023c573d5f5f3e3d5ffd5b50505050505050565b5f816102508161031a565b73ffffffffffffffffffffffffffffffffffffffff7f000000000000000000000000634200000000000000000000000000000000000116637eba7ba66102b6857f00000000000000000000000000000000000000000000000000000000000000006103cd565b6040518263ffffffff1660e01b81526004016102d491815260200190565b602060405180830381865afa1580156102ef573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906103139190610405565b9392505050565b63ffffffff7f00000000000000000000000000000000000000000000000000000000000000081661034c8260016103cd565b10610151576040517fef94d8420000000000000000000000000000000000000000000000000000000081526004810182905263ffffffff7f000000000000000000000000000000000000000000000000000000000000000816602482015260440160405180910390fd5b5f602082840312156103c6575f5ffd5b5035919050565b8082018082111561015f577f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f60208284031215610415575f5ffd5b505191905056fea2646970667358221220290614c973d24b9016b73a6b3fa98b2485a5c324199ceeb767818495dbe785e864736f6c634300081e0033");

/// Ensures the high precision timestamp oracle contract is deployed in the designated address and
/// returns the state changes. Note that the database `db` is not modified in this function. The
/// caller is responsible to commit the changes to database.
pub fn transact_deploy_high_precision_timestamp_oracle<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_mini_rex_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // Load the high precision timestamp oracle contract account from the cache
    let acc = db.load_cache_account(HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS)?;

    // If the contract is already deployed, return early
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH {
            // Although we do not need to update the account, we need to mark it as read
            return Ok(Some(EvmState::from_iter([(
                HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Update the account info with the contract code
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(HIGH_PRECISION_TIMESTAMP_ORACLE_CODE));

    // Convert the cache account back into a revm account and mark it as touched.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    Ok(Some(EvmState::from_iter([(HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, revm_acc)])))
}

#[cfg(test)]
mod tests {
    use crate::{MegaHardfork, MegaHardforkConfig};

    use super::*;
    use alloy_primitives::keccak256;
    use revm::{database::InMemoryDB, state::AccountInfo};

    #[test]
    fn test_oracle_contract_code_hash_matches() {
        // Compute the keccak256 hash of the oracle contract code (v1.0.0, pre-Rex2)
        let computed_hash = keccak256(&ORACLE_CONTRACT_CODE);

        // Verify it matches the hardcoded constant
        assert_eq!(
            computed_hash, ORACLE_CONTRACT_CODE_HASH,
            "Oracle contract code hash mismatch: computed {}, expected {}",
            computed_hash, ORACLE_CONTRACT_CODE_HASH
        );
    }

    #[test]
    fn test_oracle_contract_code_hash_rex2_matches() {
        // Compute the keccak256 hash of the oracle contract code (v1.1.0, Rex2+)
        let computed_hash = keccak256(&ORACLE_CONTRACT_CODE_REX2);

        // Verify it matches the hardcoded constant
        assert_eq!(
            computed_hash, ORACLE_CONTRACT_CODE_HASH_REX2,
            "Oracle contract Rex2 code hash mismatch: computed {}, expected {}",
            computed_hash, ORACLE_CONTRACT_CODE_HASH_REX2
        );
    }

    #[test]
    fn test_deploy_oracle_contract_on_fresh_db() {
        // Create a fresh in-memory database
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        // with_all_activated() includes Rex2, so we expect Rex2 bytecode
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

        // Verify the account info (Rex2 bytecode)
        let info = &account.info;
        assert_eq!(
            info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX2,
            "Code hash should match the expected value"
        );
        assert!(info.code.is_some(), "Code should be set");

        let code = info.code.as_ref().unwrap();
        assert_eq!(
            code.original_bytes(),
            ORACLE_CONTRACT_CODE_REX2,
            "Code bytes should match the expected value"
        );
    }

    #[test]
    fn test_deploy_oracle_contract_idempotent() {
        // Create a database with the oracle contract already deployed correctly (Rex2 bytecode)
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            ORACLE_CONTRACT_ADDRESS,
            AccountInfo {
                balance: revm::primitives::U256::ZERO,
                nonce: 0,
                code_hash: ORACLE_CONTRACT_CODE_HASH_REX2,
                code: Some(Bytecode::new_raw(ORACLE_CONTRACT_CODE_REX2)),
            },
        );

        let mut state = State::builder().with_database(&mut db).build();
        // with_all_activated() includes Rex2
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
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX2,
            "Code hash should match Rex2 bytecode"
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

        // Deploy should update the contract with correct code (Rex2 bytecode)
        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        // Verify that state changes were returned (contract was updated)
        assert_eq!(result.len(), 1, "Should have state changes to update the contract");

        // Verify the updated account has the correct code hash
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert_eq!(
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX2,
            "Code hash should be updated to Rex2 bytecode"
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
    fn test_deploy_oracle_contract_pre_rex2() {
        // Create a fresh in-memory database
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        // Activate all hardforks except Rex2
        let hardforks =
            MegaHardforkConfig::default().with_all_activated().without(MegaHardfork::Rex2);

        // Deploy the oracle contract
        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        // Verify that the v1.0.0 (pre-Rex2) bytecode is deployed
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert_eq!(
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH,
            "Should deploy v1.0.0 bytecode pre-Rex2"
        );
    }

    #[test]
    fn test_deploy_oracle_contract_rex2() {
        // Create a fresh in-memory database
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        // Activate all hardforks including Rex2
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        // Deploy the oracle contract
        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        // Verify that the v1.1.0 (Rex2) bytecode is deployed
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert_eq!(
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX2,
            "Should deploy v1.1.0 bytecode on Rex2"
        );
    }

    #[test]
    fn test_deploy_oracle_contract_upgrade_to_rex2() {
        // Create a database with the pre-Rex2 oracle contract already deployed
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            ORACLE_CONTRACT_ADDRESS,
            AccountInfo {
                balance: revm::primitives::U256::ZERO,
                nonce: 0,
                code_hash: ORACLE_CONTRACT_CODE_HASH,
                code: Some(Bytecode::new_raw(ORACLE_CONTRACT_CODE)),
            },
        );

        let mut state = State::builder().with_database(&mut db).build();
        // Activate all hardforks including Rex2
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        // Deploy should upgrade the contract to Rex2 bytecode
        let result = transact_deploy_oracle_contract(&hardforks, 0, &mut state)
            .expect("Deployment should succeed")
            .expect("Should return state");

        // Verify that the contract was upgraded to v1.1.0
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert_eq!(
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH_REX2,
            "Should upgrade to v1.1.0 bytecode on Rex2 activation"
        );
        assert!(account.is_touched(), "Account should be marked as touched");
        assert!(account.is_created(), "Account should be marked as created");
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
}
