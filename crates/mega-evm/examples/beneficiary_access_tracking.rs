//! This example demonstrates the beneficiary access detection feature.
//! It creates contracts that access beneficiary account data and shows how
//! the system can detect when such access occurs during execution.
//!
//! The contracts tested are:
//! 1. Contract 1: Reads beneficiary balance (should trigger access detection)
//! 2. Contract 2: Does NOT access beneficiary data (should not trigger detection)
//! 3. Contract 3: Accesses beneficiary via different operations (should trigger detection)

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{DefaultExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction};
use revm::{
    bytecode::opcode::{BALANCE, CALLER, POP, PUSH20, STOP},
    context::{BlockEnv, ContextSetters, ContextTr, TxEnv},
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Beneficiary Access Tracking Demo ===\n");

    // Create database and EVM context
    let mut db = CacheDB::<EmptyDB>::default();
    let spec = MegaSpecId::MINI_REX;
    let beneficiary = address!("0000000000000000000000000000000000000001");

    // Contract 1: Reads beneficiary balance
    let contract1_address = address!("0000000000000000000000000000000000100001");
    let mut contract1_code = vec![];
    // Push beneficiary address onto stack
    contract1_code.push(PUSH20);
    contract1_code.extend(beneficiary.as_slice());
    // Get balance of beneficiary
    contract1_code.push(BALANCE);
    contract1_code.push(POP); // Remove result from stack
    contract1_code.push(STOP);
    set_account_code(&mut db, contract1_address, contract1_code.into());

    // Contract 2: Does NOT access beneficiary (uses CALLER)
    let contract2_address = address!("0000000000000000000000000000000000100002");
    let contract2_code = vec![CALLER, POP, STOP];
    set_account_code(&mut db, contract2_address, contract2_code.into());

    // Contract 3: Accesses beneficiary balance (same as contract 1, but separate test)
    let contract3_address = address!("0000000000000000000000000000000000100003");
    let mut contract3_code = vec![];
    // Push beneficiary address onto stack
    contract3_code.push(PUSH20);
    contract3_code.extend(beneficiary.as_slice());
    // Get balance of beneficiary
    contract3_code.push(BALANCE);
    contract3_code.push(POP);
    contract3_code.push(STOP);
    set_account_code(&mut db, contract3_address, contract3_code.into());

    // Create EVM instance with properly configured context
    let mut context = MegaContext::new(db, spec, DefaultExternalEnvs::default());

    // Set the beneficiary in the block environment
    let block_env = BlockEnv { beneficiary, ..Default::default() };
    context.set_block(block_env);

    // Set operator fee fields to zero to avoid panic in MINI_REX spec
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));

    let mut evm = MegaEvm::new(context);

    let caller = address!("0000000000000000000000000000000000100000");

    // Test Contract 1 (reads beneficiary balance)
    println!("Testing Contract 1 (reads beneficiary balance):");

    assert!(
        !evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should start with no beneficiary access"
    );
    println!("  Initial state: no beneficiary access detected");

    let tx1 = MegaTransaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract1_address),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit: 100000, // Use reasonable gas limit
            ..Default::default()
        },
        ..Default::default()
    };

    let result1 = alloy_evm::Evm::transact_raw(&mut evm, tx1)?;
    println!("  Transaction successful: {}", result1.result.is_success());
    println!("  Gas used: {}", result1.result.gas_used());

    // Verify that beneficiary access was detected
    let beneficiary_accessed = evm.ctx_ref().has_accessed_beneficiary_balance();
    println!("  Beneficiary access detected: {}", beneficiary_accessed);
    assert!(
        beneficiary_accessed,
        "Contract 1 should have detected beneficiary access (balance read)!"
    );
    assert!(result1.result.is_success(), "Contract 1 transaction should succeed");

    // Reset for next transaction
    evm.ctx_mut().reset_block_env_access();
    println!("  After reset: no beneficiary access detected\n");

    // Test Contract 2 (does NOT access beneficiary)
    println!("Testing Contract 2 (does NOT access beneficiary):");

    assert!(
        !evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should start with no beneficiary access"
    );
    println!("  Initial state: no beneficiary access detected");

    let tx2 = MegaTransaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract2_address),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit: 100000,
            ..Default::default()
        },
        ..Default::default()
    };

    let result2 = alloy_evm::Evm::transact_raw(&mut evm, tx2)?;
    println!("  Transaction successful: {}", result2.result.is_success());
    println!("  Gas used: {}", result2.result.gas_used());

    // Verify that beneficiary was NOT accessed
    let beneficiary_accessed = evm.ctx_ref().has_accessed_beneficiary_balance();
    println!("  Beneficiary access detected: {}", beneficiary_accessed);
    assert!(!beneficiary_accessed, "Contract 2 should NOT have detected beneficiary access!");
    assert!(result2.result.is_success(), "Contract 2 transaction should succeed");

    // Reset for next transaction
    evm.ctx_mut().reset_block_env_access();
    println!("  After reset: no beneficiary access detected\n");

    // Test Contract 3 (accesses beneficiary balance)
    println!("Testing Contract 3 (accesses beneficiary balance):");

    assert!(
        !evm.ctx_ref().has_accessed_beneficiary_balance(),
        "Should start with no beneficiary access"
    );
    println!("  Initial state: no beneficiary access detected");

    let tx3 = MegaTransaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract3_address),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit: 100000,
            ..Default::default()
        },
        ..Default::default()
    };

    let result3 = alloy_evm::Evm::transact_raw(&mut evm, tx3)?;
    println!("  Transaction successful: {}", result3.result.is_success());
    println!("  Gas used: {}", result3.result.gas_used());

    // Verify that beneficiary access was detected
    let beneficiary_accessed = evm.ctx_ref().has_accessed_beneficiary_balance();
    println!("  Beneficiary access detected: {}", beneficiary_accessed);
    assert!(
        beneficiary_accessed,
        "Contract 3 should have detected beneficiary access (balance read)!"
    );
    assert!(result3.result.is_success(), "Contract 3 transaction should succeed");

    println!("\n=== Tracking Summary ===");
    println!("✅ Beneficiary access tracking is working correctly!");
    println!("✅ Contract 1 (balance read): Access tracked = true");
    println!("✅ Contract 2 (no access): Access tracked = false");
    println!("✅ Contract 3 (balance read): Access tracked = true");
    println!("✅ All tracking tests passed!");
    println!(
        "\nThe system can accurately track when transactions access beneficiary account data."
    );
    println!(
        "This detection mechanism can be used for monitoring, analytics, or enforcement purposes."
    );

    Ok(())
}

/// Helper function to set account code
fn set_account_code(db: &mut CacheDB<EmptyDB>, address: alloy_primitives::Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = AccountInfo { code: Some(bytecode), code_hash, ..Default::default() };
    db.insert_account_info(address, account_info);
}
