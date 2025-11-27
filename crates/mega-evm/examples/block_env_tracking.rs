//! This example demonstrates the block environment access tracking in the EVM.
//! It creates three contracts that use different block environment accesses and
//! tests the tracking functionality.
//!
//! The contracts are:
//! 1. Contract 1: Uses single block environment access (NUMBER opcode)
//! 2. Contract 2: Uses multiple block environment accesses (NUMBER, TIMESTAMP, BASEFEE opcodes)
//! 3. Contract 3: Does NOT use block environment (CALLER opcode)

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, VolatileDataAccess,
};
use revm::{
    bytecode::opcode::{BASEFEE, CALLER, NUMBER, POP, STOP, TIMESTAMP},
    context::{ContextTr, TxEnv},
    database::{CacheDB, EmptyDB},
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create database and EVM context
    let mut db = CacheDB::<EmptyDB>::default();
    let spec = MegaSpecId::MINI_REX;

    // Contract 1: Uses single block environment access (NUMBER opcode)
    let contract1_address = address!("0000000000000000000000000000000000100001");
    let contract1_code = vec![NUMBER, STOP]; // Read block number then stop
    set_account_code(&mut db, contract1_address, contract1_code.into());

    // Contract 2: Uses multiple block environment accesses
    let contract2_address = address!("0000000000000000000000000000000000100002");
    let contract2_code = vec![
        NUMBER, POP, // Read block number
        TIMESTAMP, POP, // Read timestamp
        BASEFEE, POP, // Read base fee
        STOP,
    ];
    set_account_code(&mut db, contract2_address, contract2_code.into());

    // Contract 3: Does NOT use block environment (CALLER opcode)
    let contract3_address = address!("0000000000000000000000000000000000100003");
    let contract3_code = vec![CALLER, STOP]; // Read caller then stop
    set_account_code(&mut db, contract3_address, contract3_code.into());

    // Create EVM instance with properly configured L1BlockInfo
    let mut context = MegaContext::new(db, spec);
    // Set operator fee fields to zero to avoid panic in MINI_REX (ISTHMUS) spec
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    let mut evm = MegaEvm::new(context);

    let caller = address!("0000000000000000000000000000000000100000");

    println!("=== Block Environment Access Tracking Demo ===\n");

    // Test Contract 1 (single block env access)
    println!("Testing Contract 1 (uses NUMBER opcode):");

    // Ensure clean state
    assert!(evm.get_block_env_accesses().is_empty(), "Should start with no block env access");
    println!("  Initial state: no block env access");

    // Create and execute transaction for contract 1
    let tx1 = MegaTransaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract1_address),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit: 1000000,
            ..Default::default()
        },
        ..Default::default()
    };

    let result1 = alloy_evm::Evm::transact_raw(&mut evm, tx1)?;
    println!("  Transaction result: {:?}", result1.result.is_success());

    let accesses1 = evm.get_block_env_accesses();
    println!("  Block env accessed: {}", !accesses1.is_empty());

    // Show detailed access information
    println!("  Access count: {}", accesses1.count_block_env_accessed());
    println!(
        "  Has accessed BlockNumber: {}",
        accesses1.contains(VolatileDataAccess::BLOCK_NUMBER)
    );

    assert_eq!(accesses1.count_block_env_accessed(), 1);
    assert!(accesses1.contains(VolatileDataAccess::BLOCK_NUMBER));
    assert!(!accesses1.is_empty(), "Contract 1 should have accessed block env");

    // Reset for next transaction
    evm.reset_volatile_data_access();
    println!("  After reset: {}\n", evm.get_block_env_accesses().is_empty());

    // Test Contract 2 (multiple block env accesses)
    println!("Testing Contract 2 (uses NUMBER, TIMESTAMP, BASEFEE opcodes):");

    assert!(evm.get_block_env_accesses().is_empty(), "Should start with no block env access");
    println!("  Initial state: no block env access");

    // Create and execute transaction for contract 2
    let tx2 = MegaTransaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract2_address),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit: 1000000,
            nonce: 0, // Reset nonce for new transaction
            ..Default::default()
        },
        ..Default::default()
    };

    let result2 = alloy_evm::Evm::transact_raw(&mut evm, tx2)?;
    println!("  Transaction result: {:?}", result2.result.is_success());

    let accesses2 = evm.get_block_env_accesses();
    println!("  Block env accessed: {}", !accesses2.is_empty());

    // Show detailed access information
    println!("  Access count: {}", accesses2.count_block_env_accessed());
    println!(
        "  Has accessed BlockNumber: {}",
        accesses2.contains(VolatileDataAccess::BLOCK_NUMBER)
    );
    println!("  Has accessed Timestamp: {}", accesses2.contains(VolatileDataAccess::TIMESTAMP));
    println!("  Has accessed BaseFee: {}", accesses2.contains(VolatileDataAccess::BASE_FEE));

    assert_eq!(accesses2.count_block_env_accessed(), 3);
    assert!(accesses2.contains(VolatileDataAccess::BLOCK_NUMBER));
    assert!(accesses2.contains(VolatileDataAccess::TIMESTAMP));
    assert!(accesses2.contains(VolatileDataAccess::BASE_FEE));
    assert!(!accesses2.is_empty(), "Contract 2 should have accessed block env");

    // Reset for next transaction
    evm.reset_volatile_data_access();
    println!("  After reset: {}\n", evm.get_block_env_accesses().is_empty());

    // Test Contract 3 (does NOT access block env)
    println!("Testing Contract 3 (uses CALLER opcode):");

    assert!(evm.get_block_env_accesses().is_empty(), "Should start with no block env access");
    println!("  Initial state: no block env access");

    // Create and execute transaction for contract 3
    let tx3 = MegaTransaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract3_address),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit: 1000000,
            nonce: 0, // Reset nonce for new transaction
            ..Default::default()
        },
        ..Default::default()
    };

    let result3 = alloy_evm::Evm::transact_raw(&mut evm, tx3)?;
    println!("  Transaction result: {:?}", result3.result.is_success());

    let accesses3 = evm.get_block_env_accesses();
    println!("  Block env accessed: {}", !accesses3.is_empty());

    println!("  Access count: {}", accesses3.count_block_env_accessed());

    assert_eq!(accesses3.count_block_env_accessed(), 0);
    assert!(accesses3.is_empty(), "Contract 3 should NOT have accessed block env");

    println!("  Demo finished");

    Ok(())
}

/// Helper function to set account code
fn set_account_code(db: &mut CacheDB<EmptyDB>, address: alloy_primitives::Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = AccountInfo { code: Some(bytecode), code_hash, ..Default::default() };
    db.insert_account_info(address, account_info);
}
