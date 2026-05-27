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
    test_utils::MemoryDatabase, BlockLimits, IOracle, ISequencerRegistry, MegaBlockExecutionCtx,
    MegaBlockExecutorFactory, MegaEvmFactory, MegaHardfork, MegaHardforkConfig, MegaSpecId,
    MegaTxEnvelope, SequencerRegistryConfig, TestExternalEnvs, MEGA_SYSTEM_ADDRESS,
    ORACLE_CONTRACT_ADDRESS, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
    SEQUENCER_REGISTRY_CODE_HASH,
};
use mega_system_contracts::sequencer_registry::storage_slots::{
    ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, PENDING_ADMIN, PENDING_SEQUENCER,
    PENDING_SYSTEM_ADDRESS, SEQUENCER_ACTIVATION_BLOCK, SYSTEM_ADDRESS_ACTIVATION_BLOCK,
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
const NEW_ADMIN: Address = address!("0x6000000000000000000000000000000000000006");

fn sequencer_registry_config() -> SequencerRegistryConfig {
    SequencerRegistryConfig {
        rex5_initial_sequencer: BOOTSTRAP_SEQUENCER,
        rex5_initial_admin: BOOTSTRAP_ADMIN,
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

/// Like [`create_tx_from`] but with an explicit `gas_limit`. System-address senders bypass
/// block gas-limit validation, so existing tests can use the very-large 1B figure;
/// regular-EOA tests must respect the block gas limit (30M in `create_evm_env`).
fn create_tx_from_with_gas_limit(
    sender: Address,
    nonce: u64,
    target: Address,
    data: Bytes,
    gas_limit: u64,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 0,
        gas_limit,
        to: TxKind::Call(target),
        value: U256::ZERO,
        input: data,
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, sender)
}

/// After `apply_pre_execution_changes()`, the context's `system_address` should be
/// `MEGA_SYSTEM_ADDRESS` because `SequencerRegistry` is deployed at genesis with the
/// initial system address hardcoded to that constant.
#[test]
fn test_bootstrap_block_resolves_system_address() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with_params(sequencer_registry_config());
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
        "Bootstrap block should resolve system_address to MEGA_SYSTEM_ADDRESS \
         (the genesis-hardcoded initial system address)"
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
        .with_params(sequencer_registry_config());
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

/// Pre-populate a pending system address change, run `apply_pre_execution_changes()`,
/// verify `system_address` switches to the new address, and confirm the old address is rejected.
#[test]
fn test_system_address_change() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(NEW_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    // Pre-deploy SequencerRegistry with seeded storage + pending system address change
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
    // Seed pending change: new system address at block 1000
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
        .with_params(sequencer_registry_config());
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );

    // Block 1000 — change should be applied
    let mut executor =
        block_executor_factory.create_executor(&mut state, block_ctx, create_evm_env());
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    // 1. Verify system_address switched
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(system_address, NEW_SYSTEM_ADDRESS, "system_address should switch after change");

    // 2. New system address can send system tx to Oracle
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(NEW_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "System tx from new system address should succeed: {:?}", result.err());

    // 3. Old system address is rejected (not recognized as system tx → validation fails)
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(result.is_err(), "Tx from old system address should fail after change");
}

/// Sequencer change does NOT affect `system_address`.
#[test]
fn test_sequencer_change_does_not_affect_system_address() {
    let new_sequencer = address!("4000000000000000000000000000000000000004");

    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    // Pre-deploy with seeded storage + pending sequencer change
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
    // Pending sequencer change at block 1000
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
        .with_params(sequencer_registry_config());
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

    // system_address should NOT change — only the sequencer changed
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(
        system_address, MEGA_SYSTEM_ADDRESS,
        "Sequencer change should not affect system_address"
    );

    // System tx from the original system address should still work
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(
        result.is_ok(),
        "System tx from original system address should still work after sequencer change"
    );
}

/// Both system address and sequencer change in the same block.
/// Verify `system_address` switches to the new system address (not the new sequencer).
#[test]
fn test_dual_change_in_same_block() {
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
    // Pending system address change at block 1000
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
    // Pending sequencer change at block 1000 (same block)
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
        .with_params(sequencer_registry_config());
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
        "Dual change: system_address should be NEW_SYSTEM_ADDRESS"
    );

    // New system address can send system tx
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(NEW_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "New system address should work after dual change");
}

/// Pending change exists but activation block is in the future — no-op.
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
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with_params(sequencer_registry_config());
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

    // system_address should still be the original — change not yet due
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(
        system_address, MEGA_SYSTEM_ADDRESS,
        "Pending not-yet-due change should not change system_address"
    );
}

/// Rex5 activated but `SequencerRegistryConfig` not configured — `apply_pre_execution_changes` must
/// fail.
#[test]
fn test_missing_sequencer_sequencer_registry_config_errors() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    // Activate Rex5 but deliberately omit .with_params(SequencerRegistryConfig { ... }).
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
    let err = executor
        .apply_pre_execution_changes()
        .expect_err("Rex5 without SequencerRegistryConfig must fail");
    assert!(
        err.to_string().contains("SequencerRegistryConfig not configured"),
        "unexpected error: {err}"
    );
}

/// End-to-end exercise of the two-step admin handoff through the block executor: the current
/// admin submits `transferAdmin`, the new admin submits `acceptAdmin`, and the registry's
/// `_admin` slot is promoted while `_pendingAdmin` is cleared. Mirrors the pattern used by
/// `test_system_address_change` for the system-address rotation flow.
#[test]
fn test_admin_handoff_via_block_executor() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(BOOTSTRAP_ADMIN, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(NEW_ADMIN, U256::from(1_000_000_000_000_000u64));

    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
            ..Default::default()
        },
    );
    // Seed the slots the handoff path actually reads/writes (slot 0 keeps Oracle/system-address
    // resolution happy across `apply_pre_execution_changes`; slot 2 is the modifier guard).
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SYSTEM_ADDRESS,
        MEGA_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        ADMIN,
        BOOTSTRAP_ADMIN.into_word().into(),
    )
    .unwrap();

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with_params(sequencer_registry_config());
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

    // tx#1: current admin schedules a transfer to NEW_ADMIN.
    let calldata = ISequencerRegistry::transferAdminCall { newAdmin: NEW_ADMIN }.abi_encode();
    let tx = create_tx_from_with_gas_limit(
        BOOTSTRAP_ADMIN,
        0,
        SEQUENCER_REGISTRY_ADDRESS,
        Bytes::from(calldata),
        1_000_000,
    );
    let receipt = executor
        .execute_transaction(&tx)
        .expect("transferAdmin tx should be accepted by the executor");
    assert!(receipt > 0, "transferAdmin tx should report non-zero gas used");

    // tx#2: NEW_ADMIN completes the handoff.
    let calldata = ISequencerRegistry::acceptAdminCall {}.abi_encode();
    let tx = create_tx_from_with_gas_limit(
        NEW_ADMIN,
        0,
        SEQUENCER_REGISTRY_ADDRESS,
        Bytes::from(calldata),
        1_000_000,
    );
    let receipt = executor
        .execute_transaction(&tx)
        .expect("acceptAdmin tx should be accepted by the executor");
    assert!(receipt > 0, "acceptAdmin tx should report non-zero gas used");

    // Drop the executor to release the &mut borrow on `state`, then read the post-state.
    drop(executor);

    let admin_slot = revm::Database::storage(&mut state, SEQUENCER_REGISTRY_ADDRESS, ADMIN)
        .expect("read ADMIN slot");
    assert_eq!(
        admin_slot,
        U256::from_be_bytes(NEW_ADMIN.into_word().0),
        "_admin must be promoted to NEW_ADMIN after acceptAdmin",
    );

    let pending_slot =
        revm::Database::storage(&mut state, SEQUENCER_REGISTRY_ADDRESS, PENDING_ADMIN)
            .expect("read PENDING_ADMIN slot");
    assert_eq!(pending_slot, U256::ZERO, "_pendingAdmin must be cleared after acceptAdmin");
}
