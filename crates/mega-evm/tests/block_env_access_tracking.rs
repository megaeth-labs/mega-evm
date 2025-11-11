//! Tests for the block environment access tracking functionality.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{test_utils::MemoryDatabase, *};
use revm::{
    bytecode::opcode::{
        BASEFEE, BLOCKHASH, CALLER, CHAINID, COINBASE, DIFFICULTY, GASLIMIT, GASPRICE, NUMBER,
        ORIGIN, POP, PUSH1, STOP, TIMESTAMP,
    },
    context::ContextTr,
};

/// Test that verifies the EVM correctly tracks block environment access for various opcodes.
/// This comprehensive test checks that the system properly detects and tracks access to different
/// block environment values (NUMBER, TIMESTAMP, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE, BLOCKHASH)
/// while correctly ignoring non-block environment opcodes (CALLER, ORIGIN, GASPRICE, CHAINID).
/// It also verifies the reset functionality works correctly.
#[test]
fn test_block_env_tracking_with_evm() {
    let test_cases = vec![
        ("NUMBER", vec![NUMBER, STOP], Some(VolatileDataAccess::BLOCK_NUMBER)),
        ("TIMESTAMP", vec![TIMESTAMP, STOP], Some(VolatileDataAccess::TIMESTAMP)),
        ("COINBASE", vec![COINBASE, STOP], Some(VolatileDataAccess::COINBASE)),
        ("DIFFICULTY", vec![DIFFICULTY, STOP], Some(VolatileDataAccess::PREV_RANDAO)),
        ("GASLIMIT", vec![GASLIMIT, STOP], Some(VolatileDataAccess::GAS_LIMIT)),
        ("BASEFEE", vec![BASEFEE, STOP], Some(VolatileDataAccess::BASE_FEE)),
        ("BLOCKHASH", vec![PUSH1, 0x05, BLOCKHASH, STOP], Some(VolatileDataAccess::BLOCK_HASH)),
        // Non-block env opcodes
        ("CALLER", vec![CALLER, STOP], None),
        ("ORIGIN", vec![ORIGIN, STOP], None),
        ("GASPRICE", vec![GASPRICE, STOP], None),
        ("CHAINID", vec![CHAINID, STOP], None),
    ];

    for (opcode_name, bytecode, expected_access_type) in test_cases {
        println!("Testing opcode: {}", opcode_name);

        let mut db = MemoryDatabase::default();
        let contract_address = address!("0000000000000000000000000000000000100001");
        db.set_account_code(contract_address, bytecode.into());

        let mut context =
            MegaContext::new(db, MegaSpecId::MINI_REX, DefaultExternalEnvs::default());
        // Configure L1BlockInfo to avoid operator fee scalar panic
        context.chain_mut().operator_fee_scalar = Some(U256::from(0));
        context.chain_mut().operator_fee_constant = Some(U256::from(0));

        // Set block number to allow BLOCKHASH to work properly
        use revm::context::{BlockEnv, ContextSetters};
        let block_env = BlockEnv { number: U256::from(10), ..Default::default() };
        context.set_block(block_env);

        let mut evm = MegaEvm::new_with_accelerated_precompiles(context, None);

        // Ensure we start with no block env access
        assert!(
            evm.get_block_env_accesses().is_empty(),
            "EVM should start with no block env access for {}",
            opcode_name
        );
        assert_eq!(
            evm.get_block_env_accesses().count_block_env_accessed(),
            0,
            "EVM should start with no access count for {}",
            opcode_name
        );

        // Create transaction
        let tx = MegaTransaction {
            base: revm::context::TxEnv {
                caller: address!("0000000000000000000000000000000000100000"),
                kind: revm::primitives::TxKind::Call(contract_address),
                data: Bytes::default(),
                value: U256::ZERO,
                gas_limit: 1000000000000000000u64,
                ..Default::default()
            },
            ..Default::default()
        };

        // Execute transaction
        let result = alloy_evm::Evm::transact_raw(&mut evm, tx);
        assert!(result.is_ok(), "Transaction should succeed for {}", opcode_name);

        // Check if block env was accessed
        match expected_access_type {
            Some(access_type) => {
                assert!(
                    !evm.get_block_env_accesses().is_empty(),
                    "Transaction should have accessed block env for {}",
                    opcode_name
                );
                assert!(
                    evm.get_block_env_accesses().contains(access_type),
                    "Transaction should have accessed {:?} for {}",
                    access_type,
                    opcode_name
                );

                let accesses = evm.get_block_env_accesses();
                if opcode_name == "BLOCKHASH" {
                    // BLOCKHASH accesses both BlockNumber (to validate range) and BlockHash
                    assert_eq!(
                        accesses.count_block_env_accessed(),
                        2,
                        "BLOCKHASH should access exactly two items: BlockNumber and BlockHash"
                    );
                    assert!(
                        accesses.contains(VolatileDataAccess::BLOCK_NUMBER),
                        "BLOCKHASH should access BlockNumber for validation"
                    );
                    assert!(accesses.contains(access_type), "BLOCKHASH should access BlockHash");
                } else {
                    assert_eq!(
                        accesses.count_block_env_accessed(),
                        1,
                        "Should have exactly one access for {}",
                        opcode_name
                    );
                    assert!(
                        accesses.contains(access_type),
                        "Should have accessed {:?} for {}",
                        access_type,
                        opcode_name
                    );
                }
            }
            None => {
                assert!(
                    evm.get_block_env_accesses().is_empty(),
                    "Transaction should NOT have accessed block env for {}",
                    opcode_name
                );
                assert_eq!(
                    evm.get_block_env_accesses().count_block_env_accessed(),
                    0,
                    "Access count should be zero for {}",
                    opcode_name
                );
            }
        }

        // Test reset functionality
        evm.reset_volatile_data_access();
        assert!(
            evm.get_block_env_accesses().is_empty(),
            "Block env access should be reset for {}",
            opcode_name
        );
        assert_eq!(
            evm.get_block_env_accesses().count_block_env_accessed(),
            0,
            "Access count should be zero after reset for {}",
            opcode_name
        );
    }
}

/// Test that verifies the EVM correctly tracks multiple block environment accesses within a single
/// transaction. This test ensures that when a contract accesses multiple different block
/// environment values (NUMBER, TIMESTAMP, BASEFEE) in one execution, the system correctly tracks
/// all of them and maintains an accurate count of accessed block environment types.
#[test]
fn test_multiple_block_env_accesses() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");

    // Contract that accesses multiple block env values: NUMBER, TIMESTAMP, BASEFEE
    let contract_code = vec![
        NUMBER, POP, // Access block number, then pop
        TIMESTAMP, POP, // Access timestamp, then pop
        BASEFEE, POP, // Access basefee, then pop
        STOP,
    ];
    db.set_account_code(contract_address, contract_code.into());

    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, DefaultExternalEnvs::default());
    // Configure L1BlockInfo to avoid operator fee scalar panic
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    let mut evm = MegaEvm::new_with_accelerated_precompiles(context, None);

    // Execute transaction
    let tx = MegaTransaction {
        base: revm::context::TxEnv {
            caller: address!("0000000000000000000000000000000000100000"),
            kind: revm::primitives::TxKind::Call(contract_address),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit: 1000000000000000000u64,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(result.is_ok());

    // Should have accessed 3 different types
    let accesses = evm.get_block_env_accesses();
    assert!(!accesses.is_empty());
    assert_eq!(
        accesses.count_block_env_accessed(),
        3,
        "Should have accessed 3 different block env types"
    );

    // Check specific accesses
    assert!(accesses.contains(VolatileDataAccess::BLOCK_NUMBER));
    assert!(accesses.contains(VolatileDataAccess::TIMESTAMP));
    assert!(accesses.contains(VolatileDataAccess::BASE_FEE));
}

/// Test that verifies the block environment access tracking is properly reset between transactions.
/// This test ensures that when a new transaction is executed, the block environment access tracking
/// is correctly reset and can track new accesses for the subsequent transaction, maintaining proper
/// isolation between different transaction executions.
#[test]
fn test_block_env_reset_between_transactions() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    db.set_account_code(contract_address, vec![NUMBER, STOP].into());

    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, DefaultExternalEnvs::default());
    // Configure L1BlockInfo to avoid operator fee scalar panic
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    let mut evm = MegaEvm::new_with_accelerated_precompiles(context, None);

    // First transaction - accesses block env
    let tx1 = MegaTransaction {
        base: revm::context::TxEnv {
            caller: address!("0000000000000000000000000000000000100000"),
            kind: revm::primitives::TxKind::Call(contract_address),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit: 1000000000000000000u64,
            ..Default::default()
        },
        ..Default::default()
    };

    let result1 = alloy_evm::Evm::transact_raw(&mut evm, tx1);
    assert!(result1.is_ok());
    assert!(!evm.get_block_env_accesses().is_empty(), "First transaction should access block env");

    // Simulate setting up for a new transaction
    let tx2_env = revm::context::TxEnv {
        caller: address!("0000000000000000000000000000000000100000"),
        kind: revm::primitives::TxKind::Call(contract_address),
        data: Bytes::default(),
        value: U256::ZERO,
        gas_limit: 1000000000000000000u64,
        nonce: 0, // Same account should continue with correct nonce
        ..Default::default()
    };
    let tx2 = MegaTransaction { base: tx2_env, ..Default::default() };

    // This simulates how the EVM would be used for a new transaction
    // Setting tx should automatically reset the block env access
    use revm::ExecuteEvm;
    evm.transact_one(tx2).unwrap();

    // After setting new transaction, flag should be reset automatically
    // But since we executed the same contract (NUMBER opcode), it should be accessed again
    assert!(
        !evm.get_block_env_accesses().is_empty(),
        "Block env should be accessed again for second transaction"
    );
}
