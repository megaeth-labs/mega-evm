//! Tests for the EVM state.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{test_utils::*, *};
use revm::{
    bytecode::opcode::PUSH0,
    database::{CacheDB, EmptyDB},
};

#[test]
fn test_evm_state_zero_storage() {
    let mut db = CacheDB::<EmptyDB>::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, revm::bytecode::opcode::SLOAD];
    set_account_code(&mut db, contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(SpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO).unwrap();
    let state = result.state;
    let contract = state.get(&contract_address).unwrap();
    // the non-existent storage slot will present in the state
    assert_eq!(contract.storage.len(), 1);
    assert!(contract.storage.contains_key(&U256::ZERO));
}
