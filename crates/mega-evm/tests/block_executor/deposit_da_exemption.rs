//! Tests for deposit transaction DA size limit exemption.
//!
//! These tests verify that deposit transactions are exempt from DA size limit checks
//! while regular transactions are still subject to DA size limits.

use std::convert::Infallible;

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor,
    MegaEvmFactory, MegaHardforkConfig, MegaSpecId, MegaTransactionExt, MegaTxEnvelope,
    TestExternalEnvs,
};
use op_alloy_consensus::TxDeposit;
use revm::{context::BlockEnv, database::State};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");
const DA_SIZE_LIMIT: u64 = 100; // Small DA limit for testing

/// Helper function to create a regular (non-deposit) transaction with large calldata.
fn create_regular_transaction_with_large_calldata(
    nonce: u64,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    // Create calldata with non-compressible data (sequential bytes) to ensure it exceeds
    // DA_SIZE_LIMIT
    let large_calldata =
        Bytes::from((0..(DA_SIZE_LIMIT * 1000)).map(|_| rand::random::<u8>()).collect::<Vec<u8>>());

    let tx_legacy = TxLegacy {
        chain_id: Some(8453), // Base mainnet
        nonce,
        gas_price: 1_000_000,
        gas_limit: 1_000_000,
        to: TxKind::Call(CONTRACT),
        value: U256::ZERO,
        input: large_calldata,
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Helper function to create a deposit transaction with large calldata.
fn create_deposit_transaction_with_large_calldata(
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    // Create calldata with non-compressible data (sequential bytes) to ensure it exceeds
    // DA_SIZE_LIMIT
    let large_calldata =
        Bytes::from((0..(DA_SIZE_LIMIT * 1000)).map(|_| rand::random::<u8>()).collect::<Vec<u8>>());

    let deposit_tx = TxDeposit {
        source_hash: B256::ZERO,
        from: CALLER,
        to: TxKind::Call(CONTRACT),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: false,
        input: large_calldata,
    };
    let tx =
        MegaTxEnvelope::Deposit(alloy_consensus::Sealed::new_unchecked(deposit_tx, B256::ZERO));
    // Deposit transactions don't have a recoverable signature, so we use CALLER as the signer
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Helper function to create a small regular transaction.
fn create_small_regular_transaction(
    nonce: u64,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit: 100_000,
        to: TxKind::Call(CONTRACT),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

#[test]
fn test_deposit_transaction_exempt_from_single_tx_da_limit() {
    // Create database with empty contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, Bytes::new());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

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

    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with strict DA size limits
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits()
            .with_block_gas_limit(30_000_000)
            .with_tx_da_size_limit(DA_SIZE_LIMIT)
            .with_block_da_size_limit(DA_SIZE_LIMIT * 10),
    );

    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute deposit transaction with large calldata - should succeed
    let deposit_tx = create_deposit_transaction_with_large_calldata();
    assert!(deposit_tx.estimated_da_size() > DA_SIZE_LIMIT * 10);

    let result = executor.execute_transaction(&deposit_tx);
    assert!(
        result.is_ok(),
        "Deposit transaction should succeed despite exceeding tx_da_size_limit, got error: {:?}",
        result.err()
    );

    // Finish block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 1, "Should have 1 receipt for deposit transaction");
}

#[test]
fn test_regular_transaction_rejected_by_single_tx_da_limit() {
    // Create database with empty contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, Bytes::new());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

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

    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with strict DA size limits
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits()
            .with_block_gas_limit(30_000_000)
            .with_tx_da_size_limit(DA_SIZE_LIMIT)
            .with_block_da_size_limit(DA_SIZE_LIMIT * 10),
    );

    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute regular transaction with large calldata - should fail
    let regular_tx = create_regular_transaction_with_large_calldata(0);
    assert!(regular_tx.estimated_da_size() > DA_SIZE_LIMIT);

    let result = executor.execute_transaction(&regular_tx);
    assert!(
        result.is_err(),
        "Regular transaction should be rejected for exceeding tx_da_size_limit"
    );

    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("DataAvailabilitySizeLimit"),
        "Error should mention DataAvailabilitySizeLimit, got: {}",
        err_msg
    );
}

#[test]
fn test_deposit_exempt_from_block_da_limit() {
    // Create database with empty contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, Bytes::new());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

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

    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with strict block DA size limit
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits()
            .with_block_gas_limit(30_000_000)
            .with_tx_da_size_limit(DA_SIZE_LIMIT * 10) // High single tx limit
            .with_block_da_size_limit(DA_SIZE_LIMIT), // Low block limit
    );

    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Execute deposit transaction with large calldata - should succeed even if block DA is exceeded
    let deposit_tx = create_deposit_transaction_with_large_calldata();
    assert!(deposit_tx.estimated_da_size() > DA_SIZE_LIMIT);

    let result = executor.execute_transaction(&deposit_tx);
    assert!(
        result.is_ok(),
        "Deposit transaction should succeed despite exceeding block_da_size_limit, got error: {:?}",
        result.err()
    );

    // Finish block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 1, "Should have 1 receipt for deposit transaction");
}

#[test]
fn test_mixed_deposit_and_regular_transactions() {
    // Create database with empty contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, Bytes::new());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

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

    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block context with moderate DA size limits
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits()
            .with_block_gas_limit(30_000_000)
            .with_tx_da_size_limit(DA_SIZE_LIMIT)
            .with_block_da_size_limit(DA_SIZE_LIMIT * 10),
    );

    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // 1. Execute small regular transaction - should succeed
    let small_tx = create_small_regular_transaction(0);
    let result1 = executor.execute_transaction(&small_tx);
    assert!(result1.is_ok(), "Small regular transaction should succeed");

    // 2. Execute large deposit transaction - should succeed (exempt from DA limits)
    let deposit_tx = create_deposit_transaction_with_large_calldata();
    let result2 = executor.execute_transaction(&deposit_tx);
    assert!(result2.is_ok(), "Deposit transaction should succeed despite large DA size");

    // 3. Execute large regular transaction - should fail (exceeds tx DA limit)
    let large_tx = create_regular_transaction_with_large_calldata(1);
    let result3 = executor.execute_transaction(&large_tx);
    assert!(result3.is_err(), "Large regular transaction should fail due to DA size limit");

    // Finish block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 2, "Should have 2 receipts (small regular + deposit)");
}
