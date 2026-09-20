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
    BlockLimits, EnrichedMegaTx, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory,
    MegaHardforkConfig, MegaSpecId, MegaTransactionExt, MegaTxEnvelope, TestExternalEnvs,
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

const CALLER2: alloy_primitives::Address = address!("3000000000000000000000000000000000000003");
const CALLER3: alloy_primitives::Address = address!("4000000000000000000000000000000000000004");
