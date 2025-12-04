//! Tests for block-level limit enforcement in `MegaBlockExecutor`.
//!
//! These tests verify that the block executor properly enforces block-level data
//! and KV-update limits across multiple transactions within a block.

use std::convert::Infallible;

use alloy_consensus::{Signed, Transaction, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_evm::{block::BlockExecutor, Evm, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{ADD, DUP1, LOG0, PUSH0, SLOAD, SSTORE},
    context::BlockEnv,
    database::{Database, State},
};

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to create a recovered transaction.
fn create_transaction(
    nonce: u64,
    gas_limit: u64,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453), // Base mainnet
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Call(CONTRACT),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Creates a contract that generates a log with specified data size.
///
/// The contract will emit LOG0 with the specified number of zero bytes.
fn create_log_generating_contract(data_size: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Push data size
    if data_size <= 0xFF {
        builder = builder.push_number(data_size as u8);
    } else if data_size <= 0xFFFF {
        builder = builder.push_number(data_size as u16);
    } else {
        builder = builder.push_number(data_size as u32);
    }

    // Push memory offset (0)
    builder = builder.append(PUSH0);

    // LOG0(offset, size)
    builder = builder.append(LOG0);

    // Stop
    builder.stop().build()
}

/// Creates a contract that performs N SSTORE operations.
///
/// Each SSTORE loads from storage slot i, increments it, and stores it back.
fn create_sstore_contract(num_writes: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    for i in 1..=num_writes {
        // Push key (slot number) for SLOAD
        if i <= 0xFF {
            builder = builder.push_number(i as u8);
        } else if i <= 0xFFFF {
            builder = builder.push_number(i as u16);
        } else {
            builder = builder.push_number(i as u32);
        }

        // Duplicate key on stack for SSTORE later
        // DUP1 duplicates the top stack item
        builder = builder.append(DUP1);

        // SLOAD - load current value from slot
        // Stack: [key, value]
        builder = builder.append(SLOAD);

        // Push 1 to increment
        // Stack: [key, value, 1]
        builder = builder.push_number(1u8);

        // ADD - increment the loaded value
        // Stack: [key, incremented_value]
        builder = builder.append(ADD);

        // SSTORE - store incremented value back to slot
        // Stack: []
        builder = builder.append(SSTORE);
    }

    builder.stop().build()
}

#[test]
fn test_block_custom_data_limit() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_log_generating_contract(2000); // 2 KB log
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with VERY LOW custom data limit
    // New behavior: The first transaction that exceeds the limit is allowed,
    // but subsequent transactions are rejected in pre-execution
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_txs_data_limit(2_500),
    ); // 2.5 KB data limit

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute first transaction (should succeed and likely exceed the limit)
    let tx1 = create_transaction(0, 1_000_000);
    let result1 = executor.execute_transaction(&tx1);
    assert!(result1.is_ok(), "First transaction should succeed");
    assert!(result1.unwrap() < tx1.gas_limit(), "Gas used should be less than gas limit");

    // Execute second transaction (should succeed, causing block to exceed limit)
    let tx2 = create_transaction(1, 1_000_000);
    let result2 = executor.execute_transaction(&tx2);
    assert!(result2.is_ok(), "Second transaction should succeed (last tx can exceed limit)");
    assert!(result2.unwrap() < tx2.gas_limit(), "Gas used should be less than gas limit");

    // Execute third transaction (should fail due to block data limit already exceeded)
    let tx3 = create_transaction(2, 1_000_000);
    let result3 = executor.execute_transaction(&tx3);
    assert!(result3.is_err(), "Third transaction should fail due to block data limit");
    let err_msg = format!("{:?}", result3.unwrap_err());
    assert!(
        err_msg.contains("TransactionDataLimit"),
        "Error should mention TransactionDataLimit, got: {}",
        err_msg
    );
}

#[test]
fn test_block_custom_kv_update_limit() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_sstore_contract(50); // 50 storage writes
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with custom KV update limit
    // Set limit to 1 to test the new behavior where the first transaction that
    // exceeds the limit is allowed, but subsequent transactions are rejected
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_kv_update_limit(1),
    ); // Very low limit - first tx will exceed it

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute first transaction (should succeed even though it will exceed the limit)
    let result1 = executor.execute_transaction(&create_transaction(0, 10_000_000));
    assert!(result1.is_ok(), "First transaction should succeed (last tx can exceed limit)");

    // Execute second transaction (should fail due to KV limit already exceeded)
    let result2 = executor.execute_transaction(&create_transaction(1, 10_000_000));
    assert!(result2.is_err(), "Second transaction should fail due to block KV update limit");
    let err_msg = format!("{:?}", result2.unwrap_err());
    assert!(
        err_msg.contains("KVUpdateLimit"),
        "Error should mention KVUpdateLimit, got: {}",
        err_msg
    );
}

#[test]
fn test_block_multiple_transactions_within_limits() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_log_generating_contract(100); // 100 bytes per tx
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with reasonable limits
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits()
            .with_block_txs_data_limit(10_000)
            .with_block_kv_update_limit(1_000),
    ); // 10 KB data limit and 1000 KV update limit

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute 5 transactions, all should succeed
    for nonce in 0..5 {
        let tx = create_transaction(nonce, 1_000_000);
        let result = executor.execute_transaction(&tx);
        assert!(result.is_ok(), "Transaction {} should succeed", nonce);
        assert!(result.unwrap() < tx.gas_limit(), "Gas used should be less than gas limit");
    }

    // Finish the block and get receipts
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 5, "Should have 5 receipts");
}

#[test]
fn test_block_data_limit_exceeded_mid_block() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_log_generating_contract(2000);
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with ~6 KB limit
    // The transaction that causes the block to exceed the limit is allowed,
    // but the next transaction is rejected
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_txs_data_limit(6_000),
    ); // 6 KB data limit

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute first transaction (should succeed)
    let result1 = executor.execute_transaction(&create_transaction(0, 1_000_000));
    assert!(result1.is_ok(), "First transaction should succeed");

    // Execute second transaction (should succeed)
    let result2 = executor.execute_transaction(&create_transaction(1, 1_000_000));
    assert!(result2.is_ok(), "Second transaction should succeed");

    // Execute third transaction (should succeed, likely causing block to exceed limit)
    let result3 = executor.execute_transaction(&create_transaction(2, 1_000_000));
    assert!(result3.is_ok(), "Third transaction should succeed (last tx can exceed limit)");

    // Execute fourth transaction (should fail due to block data limit already exceeded)
    let result4 = executor.execute_transaction(&create_transaction(3, 1_000_000));
    assert!(result4.is_err(), "Fourth transaction should fail due to block data limit");

    // Finish the block - should have 3 receipts
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 3, "Should have 3 receipts (4th tx failed)");
}

#[test]
fn test_block_kv_limit_exceeded_mid_block() {
    // Create database and deploy contract with minimal SSTORE operations
    let mut db = MemoryDatabase::default();
    let bytecode = create_sstore_contract(1); // Just 1 SSTORE
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with limit of 1 KV update
    // New behavior: The transaction that causes the block to exceed the limit is allowed,
    // but the next transaction is rejected
    // Each transaction induces 2 KV updates (sender account info and storage slot)
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_kv_update_limit(1),
    ); // 1 KV update limit - first tx will exceed it

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute first transaction (should succeed with 2 KV updates, exceeding the limit)
    let tx1 = create_transaction(0, 10_000_000);
    let result1 = executor.execute_transaction(&tx1);
    assert!(result1.is_ok(), "First transaction should succeed (last tx can exceed limit)");
    assert!(result1.unwrap() < tx1.gas_limit(), "Gas used should be less than gas limit");

    // Execute second transaction (should fail due to KV limit already exceeded)
    let tx2 = create_transaction(1, 10_000_000);
    let result2 = executor.execute_transaction(&tx2);
    assert!(result2.is_err(), "Second transaction should fail due to block KV update limit");

    // Finish the block - should have 1 receipt
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 1, "Should have 1 receipt (2nd tx failed)");
}

#[test]
fn test_block_no_state_commit_on_limit_exceeded() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(42u8) // value
        .append(PUSH0) // key (slot 0)
        .append(SSTORE) // SSTORE
        .stop()
        .build();
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with VERY low KV update limit
    // With new behavior, we need to set limit to 0 to ensure no transactions can be added
    // Setting to 0 means block_kv_updates_used (0) >= limit (0) is true from the start
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_kv_update_limit(0),
    ); // Zero limit

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute transaction (should fail due to KV limit of 0)
    let result = executor.execute_transaction(&create_transaction(0, 10_000_000));
    assert!(result.is_err(), "Transaction should fail due to block KV update limit of 0");

    // Verify storage slot 0 is NOT set in the database
    let db_state = executor.evm_mut().db_mut();
    let _ = db_state.load_cache_account(CONTRACT);
    let value = db_state.storage(CONTRACT, U256::ZERO).expect("Storage access should not fail");

    // The storage should be zero (not 42) since the transaction was not committed
    assert_eq!(value, U256::ZERO, "Storage should not be committed");

    // Finish the block - should have 0 receipts
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 0, "Should have 0 receipts (tx rejected in pre-execution)");
}

#[test]
fn test_block_tx_size_limit_default_unlimited() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_log_generating_contract(100);
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with default limits (tx size should be u64::MAX)
    let block_ctx =
        MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits());
    assert_eq!(
        block_ctx.block_limits.block_txs_encode_size_limit,
        u64::MAX,
        "Default tx size limit should be u64::MAX"
    );

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute multiple transactions - should all succeed with unlimited size
    for nonce in 0..10 {
        let tx = create_transaction(nonce, 1_000_000);
        let result = executor.execute_transaction(&tx);
        assert!(
            result.is_ok(),
            "Transaction {} should succeed with unlimited tx size limit",
            nonce
        );
    }

    // Finish the block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 10, "Should have 10 receipts");
}

#[test]
fn test_block_tx_size_limit_allows_multiple_transactions() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_log_generating_contract(100);
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create a transaction to measure its size
    let sample_tx = create_transaction(0, 1_000_000);
    let tx_size = sample_tx.encode_2718_len() as u64;

    // Set limit to allow exactly 5 transactions
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_txs_encode_size_limit(tx_size * 5),
    );

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute 5 transactions - should all succeed
    for nonce in 0..5 {
        let tx = create_transaction(nonce, 1_000_000);
        let result = executor.execute_transaction(&tx);
        assert!(result.is_ok(), "Transaction {} should succeed", nonce);
    }

    // Finish the block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 5, "Should have 5 receipts");
}

#[test]
fn test_block_tx_size_limit_exceeded_first_transaction() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_log_generating_contract(100);
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Set a very small tx size limit that won't fit even one transaction
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_txs_encode_size_limit(10),
    ); // Very small limit

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute transaction (should fail immediately due to tx size limit)
    let result = executor.execute_transaction(&create_transaction(0, 1_000_000));
    assert!(result.is_err(), "Transaction should fail due to tx size limit");

    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("TransactionEncodeSizeLimit"),
        "Error should mention TransactionEncodeSizeLimit, got: {}",
        err_msg
    );

    // Finish the block - should have 0 receipts
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 0, "Should have 0 receipts (tx failed)");
}

#[test]
fn test_block_tx_size_limit_exceeded_mid_block() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_log_generating_contract(100);
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create a transaction to measure its size
    let sample_tx = create_transaction(0, 1_000_000);
    let tx_size = sample_tx.encode_2718_len() as u64;

    // Set limit to allow exactly 3 transactions
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_txs_encode_size_limit(tx_size * 3),
    );

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute first 3 transactions (should all succeed)
    for nonce in 0..3 {
        let tx = create_transaction(nonce, 1_000_000);
        let result = executor.execute_transaction(&tx);
        assert!(result.is_ok(), "Transaction {} should succeed", nonce);
    }

    // Execute 4th transaction (should fail due to tx size limit)
    let result = executor.execute_transaction(&create_transaction(3, 1_000_000));
    assert!(result.is_err(), "Fourth transaction should fail due to tx size limit");

    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("TransactionEncodeSizeLimit"),
        "Error should mention TransactionEncodeSizeLimit, got: {}",
        err_msg
    );

    // Finish the block - should have 3 receipts
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 3, "Should have 3 receipts (4th tx failed)");
}

#[test]
fn test_block_tx_size_limit_with_varying_sizes() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_log_generating_contract(100);
    db.set_account_code(CONTRACT, bytecode);
    // Fund the caller account
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Create state
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create EVM
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Measure transaction sizes with different gas limits
    let small_tx = create_transaction(0, 100_000);
    let large_tx = create_transaction(0, 10_000_000);
    let small_size = small_tx.encode_2718_len() as u64;
    let large_size = large_tx.encode_2718_len() as u64;

    // Set limit to allow 2 small + 1 large transaction
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_txs_encode_size_limit(small_size * 2 + large_size),
    );

    // Create block executor with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute 2 small transactions
    let result1 = executor.execute_transaction(&create_transaction(0, 100_000));
    assert!(result1.is_ok(), "First small transaction should succeed");

    let result2 = executor.execute_transaction(&create_transaction(1, 100_000));
    assert!(result2.is_ok(), "Second small transaction should succeed");

    // Execute 1 large transaction
    let result3 = executor.execute_transaction(&create_transaction(2, 10_000_000));
    assert!(result3.is_ok(), "Large transaction should succeed");

    // Try to execute one more small transaction (should fail)
    let result4 = executor.execute_transaction(&create_transaction(3, 100_000));
    assert!(result4.is_err(), "Fourth transaction should fail due to tx size limit");

    // Finish the block - should have 3 receipts
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 3, "Should have 3 receipts (4th tx failed)");
}
