//! Tests for the EVM state.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{
    revm::bytecode::opcode::{INVALID, PUSH0, SLOAD},
    test_utils::{transact, MemoryDatabase},
    *,
};
use revm::database::StateBuilder;

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

/// Test that when an existing empty account is called but with no state changes, the account is
/// still marked as destroyed according to EIP-161: Specification-d. At the end of the transaction,
/// any account touched by the execution of that transaction which is now empty SHALL instead become
/// non-existent (i.e. deleted), where an account is considered to be touched when it is involved in
/// any potentially state-changing operation. This includes, but is not limited to, being the
/// recipient of a transfer of zero value.
#[test]
fn test_state_clear_with_noop_call() {
    let empty_account = address!("0000000000000000000000000000000000100001");
    let caller = address!("0000000000000000000000000000000000100000");
    let mut db = MemoryDatabase::default().account_nonce(empty_account, 0);
    let mut state = StateBuilder::<MemoryDatabase>::default()
        .with_database(&mut db)
        .with_bundle_update()
        .build();

    // Call the non-existing account with no transfer
    let result = transact(
        MegaSpecId::REX,
        &mut state,
        caller,
        Some(empty_account),
        Bytes::default(),
        U256::ZERO,
    )
    .unwrap();
    // assert that the non-existing account is marked as touched
    assert!(result.state.get(&empty_account).unwrap().is_touched());

    // apply the state changes and get the transitions
    let transitions = state.cache.apply_evm_state(result.state);
    let (_, transition) =
        transitions.iter().find(|(address, _)| *address == empty_account).unwrap();
    // assert that the storage is marked as destroyed
    assert!(transition.storage_was_destroyed);
}
