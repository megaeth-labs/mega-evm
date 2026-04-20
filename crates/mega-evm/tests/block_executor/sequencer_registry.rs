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
    MegaTxEnvelope, TestExternalEnvs, ORACLE_CONTRACT_ADDRESS, REX5_INITIAL_SEQUENCER,
    SEQUENCER_REGISTRY_ADDRESS,
};
use revm::{
    context::BlockEnv,
    database::State,
    handler::EvmTr,
    state::{AccountInfo, Bytecode},
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");

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

fn create_system_tx(
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
    alloy_consensus::transaction::Recovered::new_unchecked(tx, REX5_INITIAL_SEQUENCER)
}

fn create_regular_tx(
    nonce: u64,
    target: Address,
    data: Bytes,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit: 1_000_000,
        to: TxKind::Call(target),
        value: U256::ZERO,
        input: data,
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// After `apply_pre_execution_changes()`, the context's `system_address` should be
/// `REX5_INITIAL_SEQUENCER` because the `SequencerRegistry` was just deployed with
/// zero `_currentSequencer` storage (constant bootstrap fallback).
#[test]
fn test_bootstrap_block_resolves_system_address() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(REX5_INITIAL_SEQUENCER, U256::from(1_000_000_000_000_000u64));

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

    // Run pre-execution changes (deploys SequencerRegistry + Oracle v2.0.0, resolves system
    // address)
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    // Verify system_address was resolved to INITIAL_SEQUENCER
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(
        system_address, REX5_INITIAL_SEQUENCER,
        "Bootstrap block should resolve system_address to REX5_INITIAL_SEQUENCER"
    );
}

/// A system transaction from `REX5_INITIAL_SEQUENCER` to Oracle (whitelisted) should succeed
/// after `apply_pre_execution_changes()` resolves the dynamic system address.
#[test]
fn test_system_tx_uses_resolved_system_address() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(REX5_INITIAL_SEQUENCER, U256::from(1_000_000_000_000_000u64));

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

    // System tx from INITIAL_SEQUENCER calling Oracle.getSlot(0)
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_system_tx(0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));

    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "System tx should succeed: {:?}", result.err());
}

/// Calling `currentSequencer()` on the deployed `SequencerRegistry` should return
/// `INITIAL_SEQUENCER` on the bootstrap block (no rotation has occurred).
#[test]
fn test_registry_current_sequencer_on_bootstrap_block() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

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

    // Regular tx calling SequencerRegistry.currentSequencer()
    let calldata = ISequencerRegistry::currentSequencerCall {}.abi_encode();
    let tx = create_regular_tx(0, SEQUENCER_REGISTRY_ADDRESS, Bytes::from(calldata));

    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "currentSequencer() call should succeed: {:?}", result.err());
}

const NEW_SEQUENCER: Address = address!("3000000000000000000000000000000000000003");

// Storage slot indices from `forge inspect SequencerRegistry storage-layout`:
// slot 0: _currentSequencer, slot 1: _admin, slot 2: _pendingSequencer, slot 3: _activationBlock
const SLOT_PENDING_SEQUENCER: U256 = U256::from_limbs([2, 0, 0, 0]);
const SLOT_ACTIVATION_BLOCK: U256 = U256::from_limbs([3, 0, 0, 0]);

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

/// Pre-populate a pending rotation due at block 1000, run `apply_pre_execution_changes()`
/// at block 1000, verify `system_address` switches to new sequencer, and confirm the new
/// sequencer can send system transactions to Oracle while the old one cannot.
#[test]
fn test_rotation_switches_system_address_and_oracle_authority() {
    use mega_evm::SEQUENCER_REGISTRY_CODE_HASH;

    let mut db = MemoryDatabase::default();
    db.set_account_balance(REX5_INITIAL_SEQUENCER, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(NEW_SEQUENCER, U256::from(1_000_000_000_000_000u64));

    // Pre-deploy SequencerRegistry with pending rotation due at block 1000.
    // _currentSequencer (slot 0) = 0 (INITIAL_SEQUENCER via fallback)
    // _pendingSequencer (slot 2) = NEW_SEQUENCER
    // _activationBlock (slot 3) = 1000
    let registry_code = mega_evm::SEQUENCER_REGISTRY_CODE;
    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(registry_code)),
            ..Default::default()
        },
    );
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        SLOT_PENDING_SEQUENCER,
        NEW_SEQUENCER.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(SEQUENCER_REGISTRY_ADDRESS, SLOT_ACTIVATION_BLOCK, U256::from(1000))
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

    // Block 1000 — activationBlock matches, rotation should be applied
    let mut executor =
        block_executor_factory.create_executor(&mut state, block_ctx, create_evm_env());
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    // 1. Verify system_address switched to NEW_SEQUENCER
    let system_address = executor.evm().ctx_ref().system_address();
    assert_eq!(
        system_address, NEW_SEQUENCER,
        "After rotation, system_address should be NEW_SEQUENCER"
    );

    // 2. System tx from NEW_SEQUENCER to Oracle should succeed
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx = create_tx_from(NEW_SEQUENCER, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "System tx from new sequencer should succeed: {:?}", result.err());

    // 3. System tx from OLD sequencer (INITIAL_SEQUENCER) to Oracle should fail because the old
    //    address is no longer the system_address. It won't be recognized as a system tx → normal
    //    validation applies → gas_price=0 without deposit flag → validation error.
    let calldata = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let tx =
        create_tx_from(REX5_INITIAL_SEQUENCER, 0, ORACLE_CONTRACT_ADDRESS, Bytes::from(calldata));
    let result = executor.execute_transaction(&tx);
    assert!(
        result.is_err(),
        "Tx from old sequencer should fail after rotation (no longer recognized as system tx)"
    );
}

// ============================================================================
// Boundary / failure path tests
// ============================================================================

/// On a bootstrap block, `applyPendingChange()` is skipped (contract not yet committed).
/// Verify `system_address` is still resolved correctly via constant fallback.
#[test]
fn test_bootstrap_block_skips_apply_pending_change() {
    // Fresh DB — SequencerRegistry not pre-deployed. This IS the bootstrap block.
    let mut db = MemoryDatabase::default();
    db.set_account_balance(REX5_INITIAL_SEQUENCER, U256::from(1_000_000_000_000_000u64));

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

    // Should succeed — apply is skipped on bootstrap, resolve uses fallback
    executor
        .apply_pre_execution_changes()
        .expect("bootstrap pre-execution should succeed even without prior deploy");

    assert_eq!(
        executor.evm().ctx_ref().system_address(),
        REX5_INITIAL_SEQUENCER,
        "Bootstrap block should use INITIAL_SEQUENCER"
    );
}

/// Pending rotation exists but activation block is in the future → no-op.
/// System address should remain `INITIAL_SEQUENCER`.
#[test]
fn test_pending_not_yet_due_is_noop() {
    use mega_evm::SEQUENCER_REGISTRY_CODE_HASH;

    let mut db = MemoryDatabase::default();
    db.set_account_balance(REX5_INITIAL_SEQUENCER, U256::from(1_000_000_000_000_000u64));

    // Pre-deploy SequencerRegistry with pending rotation at block 9999 (future).
    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(mega_evm::SEQUENCER_REGISTRY_CODE)),
            ..Default::default()
        },
    );
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        SLOT_PENDING_SEQUENCER,
        NEW_SEQUENCER.into_word().into(),
    )
    .unwrap();
    // activationBlock = 9999, but block.number = 1000 → not yet due
    db.insert_account_storage(SEQUENCER_REGISTRY_ADDRESS, SLOT_ACTIVATION_BLOCK, U256::from(9999))
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
    executor.apply_pre_execution_changes().expect("pre-execution should succeed");

    // System address should still be INITIAL_SEQUENCER — rotation not yet due
    assert_eq!(
        executor.evm().ctx_ref().system_address(),
        REX5_INITIAL_SEQUENCER,
        "Pending rotation not yet due should not change system address"
    );
}

/// Wrong code hash at `SequencerRegistry` address — deploy overwrites it, pre-execution succeeds.
#[test]
fn test_deploy_overwrites_wrong_code_hash_and_pre_execution_succeeds() {
    let mut db = MemoryDatabase::default();

    // Pre-deploy with wrong code
    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: B256::ZERO,
            code: Some(Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]))),
            ..Default::default()
        },
    );

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

    // apply_pre_execution_changes will deploy the correct bytecode (overwriting the wrong one),
    // then resolve should succeed since deploy overwrites with correct hash.
    // This tests that the deploy-overwrites-wrong-code path works end-to-end.
    let result = executor.apply_pre_execution_changes();
    assert!(result.is_ok(), "Deploy should overwrite wrong code and resolve should succeed");
}
