//! The oracle system contract for the `MegaETH` EVM. The oracle contract is implemented in
//! `../../../system-contracts/src/Oracle.sol`.

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
/// It is retrieved from `../../../system-contracts/artifacts/Oracle.json`.
pub const ORACLE_CONTRACT_CODE: Bytes =
    bytes!("0x608060405234801561000f575f5ffd5b506004361061006f575f3560e01c80637eba7ba61161004d5780637eba7ba614610118578063a21e2d6914610138578063fbc0d03514610158575f5ffd5b806301caec13146100735780630dc9b5da1461008857806354fd4d50146100d9575b5f5ffd5b610086610081366004610324565b61016b565b005b6100af7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d81565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020015b60405180910390f35b604080518082018252600581527f312e302e30000000000000000000000000000000000000000000000000000000602082015290516100d09190610390565b61012a6101263660046103e3565b5490565b6040519081526020016100d0565b61014b6101463660046103fa565b6101e6565b6040516100d09190610439565b61008661016636600461047b565b61025c565b8281146101b2576040517f5b7232fa000000000000000000000000000000000000000000000000000000008152600481018490526024810182905260440160405180910390fd5b8382845f5b818110156101d457602081028381013590850135556001016101b7565b505050506101e061026b565b50505050565b60608167ffffffffffffffff8111156102015761020161049b565b60405190808252806020026020018201604052801561022a578160200160208202803683370190505b5090506020810183835f5b818110156102525760208102838101355490850152600101610235565b5050505092915050565b80825561026761026b565b5050565b3373ffffffffffffffffffffffffffffffffffffffff7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d16146102da576040517f5e742c5a00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b565b5f5f83601f8401126102ec575f5ffd5b50813567ffffffffffffffff811115610303575f5ffd5b6020830191508360208260051b850101111561031d575f5ffd5b9250929050565b5f5f5f5f60408587031215610337575f5ffd5b843567ffffffffffffffff81111561034d575f5ffd5b610359878288016102dc565b909550935050602085013567ffffffffffffffff811115610378575f5ffd5b610384878288016102dc565b95989497509550505050565b602081525f82518060208401528060208501604085015e5f6040828501015260407fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f83011684010191505092915050565b5f602082840312156103f3575f5ffd5b5035919050565b5f5f6020838503121561040b575f5ffd5b823567ffffffffffffffff811115610421575f5ffd5b61042d858286016102dc565b90969095509350505050565b602080825282518282018190525f918401906040840190835b81811015610470578351835260209384019390920191600101610452565b509095945050505050565b5f5f6040838503121561048c575f5ffd5b50508035926020909101359150565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffdfea26469706673582212205bb66f27c8ccdec3b5bbd6071d5f516754488531634a3ad38e6c7ffacf47a02464736f6c634300081e0033");

/// The code hash of the oracle contract (version 1.0.0, pre-Rex2).
pub const ORACLE_CONTRACT_CODE_HASH: B256 =
    b256!("0xe9b044afb735a0f569faeb248088b4f267578f60722f87d06ec3867b250a2c34");

/// The code of the oracle contract (version 1.1.0, Rex2+).
/// This version includes the `sendHint` function for the oracle hint mechanism.
/// It is retrieved from `../../../system-contracts/artifacts/Oracle.json`.
pub const ORACLE_CONTRACT_CODE_REX2: Bytes =
    bytes!("0x608060405234801561000f575f5ffd5b50600436106100b9575f3560e01c806366cdf82f116100725780638d4909dc116100585780638d4909dc146101c8578063a21e2d69146101db578063fbc0d035146101fb575f5ffd5b806366cdf82f146101955780637eba7ba6146101a8575f5ffd5b8063138f5ec5116100a2578063138f5ec514610123578063348a0cdc1461013657806354fd4d5014610156575f5ffd5b806301caec13146100bd5780630dc9b5da146100d2575b5f5ffd5b6100d06100cb3660046105db565b61020e565b005b6100f97f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d81565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020015b60405180910390f35b6100d0610131366004610647565b61028a565b61014961014436600461068f565b6102d4565b60405161011a919061071a565b604080518082018252600581527f312e312e300000000000000000000000000000000000000000000000000000006020820152905161011a919061079b565b6100d06101a33660046107b4565b505050565b6101ba6101b636600461082b565b5490565b60405190815260200161011a565b6100d06101d63660046107b4565b61044b565b6101ee6101e936600461068f565b61045e565b60405161011a9190610842565b6100d0610209366004610884565b6104d4565b828114610256576040517f5b7232fa00000000000000000000000000000000000000000000000000000000815260048101849052602481018290526044015b60405180910390fd5b8382845f5b81811015610278576020810283810135908501355560010161025b565b505050506102846104e3565b50505050565b805f5b818110156102ca576102c2858585848181106102ab576102ab6108a4565b90506020028101906102bd91906108d1565b610554565b60010161028d565b50506101a36104e3565b60608167ffffffffffffffff8111156102ef576102ef610932565b60405190808252806020026020018201604052801561032257816020015b606081526020019060019003908161030d5790505b5090505f5b82811015610444575f8030868685818110610344576103446108a4565b905060200281019061035691906108d1565b60405161036492919061095f565b5f60405180830381855af49150503d805f811461039c576040519150601f19603f3d011682016040523d82523d5f602084013e6103a1565b606091505b50915091508161041c578051156103ba57805181602001fd5b6040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152601660248201527f4d756c746963616c6c3a2063616c6c206661696c656400000000000000000000604482015260640161024d565b8084848151811061042f5761042f6108a4565b60209081029190910101525050600101610327565b5092915050565b610456838383610554565b6101a36104e3565b60608167ffffffffffffffff81111561047957610479610932565b6040519080825280602002602001820160405280156104a2578160200160208202803683370190505b5090506020810183835f5b818110156104ca57602081028381013554908501526001016104ad565b5050505092915050565b8082556104df6104e3565b5050565b3373ffffffffffffffffffffffffffffffffffffffff7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d1614610552576040517f5e742c5a00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b565b827fda678492695e6a825d786c2375f7fdf3c1dc012451c61c1804227f499b0fc53e838360405161058692919061096e565b60405180910390a2505050565b5f5f83601f8401126105a3575f5ffd5b50813567ffffffffffffffff8111156105ba575f5ffd5b6020830191508360208260051b85010111156105d4575f5ffd5b9250929050565b5f5f5f5f604085870312156105ee575f5ffd5b843567ffffffffffffffff811115610604575f5ffd5b61061087828801610593565b909550935050602085013567ffffffffffffffff81111561062f575f5ffd5b61063b87828801610593565b95989497509550505050565b5f5f5f60408486031215610659575f5ffd5b83359250602084013567ffffffffffffffff811115610676575f5ffd5b61068286828701610593565b9497909650939450505050565b5f5f602083850312156106a0575f5ffd5b823567ffffffffffffffff8111156106b6575f5ffd5b6106c285828601610593565b90969095509350505050565b5f81518084528060208401602086015e5f6020828601015260207fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f83011685010191505092915050565b5f602082016020835280845180835260408501915060408160051b8601019250602086015f5b8281101561078f577fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffc087860301845261077a8583516106ce565b94506020938401939190910190600101610740565b50929695505050505050565b602081525f6107ad60208301846106ce565b9392505050565b5f5f5f604084860312156107c6575f5ffd5b83359250602084013567ffffffffffffffff8111156107e3575f5ffd5b8401601f810186136107f3575f5ffd5b803567ffffffffffffffff811115610809575f5ffd5b86602082840101111561081a575f5ffd5b939660209190910195509293505050565b5f6020828403121561083b575f5ffd5b5035919050565b602080825282518282018190525f918401906040840190835b8181101561087957835183526020938401939092019160010161085b565b509095945050505050565b5f5f60408385031215610895575f5ffd5b50508035926020909101359150565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52603260045260245ffd5b5f5f83357fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe1843603018112610904575f5ffd5b83018035915067ffffffffffffffff82111561091e575f5ffd5b6020019150368190038213156105d4575f5ffd5b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffd5b818382375f9101908152919050565b60208152816020820152818360408301375f818301604090810191909152601f9092017fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe016010191905056fea2646970667358221220af1d3dbef556839392612429fa5836dad825379cb5e7bd8f077f6f5e340e45f264736f6c634300081e0033");

/// The code hash of the oracle contract (version 1.1.0, Rex2+).
pub const ORACLE_CONTRACT_CODE_HASH_REX2: B256 =
    b256!("0xd72c677d9b118c7cd85442533d4670877f781936067e89e41d91c8478e002641");

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
