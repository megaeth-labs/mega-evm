//! Tests for inspector integration with `MegaBlockExecutor`.
//!
//! These tests verify that inspectors work correctly when executing transactions
//! using `MegaBlockExecutor`.

use std::convert::Infallible;

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_op_hardforks::OpChainHardforks;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, GasInspector, MemoryDatabase},
    BlockLimits, TestExternalEnvs, MegaBlockExecutionCtx, MegaBlockExecutorFactory,
    MegaEvmFactory, MegaSpecId, MegaTxEnvelope,
};
use revm::{
    bytecode::opcode::{ADD, PUSH0, SLOAD, SSTORE},
    context::BlockEnv,
    database::State,
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");

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

/// Creates a contract that performs SSTORE operations.
fn create_test_contract() -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // key (slot 0)
        .append(SLOAD) // load value from slot 0
        .push_number(1u8) // push 1 to increment
        .append(ADD) // add 1 to loaded value
        .append(PUSH0) // key for SSTORE
        .append(SSTORE) // store incremented value
        .stop()
        .build()
}

#[test]
fn test_inspector_works_with_block_executor() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_test_contract();
    db.set_account_code(CONTRACT, bytecode);
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory and block executor factory
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new(external_envs);
    let chain_spec = OpChainHardforks::base_mainnet();
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

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

    // Create block context
    let block_ctx =
        MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits());

    // Create inspector
    let inspector = GasInspector::new();

    // Create block executor with inspector
    let mut executor = block_executor_factory
        .create_executor_with_inspector(&mut state, block_ctx, evm_env, inspector);

    // Execute transaction
    let tx = create_transaction(0, 1_000_000);
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "Transaction should succeed: {:?}", result.err());

    // Get inspector records via evm
    let records = executor.evm().inspector.records();

    // Verify that inspector recorded opcodes
    assert!(!records.is_empty(), "Inspector should have recorded opcodes");

    // Verify that we recorded the expected opcodes
    let opcodes: Vec<_> = records.iter().map(|r| r.opcode.as_str()).collect();
    assert!(opcodes.contains(&"SLOAD"), "Should record SLOAD opcode");
    assert!(opcodes.contains(&"ADD"), "Should record ADD opcode");
    assert!(opcodes.contains(&"SSTORE"), "Should record SSTORE opcode");

    // Verify that gas costs are being tracked
    for record in &records {
        assert!(
            record.gas_before >= record.gas_after,
            "Gas should decrease or stay the same after opcode execution"
        );
    }

    // Finish the block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 1, "Should have 1 receipt");
}
