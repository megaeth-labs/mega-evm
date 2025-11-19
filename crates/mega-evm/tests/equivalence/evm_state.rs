//! Tests for the EVM state.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{
    test_utils::{transact, MemoryDatabase},
    *,
};
use revm::bytecode::opcode::{INVALID, PUSH0, SLOAD};

/// Test that verifies the EVM state correctly tracks storage access for non-existent storage slots.
/// This test ensures that when a contract performs an SLOAD operation on a storage slot that
/// doesn't exist (storage slot 0), the EVM state properly records this access and includes
/// the storage slot in the contract's storage map, even though the slot was previously
/// uninitialized.
#[test]
fn test_evm_state_zero_storage() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, revm::bytecode::opcode::SLOAD];
    db.set_account_code(contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(MegaSpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO)
            .unwrap();
    let state = result.state;
    let contract = state.get(&contract_address).unwrap();
    // the non-existent storage slot will present in the state
    assert_eq!(contract.storage.len(), 1);
    assert!(contract.storage.contains_key(&U256::ZERO));
}

/// Test that verifies EVM state includes storage reads from failed transactions.
///
/// This test ensures that when a transaction fails (reverts), any storage reads
/// that occurred before the failure are still properly recorded in the EVM state.
/// The contract reads storage slot 0 and then executes INVALID to cause a revert,
/// but the storage access should still be tracked in the resulting state.
#[test]
fn test_evm_state_include_failed_tx_reads() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, SLOAD, INVALID]; // read slot 0 and then revert
    db.set_account_code(contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(MegaSpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO)
            .unwrap();
    let storage = &result.state.get(&contract_address).as_ref().unwrap().storage;
    assert!(storage.contains_key(&U256::ZERO));
}
