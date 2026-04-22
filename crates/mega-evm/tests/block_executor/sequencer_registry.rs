//! End-to-end tests for `SequencerRegistry` and dynamic system address.
//!
//! These tests verify the full pipeline: `apply_pre_execution_changes()` deploys
//! `SequencerRegistry`, resolves system address, and then transaction execution
//! correctly uses the resolved system address for system tx identification.

use std::convert::Infallible;

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, IOracle, MegaBlockExecutionCtx,
    MegaBlockExecutorFactory, MegaEvmFactory, MegaHardfork, MegaHardforkConfig, MegaSpecId,
    MegaTxEnvelope, SequencerRegistryConfig, TestExternalEnvs, MEGA_SYSTEM_ADDRESS,
    ORACLE_CONTRACT_ADDRESS, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
    SEQUENCER_REGISTRY_CODE_HASH,
};
use mega_system_contracts::sequencer_registry::storage_slots::{
    CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, PENDING_SEQUENCER, PENDING_SYSTEM_ADDRESS,
    SEQUENCER_ACTIVATION_BLOCK, SYSTEM_ADDRESS_ACTIVATION_BLOCK,
};
use revm::{
    context::BlockEnv,
    database::State,
    handler::EvmTr,
    state::{AccountInfo, Bytecode},
};

const NEW_SYSTEM_ADDRESS: Address = address!("3000000000000000000000000000000000000003");
const BOOTSTRAP_SEQUENCER: Address = address!("0x4000000000000000000000000000000000000004");
const BOOTSTRAP_ADMIN: Address = address!("0x5000000000000000000000000000000000000005");

fn sequencer_registry_config() -> SequencerRegistryConfig {
    SequencerRegistryConfig {
        initial_system_address: MEGA_SYSTEM_ADDRESS,
        initial_sequencer: BOOTSTRAP_SEQUENCER,
        initial_admin: BOOTSTRAP_ADMIN,
    }
}

fn create_evm_env() -> EvmEnv<MegaSpecId> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::REX5;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

fn create_tx_from(
    sender: Address,
    nonce: u64,
    target: Address,
    data: Bytes,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 0,
        gas_limit: 1_000_000_000,
        to: TxKind::Call(target),
        value: U256::ZERO,
        input: data,
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, sender)
}

/// After `apply_pre_execution_changes()`, the context's `system_address` should be
/// the `initial_system_address` from `SequencerRegistryConfig` because the
/// `SequencerRegistry` was
/// just deployed with that value seeded into storage.
#[test]
fn test_bootstrap_block_resolves_system_address() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with_sequencer_registry_config(sequencer_registry_config());
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );

    let mut executor =
        block_executor_factory.create_executor(&mut state, block_ctx, create_evm_env());
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(
        system_address, MEGA_SYSTEM_ADDRESS,
        "Bootstrap block should resolve system_address to \
         SequencerRegistryConfig.initial_system_address"
    );
}

/// A system transaction from the resolved `system_address` to Oracle should succeed.
#[test]
fn test_system_tx_uses_resolved_system_address() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with_sequencer_registry_config(sequencer_registry_config());
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );

    let mut executor =
        block_executor_factory.create_executor(&mut state, block_ctx, create_evm_env());
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));

    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "System tx should succeed: {:?}", result.err());
}

/// Pre-populate a pending system address rotation, run `apply_pre_execution_changes()`,
/// verify `system_address` switches to the new address, and confirm the old address is rejected.
#[test]
fn test_system_address_rotation() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(NEW_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    // Pre-deploy SequencerRegistry with seeded storage + pending system address rotation
    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
            ..Default::default()
        },
    );
    // Seed current system address
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SYSTEM_ADDRESS,
        MEGA_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    // Seed pending rotation: new system address at block 1000
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        PENDING_SYSTEM_ADDRESS,
        NEW_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        U256::from(1000),
    )
    .unwrap();

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with_sequencer_registry_config(sequencer_registry_config());
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );

    // Block 1000 — rotation should be applied
    let mut executor =
        block_executor_factory.create_executor(&mut state, block_ctx, create_evm_env());
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    // 1. Verify system_address switched
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(system_address, NEW_SYSTEM_ADDRESS, "system_address should switch after rotation");

    // 2. New system address can send system tx to Oracle
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(NEW_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "System tx from new system address should succeed: {:?}", result.err());

    // 3. Old system address is rejected (not recognized as system tx → validation fails)
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(result.is_err(), "Tx from old system address should fail after rotation");
}

/// Sequencer rotation does NOT affect `system_address`.
#[test]
fn test_sequencer_rotation_does_not_affect_system_address() {
    let new_sequencer = address!("4000000000000000000000000000000000000004");

    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    // Pre-deploy with seeded storage + pending sequencer rotation
    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
            ..Default::default()
        },
    );
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SYSTEM_ADDRESS,
        MEGA_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SEQUENCER,
        MEGA_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    // Pending sequencer rotation at block 1000
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        PENDING_SEQUENCER,
        new_sequencer.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_ACTIVATION_BLOCK,
        U256::from(1000),
    )
    .unwrap();

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with_sequencer_registry_config(sequencer_registry_config());
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );

    let mut executor =
        block_executor_factory.create_executor(&mut state, block_ctx, create_evm_env());
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    // system_address should NOT change — only the sequencer rotated
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(
        system_address, MEGA_SYSTEM_ADDRESS,
        "Sequencer rotation should not affect system_address"
    );

    // System tx from the original system address should still work
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(
        result.is_ok(),
        "System tx from original system address should still work after sequencer rotation"
    );
}

/// Both system address and sequencer rotate in the same block.
/// Verify `system_address` switches to the new system address (not the new sequencer).
#[test]
fn test_dual_rotation_in_same_block() {
    let new_sequencer = address!("4000000000000000000000000000000000000004");

    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(NEW_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
            ..Default::default()
        },
    );
    // Seed current values
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SYSTEM_ADDRESS,
        MEGA_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SEQUENCER,
        MEGA_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    // Pending system address rotation at block 1000
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        PENDING_SYSTEM_ADDRESS,
        NEW_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        U256::from(1000),
    )
    .unwrap();
    // Pending sequencer rotation at block 1000 (same block)
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        PENDING_SEQUENCER,
        new_sequencer.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_ACTIVATION_BLOCK,
        U256::from(1000),
    )
    .unwrap();

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::Rex5, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );

    let mut executor =
        block_executor_factory.create_executor(&mut state, block_ctx, create_evm_env());
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    // system_address should be the NEW system address, not the new sequencer
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(
        system_address, NEW_SYSTEM_ADDRESS,
        "Dual rotation: system_address should be NEW_SYSTEM_ADDRESS"
    );

    // New system address can send system tx
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(NEW_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "New system address should work after dual rotation");
}

/// Pending rotation exists but activation block is in the future — no-op.
#[test]
fn test_pending_not_yet_due_is_noop() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
            ..Default::default()
        },
    );
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SYSTEM_ADDRESS,
        MEGA_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    // Pending at block 9999 — our test block is 1000
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        PENDING_SYSTEM_ADDRESS,
        NEW_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        U256::from(9999),
    )
    .unwrap();

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::Rex5, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );

    let mut executor =
        block_executor_factory.create_executor(&mut state, block_ctx, create_evm_env());
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    // system_address should still be the original — rotation not yet due
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(
        system_address, MEGA_SYSTEM_ADDRESS,
        "Pending not-yet-due rotation should not change system_address"
    );
}
