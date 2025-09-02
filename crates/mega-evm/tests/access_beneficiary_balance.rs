//! Tests for beneficiary balance access detection functionality

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{Context, Evm, SpecId, Transaction};
use revm::{
    bytecode::opcode::{
        BALANCE, COINBASE, EXTCODEHASH, EXTCODESIZE, POP, PUSH1, PUSH20, SSTORE, STOP,
    },
    context::{BlockEnv, ContextSetters, ContextTr, TxEnv},
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
};

const BENEFICIARY: Address = address!("0000000000000000000000000000000000BEEF01");
const CALLER_ADDR: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT_ADDR: Address = address!("0000000000000000000000000000000000100001");

/// Helper function to set account code
fn set_account_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = AccountInfo { 
        code: Some(bytecode), 
        code_hash, 
        ..Default::default() 
    };
    db.insert_account_info(address, account_info);
}

fn create_evm_with_beneficiary() -> Evm<CacheDB<EmptyDB>, NoOpInspector> {
    let db = CacheDB::<EmptyDB>::default();
    let mut context = Context::new(db, SpecId::MINI_REX);
    
    // Set beneficiary in block environment
    let block_env = BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        ..Default::default()
    };
    context.set_block(block_env);
    
    // Configure L1BlockInfo to avoid operator fee scalar panic
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    
    Evm::new(context, NoOpInspector)
}

fn create_evm_with_disabled_beneficiary() -> Evm<CacheDB<EmptyDB>, NoOpInspector> {
    let db = CacheDB::<EmptyDB>::default();
    let mut context = Context::new(db, SpecId::MINI_REX);
    
    // Set beneficiary in block environment
    let block_env = BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        ..Default::default()
    };
    context.set_block(block_env);
    
    // Configure L1BlockInfo to avoid operator fee scalar panic
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    
    let mut evm = Evm::new(context, NoOpInspector);
    // Disable beneficiary rewards (but access detection still works)
    evm.disable_beneficiary();
    evm
}

fn execute_transaction(
    evm: &mut Evm<CacheDB<EmptyDB>, NoOpInspector>,
    caller: Address,
    to: Option<Address>,
    data: Bytes,
    value: U256,
) -> bool {
    let tx = Transaction {
        base: TxEnv {
            caller,
            kind: match to {
                Some(addr) => TxKind::Call(addr),
                None => TxKind::Create,
            },
            data,
            value,
            gas_limit: 100000,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = alloy_evm::Evm::transact_raw(evm, tx);
    println!("Result: {:?}", result);
    result.is_ok() && result.unwrap().result.is_success()
}

/// Test scenario 1: beneficiary initiates transaction
#[test]
fn test_beneficiary_as_caller() {
    let mut evm = create_evm_with_beneficiary();
    
    // Create simple contract that just stops
    let contract_code = vec![STOP];
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    // Execute transaction with beneficiary as caller
    let success = execute_transaction(
        &mut evm,
        BENEFICIARY, // beneficiary as caller
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access when beneficiary is caller"
    );
}

/// Test scenario 2: beneficiary as recipient of ETH transfer
#[test]
fn test_beneficiary_as_recipient() {
    let mut evm = create_evm_with_beneficiary();

    // Give caller some balance for transfer
    evm.ctx().db_mut().insert_account_info(
        CALLER_ADDR,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u64), // 1 ETH
            ..Default::default()
        }
    );

    // Execute transaction sending ETH to beneficiary
    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(BENEFICIARY), // beneficiary as recipient
        Bytes::default(),
        U256::from(500_000_000_000_000_000u64), // 0.5 ETH
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access when beneficiary is recipient"
    );
}

/// Test scenario 3: COINBASE opcode access
#[test]
fn test_coinbase_opcode_access() {
    let mut evm = create_evm_with_beneficiary();
    
    // Contract that uses COINBASE opcode
    let contract_code = vec![COINBASE, POP, STOP];
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    // Note: COINBASE opcode access is tracked by block environment access tracking,
    // The COINBASE opcode returns the beneficiary address, but doesn't necessarily 
    // access the beneficiary balance
}

/// Test scenario 4: Contract reads beneficiary balance using BALANCE opcode
#[test]
fn test_contract_reads_beneficiary_balance() {
    let mut evm = create_evm_with_beneficiary();
    
    // Contract that reads beneficiary balance
    let mut contract_code = vec![];
    // Push beneficiary address onto stack
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    // Call BALANCE opcode
    contract_code.push(BALANCE);
    contract_code.push(POP); // Remove balance from stack
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access when contract reads beneficiary balance"
    );
}

/// Test scenario 5: EXTCODE* opcodes accessing beneficiary
#[test]
fn test_extcode_access_beneficiary() {
    let mut evm = create_evm_with_beneficiary();
    
    // Give beneficiary some code for testing
    set_account_code(evm.ctx().db_mut(), BENEFICIARY, vec![STOP].into());

    // Test EXTCODESIZE
    let mut contract_code = vec![];
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(EXTCODESIZE);
    contract_code.push(POP);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access when using EXTCODESIZE on beneficiary"
    );

    // Reset for next test
    evm.ctx().reset_block_env_access();
    assert!(
        !evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Access flag should be reset"
    );

    // Test EXTCODEHASH
    let mut contract_code = vec![];
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(EXTCODEHASH);
    contract_code.push(POP);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access when using EXTCODEHASH on beneficiary"
    );
}

/// Test scenario 6: SSTORE with beneficiary address (edge case)
#[test]
fn test_sstore_beneficiary_address() {
    let mut evm = create_evm_with_beneficiary();
    
    // Contract that stores beneficiary address in storage
    let mut contract_code = vec![];
    // Push beneficiary address as value
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    // Push storage slot 0
    contract_code.push(PUSH1);
    contract_code.push(0);
    // Store beneficiary address at slot 0
    contract_code.push(SSTORE);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    // Note: SSTORE itself doesn't trigger beneficiary access detection,
    // but if the result state contains beneficiary changes, it might be relevant
    // This test ensures that storing beneficiary address doesn't cause issues
}

/// Test scenario: Multiple beneficiary accesses in one transaction
#[test]
fn test_multiple_beneficiary_accesses() {
    let mut evm = create_evm_with_beneficiary();
    
    // Contract that accesses beneficiary multiple ways
    let mut contract_code = vec![];
    
    // 1. Read beneficiary balance
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(BALANCE);
    contract_code.push(POP);
    
    // 2. Get beneficiary code size
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(EXTCODESIZE);
    contract_code.push(POP);
    
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access with multiple operations"
    );
}

/// Test scenario: Non-beneficiary address should not trigger detection
#[test]
fn test_non_beneficiary_no_detection() {
    let mut evm = create_evm_with_beneficiary();
    
    let non_beneficiary = address!("0000000000000000000000000000000000DEAD01");
    
    // Contract that reads non-beneficiary balance
    let mut contract_code = vec![];
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(non_beneficiary.as_slice());
    contract_code.push(BALANCE);
    contract_code.push(POP);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        !evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should NOT detect beneficiary access for non-beneficiary address"
    );
}

/// Test reset functionality between transactions
#[test]
fn test_reset_between_transactions() {
    let mut evm = create_evm_with_beneficiary();
    
    // First transaction: access beneficiary
    let mut contract_code = vec![];
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(BALANCE);
    contract_code.push(POP);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "First transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access in first transaction"
    );

    // Reset access tracking
    evm.ctx().reset_block_env_access();
    assert!(
        !evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Access flag should be reset"
    );

    // Second transaction: don't access beneficiary
    let contract_code2 = vec![STOP];
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code2.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Second transaction should succeed");
    assert!(
        !evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should NOT detect beneficiary access in second transaction"
    );
}

// Tests with beneficiary rewards disabled (but access detection still enabled)

/// Test that beneficiary access detection still works when beneficiary rewards are disabled
#[test]
fn test_disabled_beneficiary_still_detects_access() {
    let mut evm = create_evm_with_disabled_beneficiary();
    
    // Contract that reads beneficiary balance
    let mut contract_code = vec![];
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(BALANCE);
    contract_code.push(POP);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should still detect beneficiary access even when beneficiary rewards are disabled"
    );
}

/// Test beneficiary as caller with disabled rewards
#[test]
fn test_disabled_beneficiary_caller_detection() {
    let mut evm = create_evm_with_disabled_beneficiary();
    
    // Simple contract that just stops
    let contract_code = vec![STOP];
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    // Execute transaction with beneficiary as caller
    let success = execute_transaction(
        &mut evm,
        BENEFICIARY, // beneficiary as caller
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access when beneficiary is caller (even with rewards disabled)"
    );
}

/// Test beneficiary as recipient with disabled rewards
#[test]
fn test_disabled_beneficiary_recipient_detection() {
    let mut evm = create_evm_with_disabled_beneficiary();

    // Give caller some balance for transfer
    evm.ctx().db_mut().insert_account_info(
        CALLER_ADDR,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u64), // 1 ETH
            ..Default::default()
        }
    );

    // Execute transaction sending ETH to beneficiary
    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(BENEFICIARY), // beneficiary as recipient
        Bytes::default(),
        U256::from(500_000_000_000_000_000u64), // 0.5 ETH
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access when beneficiary is recipient (even with rewards disabled)"
    );
}

/// Test EXTCODE operations with disabled beneficiary rewards
#[test]
fn test_disabled_beneficiary_extcode_detection() {
    let mut evm = create_evm_with_disabled_beneficiary();
    
    // Give beneficiary some code for testing
    set_account_code(evm.ctx().db_mut(), BENEFICIARY, vec![STOP].into());

    // Test EXTCODESIZE
    let mut contract_code = vec![];
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(EXTCODESIZE);
    contract_code.push(POP);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access when using EXTCODESIZE on beneficiary (even with rewards disabled)"
    );
}

/// Test that non-beneficiary access doesn't trigger detection with disabled rewards
#[test]
fn test_disabled_beneficiary_no_false_positives() {
    let mut evm = create_evm_with_disabled_beneficiary();
    
    let non_beneficiary = address!("0000000000000000000000000000000000DEAD01");
    
    // Contract that reads non-beneficiary balance
    let mut contract_code = vec![];
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(non_beneficiary.as_slice());
    contract_code.push(BALANCE);
    contract_code.push(POP);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        !evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should NOT detect beneficiary access for non-beneficiary address (with rewards disabled)"
    );
}

/// Test SSTORE with beneficiary address with disabled rewards (edge case)
#[test]
fn test_disabled_beneficiary_sstore_address() {
    let mut evm = create_evm_with_disabled_beneficiary();
    
    // Contract that stores beneficiary address in storage
    let mut contract_code = vec![];
    // Push beneficiary address as value
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    // Push storage slot 0
    contract_code.push(PUSH1);
    contract_code.push(0);
    // Store beneficiary address at slot 0
    contract_code.push(SSTORE);
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    // Note: SSTORE itself doesn't trigger beneficiary access detection,
    // but if the result state contains beneficiary changes, it might be relevant
    // This test ensures that storing beneficiary address doesn't cause issues
    // even with beneficiary rewards disabled
}

/// Test multiple beneficiary accesses in one transaction with disabled rewards
#[test]
fn test_disabled_beneficiary_multiple_accesses() {
    let mut evm = create_evm_with_disabled_beneficiary();
    
    // Contract that accesses beneficiary multiple ways
    let mut contract_code = vec![];
    
    // 1. Read beneficiary balance
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(BALANCE);
    contract_code.push(POP);
    
    // 2. Get beneficiary code size
    contract_code.push(PUSH20);
    contract_code.extend_from_slice(BENEFICIARY.as_slice());
    contract_code.push(EXTCODESIZE);
    contract_code.push(POP);
    
    contract_code.push(STOP);
    
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, contract_code.into());

    let success = execute_transaction(
        &mut evm,
        CALLER_ADDR,
        Some(CONTRACT_ADDR),
        Bytes::default(),
        U256::ZERO,
    );
    
    assert!(success, "Transaction should succeed");
    assert!(
        evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should detect beneficiary access with multiple operations (even with rewards disabled)"
    );
}