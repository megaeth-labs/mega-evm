//! Tests for the block environment access tracking functionality.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{test_utils::*, *};
use revm::{
    bytecode::opcode::{
        BASEFEE, BLOCKHASH, CALLER, CHAINID, COINBASE, DIFFICULTY, GASLIMIT, GASPRICE, NUMBER,
        ORIGIN, POP, PUSH1, STOP, TIMESTAMP,
    },
    context::ContextTr,
    database::{CacheDB, EmptyDB},
    inspector::NoOpInspector,
};

/// Test that EVM correctly tracks block environment access
#[test]
fn test_block_env_tracking_with_evm() {
    let test_cases = vec![
        ("NUMBER", vec![NUMBER, STOP], Some(BlockEnvAccess::BLOCK_NUMBER)),
        ("TIMESTAMP", vec![TIMESTAMP, STOP], Some(BlockEnvAccess::TIMESTAMP)),
        ("COINBASE", vec![COINBASE, STOP], Some(BlockEnvAccess::COINBASE)),
        ("DIFFICULTY", vec![DIFFICULTY, STOP], Some(BlockEnvAccess::PREV_RANDAO)),
        ("GASLIMIT", vec![GASLIMIT, STOP], Some(BlockEnvAccess::GAS_LIMIT)),
        ("BASEFEE", vec![BASEFEE, STOP], Some(BlockEnvAccess::BASE_FEE)),
        ("BLOCKHASH", vec![PUSH1, 0x05, BLOCKHASH, STOP], Some(BlockEnvAccess::BLOCK_HASH)),
        // Non-block env opcodes
        ("CALLER", vec![CALLER, STOP], None),
        ("ORIGIN", vec![ORIGIN, STOP], None),
        ("GASPRICE", vec![GASPRICE, STOP], None),
        ("CHAINID", vec![CHAINID, STOP], None),
    ];

    for (opcode_name, bytecode, expected_access_type) in test_cases {
        println!("Testing opcode: {}", opcode_name);

        let mut db = CacheDB::<EmptyDB>::default();
        let contract_address = address!("0000000000000000000000000000000000100001");
        set_account_code(&mut db, contract_address, bytecode.into());

        let mut context = Context::new(db, SpecId::MINI_REX, NoOpOracle);
        // Configure L1BlockInfo to avoid operator fee scalar panic
        context.chain_mut().operator_fee_scalar = Some(U256::from(0));
        context.chain_mut().operator_fee_constant = Some(U256::from(0));

        // Set block number to allow BLOCKHASH to work properly
        use revm::context::{BlockEnv, ContextSetters};
        let block_env = BlockEnv { number: U256::from(10), ..Default::default() };
        context.set_block(block_env);

        let mut evm = Evm::new(context, NoOpInspector);

        // Ensure we start with no block env access
        assert!(
            evm.get_block_env_accesses().is_empty(),
            "EVM should start with no block env access for {}",
            opcode_name
        );
        assert_eq!(
            evm.get_block_env_accesses().count_accessed(),
            0,
            "EVM should start with no access count for {}",
            opcode_name
        );

        // Create transaction
        let tx = Transaction {
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
                        accesses.count_accessed(),
                        2,
                        "BLOCKHASH should access exactly two items: BlockNumber and BlockHash"
                    );
                    assert!(
                        accesses.contains(BlockEnvAccess::BLOCK_NUMBER),
                        "BLOCKHASH should access BlockNumber for validation"
                    );
                    assert!(accesses.contains(access_type), "BLOCKHASH should access BlockHash");
                } else {
                    assert_eq!(
                        accesses.count_accessed(),
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
                    evm.get_block_env_accesses().count_accessed(),
                    0,
                    "Access count should be zero for {}",
                    opcode_name
                );
            }
        }

        // Test reset functionality
        evm.reset_block_env_access();
        assert!(
            evm.get_block_env_accesses().is_empty(),
            "Block env access should be reset for {}",
            opcode_name
        );
        assert_eq!(
            evm.get_block_env_accesses().count_accessed(),
            0,
            "Access count should be zero after reset for {}",
            opcode_name
        );
    }
}

/// Test multiple block env accesses in a single transaction
#[test]
fn test_multiple_block_env_accesses() {
    let mut db = CacheDB::<EmptyDB>::default();
    let contract_address = address!("0000000000000000000000000000000000100001");

    // Contract that accesses multiple block env values: NUMBER, TIMESTAMP, BASEFEE
    let contract_code = vec![
        NUMBER, POP, // Access block number, then pop
        TIMESTAMP, POP, // Access timestamp, then pop
        BASEFEE, POP, // Access basefee, then pop
        STOP,
    ];
    set_account_code(&mut db, contract_address, contract_code.into());

    let mut context = Context::new(db, SpecId::MINI_REX, NoOpOracle);
    // Configure L1BlockInfo to avoid operator fee scalar panic
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    let mut evm = Evm::new(context, NoOpInspector);

    // Execute transaction
    let tx = Transaction {
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
    assert_eq!(accesses.count_accessed(), 3, "Should have accessed 3 different block env types");

    // Check specific accesses
    assert!(accesses.contains(BlockEnvAccess::BLOCK_NUMBER));
    assert!(accesses.contains(BlockEnvAccess::TIMESTAMP));
    assert!(accesses.contains(BlockEnvAccess::BASE_FEE));
}

/// Test that access list is reset between transactions
#[test]
fn test_block_env_reset_between_transactions() {
    let mut db = CacheDB::<EmptyDB>::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    set_account_code(&mut db, contract_address, vec![NUMBER, STOP].into());

    let mut context = Context::new(db, SpecId::MINI_REX, NoOpOracle);
    // Configure L1BlockInfo to avoid operator fee scalar panic
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    let mut evm = Evm::new(context, NoOpInspector);

    // First transaction - accesses block env
    let tx1 = Transaction {
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
    let tx2 = Transaction { base: tx2_env, ..Default::default() };

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
