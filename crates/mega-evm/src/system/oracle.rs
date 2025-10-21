//! The oracle system contract for the `MegaETH` EVM. The oracle contract is implemented in
//! `../../system-contracts/src/Oracle.sol`.

use alloy_evm::Database;
use alloy_primitives::{address, b256, bytes, Address, Bytes, B256};
use alloy_sol_types::sol;
pub use alloy_sol_types::SolCall;
use revm::{
    database::State,
    state::{Bytecode, EvmState},
};

/// The address of the oracle system contract.
pub const ORACLE_CONTRACT_ADDRESS: Address = address!("0x6342000000000000000000000000000000000001");

/// The code of the oracle contract. It is retrieved from
/// `../../system-contracts/artifacts/Oracle.json`.
pub const ORACLE_CONTRACT_CODE: Bytes =
    bytes!("0x608060405234801561000f575f5ffd5b506004361061006f575f3560e01c80635747f6d41161004d5780635747f6d4146101185780636317e00b14610138578063d3607ed914610158575f5ffd5b80630dc9b5da1461007357806312838160146100c457806354fd4d50146100d9575b5f5ffd5b61009a7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d81565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020015b60405180910390f35b6100d76100d23660046103bd565b61016b565b005b604080518082018252600581527f312e302e30000000000000000000000000000000000000000000000000000000602082015290516100bb9190610429565b61012b61012636600461047c565b610276565b6040516100bb91906104bb565b61014a6101463660046104fd565b5490565b6040519081526020016100bb565b6100d7610166366004610514565b610302565b3373ffffffffffffffffffffffffffffffffffffffff7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d16146101da576040517f5e742c5a00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b828114610221576040517f5b7232fa000000000000000000000000000000000000000000000000000000008152600481018490526024810182905260440160405180910390fd5b5f5b8381101561026f575f85858381811061023e5761023e610534565b9050602002013590505f84848481811061025a5761025a610534565b60200291909101359092555050600101610223565b5050505050565b60608167ffffffffffffffff81111561029157610291610561565b6040519080825280602002602001820160405280156102ba578160200160208202803683370190505b5090505f5b828110156102fb575f8484838181106102da576102da610534565b905060200201359050805460208302602085010152816001019150506102bf565b5092915050565b3373ffffffffffffffffffffffffffffffffffffffff7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d1614610371576040517f5e742c5a00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b9055565b5f5f83601f840112610385575f5ffd5b50813567ffffffffffffffff81111561039c575f5ffd5b6020830191508360208260051b85010111156103b6575f5ffd5b9250929050565b5f5f5f5f604085870312156103d0575f5ffd5b843567ffffffffffffffff8111156103e6575f5ffd5b6103f287828801610375565b909550935050602085013567ffffffffffffffff811115610411575f5ffd5b61041d87828801610375565b95989497509550505050565b602081525f82518060208401528060208501604085015e5f6040828501015260407fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f83011684010191505092915050565b5f5f6020838503121561048d575f5ffd5b823567ffffffffffffffff8111156104a3575f5ffd5b6104af85828601610375565b90969095509350505050565b602080825282518282018190525f918401906040840190835b818110156104f25783518352602093840193909201916001016104d4565b509095945050505050565b5f6020828403121561050d575f5ffd5b5035919050565b5f5f60408385031215610525575f5ffd5b50508035926020909101359150565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52603260045260245ffd5b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffdfea2646970667358221220ca6e75612122f091d09fc4d7eb6d1c6faad6ab67c2183e5067014152683f274364736f6c634300081e0033");

/// The code hash of the oracle contract.
pub const ORACLE_CONTRACT_CODE_HASH: B256 =
    b256!("0x76d9dee8ee8b8353378ede74ddd42258907f7eccce4691c37ac7d69d4ef4e0a8");

sol! {
    /// The Solidity interface for the oracle contract.
    interface Oracle {
        function getSlot(bytes32 slot) external view returns (bytes32 value);
        function setSlot(bytes32 slot, bytes32 value) external;
        function getSlots(bytes32[] calldata slots) external view returns (bytes32[] memory values);
        function setSlots(bytes32[] calldata slots, bytes32[] calldata values) external;
    }
}

/// Deploys the oracle contract in the designated address and returns the state changes. Note that
/// the database `db` is not modified in this function. The caller is responsible to commit the
/// changes to database.
pub fn deploy_oracle_contract<DB: Database>(db: &mut State<DB>) -> Result<EvmState, DB::Error> {
    // Load the oracle contract account from the cache
    let acc = db.load_cache_account(ORACLE_CONTRACT_ADDRESS)?;

    // If the contract is already deployed, return early
    if acc.account_info().is_some_and(|info| info.code_hash == ORACLE_CONTRACT_CODE_HASH) {
        return Ok(EvmState::default());
    }

    // Update the account info with the contract code
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = ORACLE_CONTRACT_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(ORACLE_CONTRACT_CODE));

    // Convert the cache account back into a revm account and mark it as touched.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();

    Ok(EvmState::from_iter([(ORACLE_CONTRACT_ADDRESS, revm_acc)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use revm::{database::InMemoryDB, state::AccountInfo};

    #[test]
    fn test_oracle_contract_code_hash_matches() {
        // Compute the keccak256 hash of the oracle contract code
        let computed_hash = keccak256(&ORACLE_CONTRACT_CODE);

        // Verify it matches the hardcoded constant
        assert_eq!(
            computed_hash, ORACLE_CONTRACT_CODE_HASH,
            "Oracle contract code hash mismatch: computed {}, expected {}",
            computed_hash, ORACLE_CONTRACT_CODE_HASH
        );
    }

    #[test]
    fn test_deploy_oracle_contract_on_fresh_db() {
        // Create a fresh in-memory database
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        // Deploy the oracle contract
        let result = deploy_oracle_contract(&mut state).expect("Deployment should succeed");

        // Verify that state changes were returned
        assert_eq!(result.len(), 1, "Should have state changes for one account");
        assert!(
            result.contains_key(&ORACLE_CONTRACT_ADDRESS),
            "State changes should contain oracle contract address"
        );

        // Verify the account in the state changes
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert!(account.is_touched(), "Account should be marked as touched");

        // Verify the account info
        let info = &account.info;
        assert_eq!(
            info.code_hash, ORACLE_CONTRACT_CODE_HASH,
            "Code hash should match the expected value"
        );
        assert!(info.code.is_some(), "Code should be set");

        let code = info.code.as_ref().unwrap();
        assert_eq!(
            code.original_bytes(),
            ORACLE_CONTRACT_CODE,
            "Code bytes should match the expected value"
        );
    }

    #[test]
    fn test_deploy_oracle_contract_idempotent() {
        // Create a database with the oracle contract already deployed correctly
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

        // Deploy should return empty state (no changes needed)
        let result = deploy_oracle_contract(&mut state).expect("Deployment should succeed");
        assert_eq!(
            result.len(),
            0,
            "Deployment should return empty state when contract is already correctly deployed"
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

        // Deploy should update the contract with correct code
        let result = deploy_oracle_contract(&mut state).expect("Deployment should succeed");

        // Verify that state changes were returned (contract was updated)
        assert_eq!(result.len(), 1, "Should have state changes to update the contract");

        // Verify the updated account has the correct code hash
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist");
        assert_eq!(
            account.info.code_hash, ORACLE_CONTRACT_CODE_HASH,
            "Code hash should be updated to correct value"
        );
    }

    #[test]
    fn test_deploy_oracle_contract_marks_account_as_touched() {
        // Create a fresh in-memory database
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        // Deploy the oracle contract
        let result = deploy_oracle_contract(&mut state).expect("Deployment should succeed");

        // Get the account from result
        let account = result.get(&ORACLE_CONTRACT_ADDRESS).expect("Account should exist in result");

        // Verify the account is marked as touched (required for state changes to be committed)
        assert!(
            account.is_touched(),
            "Deployed account must be marked as touched for state changes to take effect"
        );
    }
}
