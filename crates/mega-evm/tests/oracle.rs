//! Tests for oracle contract access detection.
#![allow(clippy::doc_markdown)]

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    constants::mini_rex::SENSITIVE_DATA_ACCESS_REMAINING_GAS,
    test_utils::{BytecodeBuilder, GasInspector, MemoryDatabase, MsgCallMeta},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MEGA_ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALL, GAS, KECCAK256, PUSH0, SLOAD},
    context::TxEnv,
};
use std::collections::HashMap;

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to create and execute a transaction with the given contracts.
/// Returns a tuple of `(oracle_accessed: bool, result: Result, gas_used: u64, gas_inspector:
/// Option<GasInspector>)`.
fn execute_transaction_with_contracts(
    spec: MegaSpecId,
    contracts: HashMap<alloy_primitives::Address, Bytes>,
    gas_inspector: Option<&mut GasInspector>,
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

    let (result, oracle_accessed) = if let Some(inspector) = gas_inspector {
        let mut evm = MegaEvm::new(context).with_inspector(inspector);
        let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
        let oracle_accessed = evm.ctx.sensitive_data_tracker.borrow().has_accessed_oracle();
        (result, oracle_accessed)
    } else {
        let mut evm = MegaEvm::new(context);
        let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
        let oracle_accessed = evm.ctx.sensitive_data_tracker.borrow().has_accessed_oracle();
        (result, oracle_accessed)
    };

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

    let (oracle_accessed, success, _gas_used) =
        execute_transaction_with_contracts(spec, contracts, None);
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
/// Oracle detection only works in `MINI_REX` spec with custom CALL instruction.
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
        .push_number(0u8)
        .stop()
        .build();

    // Create main contract that calls intermediate contract, then executes more opcodes
    // After the call returns, parent should only have SENSITIVE_DATA_ACCESS_REMAINING_GAS left
    let main_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(INTERMEDIATE_CONTRACT) // callee: intermediate contract
        .append(GAS)
        .append(CALL)
        // After this call, parent gas should be limited to SENSITIVE_DATA_ACCESS_REMAINING_GAS
        // Execute a few more opcodes to verify gas limiting persists
        .push_number(42u8)
        .push_number(100u8)
        .append_many([PUSH0, PUSH0])
        .stop()
        .build();

    let mut contracts = HashMap::new();
    contracts.insert(CALLEE, main_code);
    contracts.insert(INTERMEDIATE_CONTRACT, intermediate_code);

    let mut gas_inspector = GasInspector::new();
    let (oracle_accessed, success, _gas_used) = execute_transaction_with_contracts(
        MegaSpecId::MINI_REX,
        contracts,
        Some(&mut gas_inspector),
    );
    assert!(success, "Transaction should succeed");
    assert!(oracle_accessed, "Oracle should have been accessed");

    // Check that after the call to oracle address, the total gas is limited to
    // SENSITIVE_DATA_ACCESS_REMAINING_GAS
    let mut accessed = false;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, node, _item_location, item| {
            if accessed {
                assert!(
                    item.borrow().gas_after <= SENSITIVE_DATA_ACCESS_REMAINING_GAS,
                    "Gas after oracle access is greater than SENSITIVE_DATA_ACCESS_REMAINING_GAS"
                );
            } else {
                match &node.borrow().meta {
                    MsgCallMeta::Call(call_inputs) => {
                        if call_inputs.target_address == MEGA_ORACLE_CONTRACT_ADDRESS {
                            accessed = true;
                        }
                    }
                    MsgCallMeta::Create(_) => {}
                }
            }
        },
    );
}

/// Test that contract runs out of gas when trying to execute expensive operations after oracle
/// access.
#[test]
fn test_parent_runs_out_of_gas_after_oracle_access() {
    const INTERMEDIATE_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");

    // Create intermediate contract that calls the oracle
    let mut builder = BytecodeBuilder::default();
    builder = builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL);
    // After the call returns, the left gas is limited to 10k
    // Try to execute 1000 KECCAK256 operations (each costs ~30-60 gas minimum)
    // This should run out of gas partway through
    for i in 0..1000 {
        builder = builder
            .push_number(32u8) // size: 32 bytes
            .push_number(i as u8) // offset: varying offset to avoid optimization
            .append(KECCAK256)
            .append(PUSH0); // pop the result
    }
    let intermediate_code = builder.stop().build();

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
    let main_code = builder.append(SLOAD).stop().build();

    let mut contracts = HashMap::new();
    contracts.insert(CALLEE, main_code);
    contracts.insert(INTERMEDIATE_CONTRACT, intermediate_code);

    let mut gas_inspector = GasInspector::new();
    let (oracle_accessed, success, _gas_used) = execute_transaction_with_contracts(
        MegaSpecId::MINI_REX,
        contracts,
        Some(&mut gas_inspector),
    );

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");

    // The transaction runs out of gas - the parent frame couldn't complete all expensive operations
    // because its gas was limited to 10k after oracle access
    assert!(!success, "Transaction should run out of gas");
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

    let mut gas_inspector = GasInspector::new();
    let (oracle_accessed, success, _gas_used) = execute_transaction_with_contracts(
        MegaSpecId::MINI_REX,
        contracts,
        Some(&mut gas_inspector),
    );
    assert!(success, "Transaction should succeed");

    // Verify oracle was NOT accessed
    assert!(!oracle_accessed, "Oracle should not have been accessed");

    // Verify that the gas is not limited
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            assert!(
                item.borrow().gas_after > SENSITIVE_DATA_ACCESS_REMAINING_GAS,
                "Gas after oracle access is greater than SENSITIVE_DATA_ACCESS_REMAINING_GAS"
            );
        },
    );
}

/// Test that when the oracle contract has code, that code is also subject to the gas limit.
#[test]
fn test_oracle_contract_code_subject_to_gas_limit() {
    // Create oracle contract code that tries to execute many expensive operations
    // Since calling the oracle immediately limits gas to 10k, the oracle's own code
    // should run out of gas when trying to execute too many operations
    let mut builder = BytecodeBuilder::default();
    // Try to execute 1000 KECCAK256 operations (each costs ~30-60 gas minimum)
    // With only 10k gas limit, this should run out of gas partway through
    for i in 0..1000 {
        builder = builder
            .push_number(32u8) // size: 32 bytes
            .push_number(i as u8) // offset: varying offset to avoid optimization
            .append(KECCAK256)
            .append(PUSH0); // pop the result
    }
    let oracle_code = builder.stop().build();

    // Create main contract that calls the oracle
    let main_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(MEGA_ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut contracts = HashMap::new();
    contracts.insert(CALLEE, main_code);
    contracts.insert(MEGA_ORACLE_CONTRACT_ADDRESS, oracle_code);

    let mut gas_inspector = GasInspector::new();
    let (oracle_accessed, success, _gas_used) = execute_transaction_with_contracts(
        MegaSpecId::MINI_REX,
        contracts,
        Some(&mut gas_inspector),
    );

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");

    // The transaction should run out of gas because the oracle contract's code
    // tries to execute too many expensive operations with only 10k gas available
    assert!(!success, "Transaction should run out of gas due to oracle contract code exceeding gas limit");

    // Verify that inside the oracle contract call, gas was limited
    let mut inside_oracle = false;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, node, _item_location, item| {
            match &node.borrow().meta {
                MsgCallMeta::Call(call_inputs) => {
                    if call_inputs.target_address == MEGA_ORACLE_CONTRACT_ADDRESS {
                        inside_oracle = true;
                    }
                }
                MsgCallMeta::Create(_) => {}
            }
            if inside_oracle {
                assert!(
                    item.borrow().gas_after <= SENSITIVE_DATA_ACCESS_REMAINING_GAS,
                    "Gas inside oracle contract should be limited to SENSITIVE_DATA_ACCESS_REMAINING_GAS"
                );
            }
        },
    );
}
