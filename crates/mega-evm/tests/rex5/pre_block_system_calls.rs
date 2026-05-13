#![allow(clippy::doc_markdown)]
//! End-to-end tests for REX5 pre-block system call result handling.
//!
//! Under `MegaSpecId::REX5`, a non-`Success` result from the EIP-2935 or
//! EIP-4788 pre-block system call must reject the block before any state
//! delta is committed or observed by the on-state hook. Pre-REX5 chains
//! keep their existing accept-and-commit-empty behavior.

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};

use alloy_evm::{
    block::{BlockExecutor, BlockValidationError, OnStateHook, StateChangeSource},
    Evm, EvmEnv,
};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, BucketHasher, BucketId, MegaBlockExecutionCtx,
    MegaBlockExecutorFactory, MegaEvmFactory, MegaHardfork, MegaHardforkConfig, MegaSpecId,
    SequencerRegistryConfig, TestExternalEnvs,
};
use revm::{
    context::BlockEnv,
    database::{Database as _, State},
    state::{AccountInfo, Bytecode, EvmState},
};

const ACTIVATION_BLOCK: u64 = 1000;

/// Bucket that every SALT lookup is routed to under [`SingleBucketHasher`].
const HEAVY_BUCKET_ID: BucketId = 100_000;
/// Capacity = 2000 × `MIN_BUCKET_SIZE` (256). Yields a 2000× multiplier, so
/// each zero→nonzero `SSTORE` charges ≈ 40M of dynamic storage gas. EIP-2935
/// performs at least one such write per pre-block call, which exceeds revm's
/// hard-coded 30M system-call gas budget and forces a Halt.
const HEAVY_BUCKET_CAPACITY: u64 = 512_000;
const BLOCK_GAS_LIMIT: u64 = 250_000_000;

const BOOTSTRAP_SEQUENCER: Address =
    alloy_primitives::address!("4000000000000000000000000000000000000004");
const BOOTSTRAP_ADMIN: Address =
    alloy_primitives::address!("5000000000000000000000000000000000000005");

/// Routes every account / slot to a single bucket so the heavy capacity bites
/// every `SSTORE` performed by the pre-block helpers, regardless of the slot.
#[derive(Debug, Clone, Copy)]
struct SingleBucketHasher;

impl BucketHasher for SingleBucketHasher {
    fn bucket_id(_key: &[u8]) -> BucketId {
        HEAVY_BUCKET_ID
    }
}

fn sequencer_registry_config() -> SequencerRegistryConfig {
    SequencerRegistryConfig {
        rex5_initial_sequencer: BOOTSTRAP_SEQUENCER,
        rex5_initial_admin: BOOTSTRAP_ADMIN,
    }
}

fn create_evm_env(spec: MegaSpecId, gas_limit: u64) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = spec;
    let block_env = BlockEnv {
        number: U256::from(ACTIVATION_BLOCK),
        timestamp: U256::from(1_800_000_000),
        gas_limit,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

fn rex5_chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with_params(sequencer_registry_config())
}

fn rex4_chain_spec() -> MegaHardforkConfig {
    // Activate up to Rex4 only — Rex5 stays at `ForkCondition::Never`, so
    // `is_rex_5_active_at_timestamp(_)` returns false and the new check is
    // skipped. Sequencer registry is not needed because its deploy is gated
    // on Rex5 activation.
    MegaHardforkConfig::default().with(MegaHardfork::Rex4, ForkCondition::Timestamp(0))
}

fn install_eip2935_history_storage(db: &mut MemoryDatabase) {
    let bytecode = Bytecode::new_raw(alloy_eips::eip2935::HISTORY_STORAGE_CODE.clone());
    db.insert_account_info(
        alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
        AccountInfo { code_hash: bytecode.hash_slow(), code: Some(bytecode), ..Default::default() },
    );
}

fn install_eip4788_beacon_roots(db: &mut MemoryDatabase) {
    let bytecode = Bytecode::new_raw(alloy_eips::eip4788::BEACON_ROOTS_CODE.clone());
    db.insert_account_info(
        alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
        AccountInfo { code_hash: bytecode.hash_slow(), code: Some(bytecode), ..Default::default() },
    );
}

fn heavy_external_envs() -> TestExternalEnvs<Infallible, SingleBucketHasher> {
    TestExternalEnvs::<Infallible, SingleBucketHasher>::new()
        .with_bucket_capacity(HEAVY_BUCKET_ID, HEAVY_BUCKET_CAPACITY)
}

fn light_external_envs() -> TestExternalEnvs<Infallible, SingleBucketHasher> {
    // Default capacity for the heavy bucket → 1× multiplier, normal gas cost.
    TestExternalEnvs::<Infallible, SingleBucketHasher>::new()
}

/// Recording state hook that captures every `(StateChangeSource, EvmState)`
/// pair the block executor reports, so a test can assert that a failed
/// pre-block call's state delta did NOT enter the witness path.
#[derive(Debug, Default, Clone)]
struct RecordingStateHook {
    events: Arc<Mutex<Vec<StateChangeSource>>>,
}

impl OnStateHook for RecordingStateHook {
    fn on_state(&mut self, source: StateChangeSource, _state: &EvmState) {
        self.events.lock().unwrap().push(source);
    }
}

/// Block setup with parent_hash = nonzero (so EIP-2935 actually runs the
/// SSTORE) and parent_beacon_block_root = nonzero. The non-genesis block
/// number means the helpers don't take the genesis-skip early-return.
fn block_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(
        B256::from([0x29; 32]),       // parent_hash
        Some(B256::from([0x47; 32])), // parent_beacon_block_root
        Bytes::new(),
        BlockLimits::no_limits(),
    )
}

/// Heavy SALT makes the EIP-2935 history-storage SSTORE OOG inside the
/// 30M system-call gas budget. Under REX5+, the block executor must
/// reject the block, and the failed call's state delta must not reach
/// the on-state hook.
#[test]
fn test_rex5_block_rejected_when_blockhashes_pre_block_call_halts() {
    let mut db = MemoryDatabase::default();
    install_eip2935_history_storage(&mut db);
    install_eip4788_beacon_roots(&mut db);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(heavy_external_envs());
    let block_executor_factory = MegaBlockExecutorFactory::new(
        rex5_chain_spec(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx(),
        create_evm_env(MegaSpecId::REX5, BLOCK_GAS_LIMIT),
    );

    // Recording state hook so we can also pin the witness-path invariant.
    let recorder = RecordingStateHook::default();
    let events = recorder.events.clone();
    BlockExecutor::set_state_hook(&mut executor, Some(Box::new(recorder)));

    let err = executor
        .apply_pre_execution_changes()
        .expect_err("REX5 must reject a halted EIP-2935 pre-block system call");

    let validation_err = match &err {
        alloy_evm::block::BlockExecutionError::Validation(v) => v,
        other => panic!("Expected Validation error, got: {other:?}"),
    };
    assert!(
        matches!(validation_err, BlockValidationError::BlockHashContractCall { .. }),
        "Expected BlockHashContractCall, got: {validation_err:?}"
    );

    // The failed pre-block call's state delta must not reach the on-state hook.
    let recorded = events.lock().unwrap();
    let saw_block_hashes = recorded.iter().any(|s| {
        matches!(
            s,
            StateChangeSource::PreBlock(
                alloy_evm::block::StateChangePreBlockSource::BlockHashesContract
            )
        )
    });
    assert!(
        !saw_block_hashes,
        "Failed EIP-2935 pre-block call's state delta must NOT enter the on-state \
         witness path; recorded sources: {recorded:?}"
    );
}

/// Symmetric to `..._blockhashes_...`: heavy SALT makes the EIP-4788
/// beacon-roots SSTORE OOG; REX5+ must reject the block.
#[test]
fn test_rex5_block_rejected_when_beacon_root_pre_block_call_halts() {
    let mut db = MemoryDatabase::default();
    // No EIP-2935 bytecode → its system call returns Ok(Success) trivially.
    install_eip4788_beacon_roots(&mut db);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(heavy_external_envs());
    let block_executor_factory = MegaBlockExecutorFactory::new(
        rex5_chain_spec(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx(),
        create_evm_env(MegaSpecId::REX5, BLOCK_GAS_LIMIT),
    );

    let err = executor
        .apply_pre_execution_changes()
        .expect_err("REX5 must reject a halted EIP-4788 pre-block system call");

    let validation_err = match &err {
        alloy_evm::block::BlockExecutionError::Validation(v) => v,
        other => panic!("Expected Validation error, got: {other:?}"),
    };
    assert!(
        matches!(validation_err, BlockValidationError::BeaconRootContractCall { .. }),
        "Expected BeaconRootContractCall, got: {validation_err:?}"
    );
}

/// Same heavy-SALT scenario under a Rex4 chain spec: pre-REX5 keeps its
/// existing accept-and-commit-empty behavior so historical replay is
/// byte-for-byte deterministic.
#[test]
fn test_pre_rex5_preserves_silent_pre_block_call_failure() {
    let mut db = MemoryDatabase::default();
    install_eip2935_history_storage(&mut db);
    install_eip4788_beacon_roots(&mut db);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(heavy_external_envs());
    let block_executor_factory = MegaBlockExecutorFactory::new(
        rex4_chain_spec(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx(),
        create_evm_env(MegaSpecId::REX4, BLOCK_GAS_LIMIT),
    );

    executor
        .apply_pre_execution_changes()
        .expect("pre-REX5 must accept a halted pre-block system call");

    // The OOG'd SSTORE never wrote the parent hash to slot 0, so slot 0
    // must not equal the parent-hash value.
    let storage_after = executor
        .evm_mut()
        .db_mut()
        .storage(alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS, U256::ZERO)
        .unwrap();
    let parent_hash_word = U256::from_be_bytes(B256::from([0x29; 32]).0);
    assert_ne!(
        storage_after, parent_hash_word,
        "history-storage slot 0 must not contain the parent hash; the SSTORE OOG'd",
    );
}

/// Successful pre-block system calls under REX5 commit normally and
/// reach the on-state hook.
#[test]
fn test_rex5_successful_pre_block_call_commits_normally() {
    let mut db = MemoryDatabase::default();
    install_eip2935_history_storage(&mut db);
    install_eip4788_beacon_roots(&mut db);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(light_external_envs());
    let block_executor_factory = MegaBlockExecutorFactory::new(
        rex5_chain_spec(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx(),
        create_evm_env(MegaSpecId::REX5, BLOCK_GAS_LIMIT),
    );

    let recorder = RecordingStateHook::default();
    let events = recorder.events.clone();
    BlockExecutor::set_state_hook(&mut executor, Some(Box::new(recorder)));

    executor
        .apply_pre_execution_changes()
        .expect("REX5 must accept successful pre-block system calls");

    // Both EIP-2935 and EIP-4788 outcomes appear in the witness path.
    let recorded = events.lock().unwrap();
    let saw_block_hashes = recorded.iter().any(|s| {
        matches!(
            s,
            StateChangeSource::PreBlock(
                alloy_evm::block::StateChangePreBlockSource::BlockHashesContract
            )
        )
    });
    let saw_beacon_root = recorded.iter().any(|s| {
        matches!(
            s,
            StateChangeSource::PreBlock(
                alloy_evm::block::StateChangePreBlockSource::BeaconRootContract
            )
        )
    });
    assert!(
        saw_block_hashes,
        "Successful EIP-2935 pre-block call must reach the on-state hook; sources: {recorded:?}"
    );
    assert!(
        saw_beacon_root,
        "Successful EIP-4788 pre-block call must reach the on-state hook; sources: {recorded:?}"
    );
}

/// At block number 0 both pre-block helpers return `None` (no system
/// call attempted); the REX5 check is a no-op on this path.
#[test]
fn test_rex5_genesis_block_pre_block_call_skip_unaffected() {
    let mut db = MemoryDatabase::default();
    install_eip2935_history_storage(&mut db);
    install_eip4788_beacon_roots(&mut db);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(heavy_external_envs());
    let block_executor_factory = MegaBlockExecutorFactory::new(
        rex5_chain_spec(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );

    // Block number 0 — genesis-skip in both helpers.
    let genesis_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::REX5;
    let block_env = BlockEnv {
        number: U256::ZERO,
        timestamp: U256::from(1_800_000_000),
        gas_limit: BLOCK_GAS_LIMIT,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    let mut executor = block_executor_factory.create_executor(&mut state, genesis_ctx, evm_env);

    executor.apply_pre_execution_changes().expect(
        "Genesis block must skip pre-block system calls (helpers return None) and the \
         REX5 check must be a no-op on this path",
    );
}
