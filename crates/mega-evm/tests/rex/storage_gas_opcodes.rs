//! Tests for REX hardfork storage gas costs for CREATE, CREATE2, and CALL opcodes.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaSpecId,
};

use super::storage_gas::{transact, CALLEE, CALLER, NEW_ACCOUNT};

#[test]
fn test_create_opcode() {
    // Test CREATE opcode is executed correctly in REX mode
    let mut db = MemoryDatabase::default();

    db.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));

    // Create a contract that uses CREATE opcode to deploy another contract
    let deployed_contract = BytecodeBuilder::default().stop().build();

    let code_len = deployed_contract.len();
    let creator_bytecode = BytecodeBuilder::default()
        .mstore(0x00, deployed_contract.as_ref())
        .push_number(code_len as u64)
        .push_number(0x00_u64)
        .push_number(0x00_u64)
        .append(CREATE)
        .stop()
        .build();

    db.set_account_code(CALLEE, creator_bytecode);

    let external_envs = DefaultExternalEnvs::default();

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO,
        10_000_000,
    )
    .expect("Transaction should succeed");

    assert!(result.result.is_success(), "CREATE opcode should execute successfully");
}

#[test]
fn test_create2_opcode() {
    // Test CREATE2 opcode is executed correctly in REX mode
    let mut db = MemoryDatabase::default();

    db.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));

    let deployed_contract = BytecodeBuilder::default().stop().build();

    let code_len = deployed_contract.len();
    let salt = U256::from(0x42);
    let creator_bytecode = BytecodeBuilder::default()
        .mstore(0x00, deployed_contract.as_ref())
        .push_u256(salt)
        .push_number(code_len as u64)
        .push_number(0x00_u64)
        .push_number(0x00_u64)
        .append(CREATE2)
        .stop()
        .build();

    db.set_account_code(CALLEE, creator_bytecode);

    let external_envs = DefaultExternalEnvs::default();

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO,
        10_000_000,
    )
    .expect("Transaction should succeed");

    assert!(result.result.is_success(), "CREATE2 opcode should execute successfully");
}

#[test]
fn test_call_opcode_creates_account() {
    // Test CALL opcode creating a new account by sending value
    let mut db = MemoryDatabase::default();

    db.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));
    db.set_account_balance(CALLEE, U256::from(1_000_000u64));

    // Contract that uses CALL opcode to send value to NEW_ACCOUNT
    let caller_bytecode = BytecodeBuilder::default()
        .push_number(0x00_u64) // retSize
        .push_number(0x00_u64) // retOffset
        .push_number(0x00_u64) // argsSize
        .push_number(0x00_u64) // argsOffset
        .push_number(1000_u64) // value to send
        .push_address(NEW_ACCOUNT) // address
        .push_number(100_000_u64) // gas
        .append(CALL)
        .stop()
        .build();

    db.set_account_code(CALLEE, caller_bytecode);

    let external_envs = DefaultExternalEnvs::default();

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO,
        10_000_000,
    )
    .expect("Transaction should succeed");

    assert!(result.result.is_success(), "CALL opcode should execute successfully");
}
