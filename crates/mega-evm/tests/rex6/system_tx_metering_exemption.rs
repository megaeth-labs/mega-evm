#![allow(clippy::doc_markdown)]
//! End-to-end tests for the REX6 system-originated metering exemption.
//!
//! REX6 exempts system-originated transactions (the protocol's pre-block system calls and
//! sequencer mega system transactions — see `mega_evm::is_system_originated`) from MegaETH's
//! per-transaction resource metering: SALT-scaled storage gas is charged at the minimum bucket
//! capacity (so it never grows with the bucket), and the four `AdditionalLimit` dimensions plus
//! gas detention are bypassed. Only the standard EVM `gas_limit` still bounds the work.
//!
//! The heavy-SALT scenario uses a 100,000× bucket multiplier, which makes each zero→nonzero
//! `SSTORE` cost ≈ 2G of dynamic storage gas. Two paired tests pin the parity claim end-to-end:
//! the REX6 case accepts the block; the REX5 case (same setup, only `cfg.spec` differs) rejects
//! it on the EIP-2935 pre-block `SSTORE`.

use std::convert::Infallible;

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::{
    block::{
        BlockExecutor, BlockValidationError, OnStateHook, StateChangePreBlockSource,
        StateChangeSource,
    },
    Evm as _, EvmEnv,
};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, BucketHasher, BucketId, IOracle,
    MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaHardfork,
    MegaHardforkConfig, MegaSpecId, MegaTxEnvelope, SequencerRegistryConfig,
    SequencerRegistryRex6Config, TestExternalEnvs, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    context::{BlockEnv, CfgEnv, ContextTr as _},
    database::State,
    state::{AccountInfo, Bytecode, EvmState},
    Database as _,
};
use std::sync::{Arc, Mutex};

const ACTIVATION_BLOCK: u64 = 1000;
const MEGA_CHAIN_ID: u64 = 4326;
const BLOCK_GAS_LIMIT: u64 = 250_000_000;
const SYSTEM_TX_GAS_LIMIT: u64 = 4_020_000;
const ORACLE_SLOT: U256 = U256::ZERO;

/// Bucket that every SALT lookup is routed to under [`SingleBucketHasher`].
const HEAVY_BUCKET_ID: BucketId = 100_000;
/// Capacity = 100,000 × `MIN_BUCKET_SIZE` (256) → a 100,000× multiplier, so each zero→nonzero
/// `SSTORE` would cost ≈ 2G of dynamic storage gas without the exemption (far beyond any
/// reasonable gas limit). This is the capacity that rejects the block under REX5.
const HEAVY_BUCKET_CAPACITY: u64 = 25_600_000;

const BOOTSTRAP_SEQUENCER: Address = address!("4000000000000000000000000000000000000004");
const BOOTSTRAP_ADMIN: Address = address!("5000000000000000000000000000000000000005");

/// Routes every account / slot to a single bucket so the heavy capacity bites every `SSTORE`,
/// regardless of which contract/slot performs it.
#[derive(Debug, Clone, Copy)]
struct SingleBucketHasher;

impl BucketHasher for SingleBucketHasher {
    fn bucket_id(_key: &[u8]) -> BucketId {
        HEAVY_BUCKET_ID
    }
}

fn rex6_chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with_all_activated()
        .with_params(SequencerRegistryConfig {
            rex5_initial_sequencer: BOOTSTRAP_SEQUENCER,
            rex5_initial_admin: BOOTSTRAP_ADMIN,
        })
        .with_params(SequencerRegistryRex6Config { rex6_min_rotation_delay: 100 })
}

/// REX5-only chain spec (REX6 stays at `ForkCondition::Never`). Used by the negative parity test
/// to confirm the heavy-SALT pre-block-call scenario is rejected pre-REX6.
fn rex5_chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default().with(MegaHardfork::Rex5, ForkCondition::Timestamp(0)).with_params(
        SequencerRegistryConfig {
            rex5_initial_sequencer: BOOTSTRAP_SEQUENCER,
            rex5_initial_admin: BOOTSTRAP_ADMIN,
        },
    )
}

fn evm_env(spec: MegaSpecId, gas_limit: u64) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = CfgEnv::default();
    cfg_env.spec = spec;
    cfg_env.chain_id = MEGA_CHAIN_ID;
    let block_env = BlockEnv {
        number: U256::from(ACTIVATION_BLOCK),
        timestamp: U256::from(1_800_000_000),
        gas_limit,
        basefee: 0,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

fn rex6_evm_env(gas_limit: u64) -> EvmEnv<MegaSpecId> {
    evm_env(MegaSpecId::REX6, gas_limit)
}

fn heavy_external_envs() -> TestExternalEnvs<Infallible, SingleBucketHasher> {
    TestExternalEnvs::<Infallible, SingleBucketHasher>::new()
        .with_bucket_capacity(HEAVY_BUCKET_ID, HEAVY_BUCKET_CAPACITY)
}

/// Default capacity (`MIN_BUCKET_SIZE`) for the heavy bucket → 1× multiplier, normal gas cost.
fn light_external_envs() -> TestExternalEnvs<Infallible, SingleBucketHasher> {
    TestExternalEnvs::<Infallible, SingleBucketHasher>::new()
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

/// Recording state hook capturing every reported `StateChangeSource`, so a test can assert that a
/// pre-block call's state delta reached the witness path (i.e., the call succeeded and committed).
#[derive(Debug, Default, Clone)]
struct RecordingStateHook {
    events: Arc<Mutex<Vec<StateChangeSource>>>,
}

impl OnStateHook for RecordingStateHook {
    fn on_state(&mut self, source: StateChangeSource, _state: &EvmState) {
        self.events.lock().unwrap().push(source);
    }
}

fn oracle_set_slots_calldata(slot: U256, value: B256) -> Bytes {
    IOracle::setSlotsCall { slots: vec![slot], values: vec![value] }.abi_encode().into()
}

/// A sequencer mega system tx: legacy tx from `MEGA_SYSTEM_ADDRESS` to the whitelisted oracle.
fn oracle_system_tx(nonce: u64, slot: U256, value: B256) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(MEGA_CHAIN_ID),
        nonce,
        gas_price: 0,
        gas_limit: SYSTEM_TX_GAS_LIMIT,
        to: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        value: U256::ZERO,
        input: oracle_set_slots_calldata(slot, value),
    };
    let signed = Signed::new_unchecked(
        tx,
        Signature::test_signature(),
        B256::with_last_byte(nonce.saturating_add(1) as u8),
    );
    Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), MEGA_SYSTEM_ADDRESS)
}

// ============================================================================
// 1. Pre-block system calls (EIP-2935 / EIP-4788) accepted under heavy SALT
// ============================================================================

/// Under heavy SALT, the EIP-2935 history-storage and EIP-4788 beacon-root pre-block `SSTORE`s
/// would each cost ≈ 2G of dynamic storage gas — which rejects the block under REX5. REX6 exempts
/// these system-originated calls (`0xff..fe` caller), so both succeed and their state deltas reach
/// the on-state witness path.
#[test]
fn test_rex6_pre_block_calls_accepted_under_heavy_salt() {
    let mut db = MemoryDatabase::default();
    install_eip2935_history_storage(&mut db);
    install_eip4788_beacon_roots(&mut db);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(heavy_external_envs());
    let block_executor_factory = MegaBlockExecutorFactory::new(
        rex6_chain_spec(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );
    // Non-zero parent hash / beacon root so both pre-block calls perform a real zero→nonzero
    // SSTORE.
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::from([0x29; 32]),
        Some(B256::from([0x47; 32])),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx,
        rex6_evm_env(BLOCK_GAS_LIMIT),
    );

    let recorder = RecordingStateHook::default();
    let events = recorder.events.clone();
    BlockExecutor::set_state_hook(&mut executor, Some(Box::new(recorder)));

    executor
        .apply_pre_execution_changes()
        .expect("REX6 must accept pre-block system calls even under heavy SALT");

    let recorded = events.lock().unwrap();
    let saw_block_hashes = recorded.iter().any(|s| {
        matches!(s, StateChangeSource::PreBlock(StateChangePreBlockSource::BlockHashesContract))
    });
    let saw_beacon_root = recorded.iter().any(|s| {
        matches!(s, StateChangeSource::PreBlock(StateChangePreBlockSource::BeaconRootContract))
    });
    assert!(saw_block_hashes, "EIP-2935 pre-block call must succeed and commit under REX6");
    assert!(saw_beacon_root, "EIP-4788 pre-block call must succeed and commit under REX6");
}

/// Parity counterpart to the test above: the identical heavy-SALT setup, but with
/// `cfg.spec = REX5` and a REX5-only chain spec, must reject the block. This pins the parity
/// claim — exemption is only in effect under REX6 — inside this file rather than relying on the
/// reader to cross-reference the REX5 test suite.
#[test]
fn test_rex5_pre_block_calls_rejected_under_heavy_salt() {
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
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::from([0x29; 32]),
        Some(B256::from([0x47; 32])),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx,
        evm_env(MegaSpecId::REX5, BLOCK_GAS_LIMIT),
    );

    let err = executor
        .apply_pre_execution_changes()
        .expect_err("REX5 must reject the heavy-SALT EIP-2935 pre-block call");
    let validation_err = match &err {
        alloy_evm::block::BlockExecutionError::Validation(v) => v,
        other => panic!("Expected Validation error, got: {other:?}"),
    };
    assert!(
        matches!(validation_err, BlockValidationError::BlockHashContractCall { .. }),
        "Expected BlockHashContractCall, got: {validation_err:?}"
    );
}

// ============================================================================
// 2. Sequencer mega system tx (oracle write) succeeds under heavy SALT
// ============================================================================

/// A sequencer oracle `setSlots` mega system tx performs a real (non-intercepted) zero→nonzero
/// `SSTORE`. Under heavy SALT this would OOG without the exemption; REX6 charges it at the minimum
/// bucket capacity, so it succeeds and the oracle slot is updated.
#[test]
fn test_rex6_sequencer_oracle_tx_succeeds_under_heavy_salt() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(heavy_external_envs());
    let executor_factory = MegaBlockExecutorFactory::new(
        rex6_chain_spec(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );
    // Zero parent hash/beacon → pre-block calls write zero (no SSTORE_SET), isolating the SALT
    // exposure to the oracle write under test.
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    let mut executor =
        executor_factory.create_executor(&mut state, block_ctx, rex6_evm_env(BLOCK_GAS_LIMIT));
    executor.evm.ctx.chain_mut().operator_fee_scalar = Some(U256::ZERO);
    executor.evm.ctx.chain_mut().operator_fee_constant = Some(U256::ZERO);

    executor
        .apply_pre_execution_changes()
        .expect("REX6 pre-execution must deploy oracle + registry");

    let value = B256::with_last_byte(0xAB);
    let tx = oracle_system_tx(0, ORACLE_SLOT, value);
    let outcome =
        executor.run_transaction(&tx).expect("oracle system tx must execute under REX6 heavy SALT");
    assert!(outcome.result.is_success(), "oracle system tx must succeed: {:?}", outcome.result);
    executor.commit_transaction_outcome(outcome).expect("commit oracle system tx");

    let oracle_after = executor
        .evm
        .db_mut()
        .storage(ORACLE_CONTRACT_ADDRESS, ORACLE_SLOT)
        .expect("oracle storage read");
    assert_eq!(
        oracle_after,
        U256::from_be_bytes(value.0),
        "oracle slot must hold the written value after the exempt system tx",
    );
}

// ============================================================================
// 3. Determinism: system tx cost/state independent of bucket capacity
// ============================================================================

/// Runs the identical oracle system tx under light (1×) and heavy (100,000×) SALT, both REX6, and
/// asserts identical `gas_used` and identical resulting oracle state. This pins the core property:
/// a system-originated transaction's cost no longer depends on SALT bucket capacity. (Under REX5
/// the heavy case would instead OOG.)
#[test]
fn test_rex6_system_tx_storage_gas_independent_of_bucket_capacity() {
    fn run_oracle_tx(envs: TestExternalEnvs<Infallible, SingleBucketHasher>) -> (u64, U256) {
        let mut state = State::builder().with_database(MemoryDatabase::default()).build();
        let evm_factory = MegaEvmFactory::new().with_external_env_factory(envs);
        let executor_factory = MegaBlockExecutorFactory::new(
            rex6_chain_spec(),
            evm_factory,
            OpAlloyReceiptBuilder::default(),
        );
        let block_ctx = MegaBlockExecutionCtx::new(
            B256::ZERO,
            Some(B256::ZERO),
            Bytes::new(),
            BlockLimits::no_limits(),
        );
        let mut executor =
            executor_factory.create_executor(&mut state, block_ctx, rex6_evm_env(BLOCK_GAS_LIMIT));
        executor.evm.ctx.chain_mut().operator_fee_scalar = Some(U256::ZERO);
        executor.evm.ctx.chain_mut().operator_fee_constant = Some(U256::ZERO);
        executor.apply_pre_execution_changes().expect("pre-execution");

        let value = B256::with_last_byte(0xCD);
        let tx = oracle_system_tx(0, ORACLE_SLOT, value);
        let outcome = executor.run_transaction(&tx).expect("oracle system tx must execute");
        assert!(outcome.result.is_success(), "must succeed: {:?}", outcome.result);
        let gas_used = outcome.result.gas_used();
        executor.commit_transaction_outcome(outcome).expect("commit");
        let slot = executor
            .evm
            .db_mut()
            .storage(ORACLE_CONTRACT_ADDRESS, ORACLE_SLOT)
            .expect("oracle storage read");
        (gas_used, slot)
    }

    let (gas_light, slot_light) = run_oracle_tx(light_external_envs());
    let (gas_heavy, slot_heavy) = run_oracle_tx(heavy_external_envs());

    assert_eq!(
        gas_light, gas_heavy,
        "exempt system tx gas must be independent of SALT bucket capacity (light={gas_light}, heavy={gas_heavy})",
    );
    assert_eq!(
        slot_light, slot_heavy,
        "resulting oracle state must be identical regardless of bucket capacity"
    );
}
