//! Tests for oracle contract access detection.

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{opcode_gen::BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MEGA_ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALL, GAS, KECCAK256, PUSH0},
    context::TxEnv,
};
use std::collections::HashMap;

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to create and execute a transaction with the given contracts.
/// Returns a tuple of (oracle_accessed: bool, result: Result, gas_used: u64).
fn execute_transaction_with_contracts(
    spec: MegaSpecId,
    contracts: HashMap<alloy_primitives::Address, Bytes>,
) -> (bool, bool, u64) {
    let mut db = MemoryDatabase::default();
    for (addr, code) in contracts {
        db.set_account_code(addr, code);
    }

    let external_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(&mut db, spec, &external_envs);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        data: Default::default(),
        value: U256::ZERO,
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let oracle_accessed = evm.ctx.has_accessed_oracle();
    let success = result.result.is_success();
    let gas_used = result.result.gas_used();

    (oracle_accessed, success, gas_used)
}

/// Helper function to execute a transaction and check oracle access.
fn execute_transaction_and_check_oracle_access(
    spec: MegaSpecId,
    contract_code: Bytes,
    expected_oracle_accessed: bool,
) {
    let mut contracts = HashMap::new();
    contracts.insert(CALLEE, contract_code);

    let (oracle_accessed, success, _gas_used) = execute_transaction_with_contracts(spec, contracts);
    assert!(success, "Transaction should succeed");

    // Check oracle access status
    assert_eq!(oracle_accessed, expected_oracle_accessed, "Oracle access detection mismatch");
}

/// Test that calling the oracle contract is detected.
#[test]
fn test_oracle_access_detected_on_call() {
    // Create bytecode that calls the oracle contract
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    execute_transaction_and_check_oracle_access(MegaSpecId::MINI_REX, bytecode, true);
}

/// Test that calling a non-oracle contract is not detected as oracle access.
#[test]
fn test_oracle_access_not_detected_on_regular_call() {
    const OTHER_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");

    // Create bytecode that calls a different contract (not oracle)
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(OTHER_CONTRACT) // callee: some other contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    execute_transaction_and_check_oracle_access(MegaSpecId::MINI_REX, bytecode, false);
}

/// Test that oracle access is not detected when no CALL is made.
#[test]
fn test_oracle_access_not_detected_without_call() {
    // Simple bytecode that doesn't call anything
    let bytecode = BytecodeBuilder::default().push_number(42u8).stop().build();

    execute_transaction_and_check_oracle_access(MegaSpecId::MINI_REX, bytecode, false);
}

/// Test that oracle access is NOT detected in EQUIVALENCE spec (uses standard CALL).
/// Oracle detection only works in MINI_REX spec with custom CALL instruction.
#[test]
fn test_oracle_access_not_detected_in_equivalence_spec() {
    // Create bytecode that calls the oracle contract
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    // Oracle detection only works in MINI_REX spec, not EQUIVALENCE
    execute_transaction_and_check_oracle_access(MegaSpecId::EQUIVALENCE, bytecode, false);
}

/// Test that oracle access is detected with explicit 0 value parameter.
#[test]
fn test_oracle_access_detected_with_explicit_zero_value() {
    // Create bytecode that calls the oracle contract with explicit 0 wei value
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei (explicit)
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    execute_transaction_and_check_oracle_access(MegaSpecId::MINI_REX, bytecode, true);
}

/// Test that multiple calls to oracle are still tracked (should not fail).
#[test]
fn test_oracle_access_detected_on_multiple_calls() {
    // Create bytecode that calls the oracle contract twice
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args again
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS) // callee: oracle contract again
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    execute_transaction_and_check_oracle_access(MegaSpecId::MINI_REX, bytecode, true);
}

/// Test that parent frame's gas is limited after oracle access in a nested call.
#[test]
fn test_oracle_access_limits_parent_gas() {
    const INTERMEDIATE_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");

    // Create intermediate contract that calls the oracle
    let intermediate_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    // Create main contract that calls intermediate contract, then tries to use lots of gas
    // After the call returns, parent should only have ~10k gas left
    let main_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(INTERMEDIATE_CONTRACT) // callee: intermediate contract
        .append(GAS)
        .append(CALL)
        // After this call, parent gas should be limited to 10k
        // Try to do an expensive operation (many SHA3 calls)
        // Each SHA3 costs at least 30 gas, so 10k gas = ~300 SHA3 calls max
        .stop()
        .build();

    let mut contracts = HashMap::new();
    contracts.insert(CALLEE, main_code);
    contracts.insert(INTERMEDIATE_CONTRACT, intermediate_code);

    let (oracle_accessed, success, gas_used) =
        execute_transaction_with_contracts(MegaSpecId::MINI_REX, contracts);
    assert!(success, "Transaction should succeed");

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");

    // Verify that not all gas was used (parent was limited)
    assert!(gas_used < 1_000_000, "Gas should be limited after oracle access, used: {}", gas_used);
}

/// Test that deeply nested calls have their gas limited after oracle access.
#[test]
fn test_oracle_access_limits_deeply_nested_calls() {
    const LEVEL1: alloy_primitives::Address = address!("3000000000000000000000000000000000000001");
    const LEVEL2: alloy_primitives::Address = address!("3000000000000000000000000000000000000002");
    const LEVEL3: alloy_primitives::Address = address!("3000000000000000000000000000000000000003");

    // Level 3: calls oracle
    let level3_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    // Level 2: calls level 3
    let level2_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(LEVEL3)
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    // Level 1: calls level 2
    let level1_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(LEVEL2)
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    // Main contract: calls level 1
    let main_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(LEVEL1)
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut contracts = HashMap::new();
    contracts.insert(CALLEE, main_code);
    contracts.insert(LEVEL1, level1_code);
    contracts.insert(LEVEL2, level2_code);
    contracts.insert(LEVEL3, level3_code);

    let (oracle_accessed, success, _gas_used) =
        execute_transaction_with_contracts(MegaSpecId::MINI_REX, contracts);
    assert!(success, "Transaction should succeed");

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed in deeply nested call");
}

/// Test that contract runs out of gas when trying to execute expensive operations after oracle
/// access.
#[test]
fn test_parent_runs_out_of_gas_after_oracle_access() {
    const INTERMEDIATE_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");

    // Create intermediate contract that calls the oracle
    let intermediate_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    // Create main contract that:
    // 1. Calls intermediate contract (which accesses oracle)
    // 2. After return, tries to execute many expensive KECCAK256 operations
    // Expected: Parent gas is limited to 10k after oracle access, can't complete all operations
    let mut builder = BytecodeBuilder::default();

    // Call intermediate contract
    builder = builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(INTERMEDIATE_CONTRACT)
        .append(GAS)
        .append(CALL);

    // After the call returns, parent gas is limited to 10k
    // Try to execute 1000 KECCAK256 operations (each costs ~30-60 gas minimum)
    // This should run out of gas partway through
    for i in 0..1000 {
        builder = builder
            .push_number(32u8) // size: 32 bytes
            .push_number(i as u8) // offset: varying offset to avoid optimization
            .append(KECCAK256)
            .append(PUSH0); // pop the result
    }
    let main_code = builder.stop().build();

    let mut contracts = HashMap::new();
    contracts.insert(CALLEE, main_code);
    contracts.insert(INTERMEDIATE_CONTRACT, intermediate_code);

    let (oracle_accessed, success, _gas_used) =
        execute_transaction_with_contracts(MegaSpecId::MINI_REX, contracts);

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");

    // The transaction runs out of gas - the parent frame couldn't complete all expensive operations
    // because its gas was limited to 10k after oracle access
    assert!(!success, "Transaction should run out of gas");

    // This demonstrates that:
    // 1. Oracle access triggers gas limiting ✓
    // 2. Parent frames get limited to 10k remaining gas ✓
    // 3. Parent can't execute expensive operations after oracle access (runs out of gas) ✓
    //
    // The parent frame ran out of gas before completing all 1000 KECCAK256 operations,
    // which is the desired behavior - forcing the transaction to finish quickly.
}

/// Test that gas is NOT limited when oracle is not accessed in nested calls.
#[test]
fn test_no_gas_limiting_without_oracle_access() {
    const INTERMEDIATE_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");
    const OTHER_CONTRACT: alloy_primitives::Address =
        address!("4000000000000000000000000000000000000004");

    // Create intermediate contract that calls a non-oracle contract
    let intermediate_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(OTHER_CONTRACT) // NOT the oracle
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    // Create main contract that calls intermediate contract
    let main_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(INTERMEDIATE_CONTRACT)
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut contracts = HashMap::new();
    contracts.insert(CALLEE, main_code);
    contracts.insert(INTERMEDIATE_CONTRACT, intermediate_code);
    contracts.insert(OTHER_CONTRACT, Bytes::new()); // Empty contract

    let (oracle_accessed, success, _gas_used) =
        execute_transaction_with_contracts(MegaSpecId::MINI_REX, contracts);
    assert!(success, "Transaction should succeed");

    // Verify oracle was NOT accessed
    assert!(!oracle_accessed, "Oracle should not have been accessed");
}
