//! End-to-end regression tests for the REX5 `applyPendingChanges()` system
//! call gas budget.
//!
//! Pre-fix: the pre-block `applyPendingChanges()` call ran with revm's
//! hard-coded 30M default. On a REX5 activation block where SALT bucket
//! capacity inflates the role-rotation `SSTORE`s past 30M, the call would OOG
//! and the rotation would never apply.
//!
//! Post-fix: `transact_apply_pending_changes` issues the call with
//! `max(block.gas_limit, SYSTEM_CALL_GAS_LIMIT_FLOOR)`, so any block whose
//! live budget covers the role-rotation cost succeeds.
//!
//! The apply-pending-change tests pin both halves of the invariant under one shared
//! heavy-SALT scenario:
//!
//! - [`test_rex5_apply_pending_changes_succeeds_under_heavy_storage_gas`] exercises the production
//!   path end-to-end via the block executor and asserts the rotation commits. A regression that
//!   routes the call back through the upstream `transact_system_call` (30M cap) fails here even
//!   though the unit tests in `system/sequencer_registry.rs` still pass.
//! - [`test_rex5_apply_pending_changes_oogs_under_upstream_30m_cap`] pins the *baseline* — that the
//!   upstream 30M cap really is insufficient under this scenario — by invoking the system call
//!   directly with the upstream default. If that test ever starts to pass (e.g. because role
//!   rotation becomes cheap), the fix above no longer protects anything and the scenario itself
//!   needs to be re-tuned.
//!
//! The EIP-2935 and EIP-4788 tests pin the isolation invariant: REX5 widens only
//! `SequencerRegistry.applyPendingChanges()`. The default `transact_system_call`
//! path used by the upstream EIP pre-block helpers must keep the historical 30M
//! budget, even when the live block gas limit is higher.

use std::convert::Infallible;

use alloy_evm::{block::BlockExecutor, Evm, EvmEnv};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, BucketHasher, BucketId, EthHaltReason,
    MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaContext, MegaEvm, MegaEvmFactory,
    MegaHaltReason, MegaHardfork, MegaHardforkConfig, MegaSpecId, OpHaltReason,
    SequencerRegistryConfig, TestExternalEnvs, MEGA_SYSTEM_ADDRESS, SEQUENCER_REGISTRY_ADDRESS,
    SEQUENCER_REGISTRY_CODE, SEQUENCER_REGISTRY_CODE_HASH,
};
use mega_system_contracts::sequencer_registry::{
    storage_slots::{
        CURRENT_SYSTEM_ADDRESS, PENDING_SYSTEM_ADDRESS, SYSTEM_ADDRESS_ACTIVATION_BLOCK,
    },
    ISequencerRegistry,
};
use revm::{
    context::{result::ExecutionResult, BlockEnv},
    database::{Database as _, State},
    handler::EvmTr,
    state::{AccountInfo, Bytecode},
};

const NEW_SYSTEM_ADDRESS: Address = address!("3000000000000000000000000000000000000003");
const BOOTSTRAP_SEQUENCER: Address = address!("4000000000000000000000000000000000000004");
const BOOTSTRAP_ADMIN: Address = address!("5000000000000000000000000000000000000005");

const ACTIVATION_BLOCK: u64 = 1000;

/// Bucket that every SALT lookup is routed to under [`SingleBucketHasher`].
const HEAVY_BUCKET_ID: BucketId = 100_000;
/// Capacity = 2000 × `MIN_BUCKET_SIZE` (256). Yields a 2000× multiplier, so each
/// zero→nonzero `SSTORE` charges ≈ `20_000 × 1999` ≈ 40M of dynamic storage gas
/// (REX `SSTORE_SET_STORAGE_GAS_BASE × (multiplier - 1)`). `applyPendingChanges()`
/// triggers two such writes (history-length slot and the first history element),
/// totaling ≈ 80M — well above the upstream 30M cap and well below the 250M
/// block budget below.
const HEAVY_BUCKET_CAPACITY: u64 = 512_000;
/// Block gas limit chosen high enough to cover the inflated rotation cost.
const BLOCK_GAS_LIMIT: u64 = 250_000_000;

/// Routes every account/slot to a single bucket so that one
/// `with_bucket_capacity()` call deterministically inflates every `SSTORE`
/// the system call performs, regardless of the slot keys involved.
#[derive(Debug, Clone, Copy)]
struct SingleBucketHasher;

impl BucketHasher for SingleBucketHasher {
    fn bucket_id(_key: &[u8]) -> BucketId {
        HEAVY_BUCKET_ID
    }
}

fn sequencer_registry_config() -> SequencerRegistryConfig {
    SequencerRegistryConfig {
        initial_system_address: MEGA_SYSTEM_ADDRESS,
        initial_sequencer: BOOTSTRAP_SEQUENCER,
        initial_admin: BOOTSTRAP_ADMIN,
    }
}

fn create_evm_env(gas_limit: u64) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::REX5;
    let block_env = BlockEnv {
        number: U256::from(ACTIVATION_BLOCK),
        timestamp: U256::from(1_800_000_000),
        gas_limit,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

/// Pre-deploys `SequencerRegistry` with a pending system address change due
/// at [`ACTIVATION_BLOCK`] and funds both system addresses. Deploy is skipped
/// during pre-execution because the account already has the registry code, so
/// the heavy SALT setup only bites the SSTOREs from `applyPendingChanges()`.
fn seed_db_with_pending_change() -> MemoryDatabase {
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
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SYSTEM_ADDRESS,
        MEGA_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        PENDING_SYSTEM_ADDRESS,
        NEW_SYSTEM_ADDRESS.into_word().into(),
    )
    .unwrap();
    db.insert_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        U256::from(ACTIVATION_BLOCK),
    )
    .unwrap();

    db
}

/// Heavy SALT environment: the single bucket is configured with a 2000×
/// multiplier, so any zero→nonzero `SSTORE` performed by
/// `applyPendingChanges()` costs ≈ 40M dynamic storage gas.
fn heavy_external_envs() -> TestExternalEnvs<Infallible, SingleBucketHasher> {
    TestExternalEnvs::<Infallible, SingleBucketHasher>::new()
        .with_bucket_capacity(HEAVY_BUCKET_ID, HEAVY_BUCKET_CAPACITY)
}

fn assert_30m_system_call_oog(result: &ExecutionResult<MegaHaltReason>, system_call_name: &str) {
    assert!(
        matches!(
            result,
            ExecutionResult::Halt {
                reason: MegaHaltReason::Base(OpHaltReason::Base(EthHaltReason::OutOfGas(_))),
                ..
            }
        ),
        "{system_call_name} must halt with OOG under the upstream 30M cap with heavy SALT. \
         Result: {result:?}",
    );
}

/// Without the fix, `applyPendingChanges()` would OOG on the first
/// zero→nonzero `SSTORE` (≈ 40M storage gas alone, > the upstream 30M cap)
/// and `apply_pre_execution_changes()` would error. With the fix, the live
/// 250M block budget is used and the rotation commits.
#[test]
fn test_rex5_apply_pending_changes_succeeds_under_heavy_storage_gas() {
    let mut db = seed_db_with_pending_change();
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(heavy_external_envs());
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

    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx,
        create_evm_env(BLOCK_GAS_LIMIT),
    );
    executor.apply_pre_execution_changes().expect(
        "applyPendingChanges() must succeed under the live block gas budget — \
         a regression here means the system call was capped at the upstream 30M default",
    );

    // The system address rotation only takes effect if `applyPendingChanges()` ran
    // to completion. This proves the inflated SSTOREs were actually charged and
    // committed, not that the call simply early-returned.
    let resolved = executor.evm().ctx_ref().system_address();
    assert_eq!(resolved, NEW_SYSTEM_ADDRESS, "Pending system address change must be applied");

    assert_eq!(
        executor
            .evm_mut()
            .db_mut()
            .storage(SEQUENCER_REGISTRY_ADDRESS, PENDING_SYSTEM_ADDRESS)
            .unwrap(),
        U256::ZERO,
        "Pending system address slot must be cleared after applyPendingChanges() commits",
    );
    assert_eq!(
        executor
            .evm_mut()
            .db_mut()
            .storage(SEQUENCER_REGISTRY_ADDRESS, SYSTEM_ADDRESS_ACTIVATION_BLOCK)
            .unwrap(),
        U256::ZERO,
        "System address activation slot must be cleared after applyPendingChanges() commits",
    );
}

/// Pins the regression *baseline*: under the same heavy-SALT scenario, a
/// system call issued with the upstream-fixed 30M cap MUST fail. If this test
/// ever starts to pass, role rotation has become cheap enough for the
/// upstream default and the success test above no longer proves the fix is
/// load-bearing — the SALT setup needs to be re-tuned (or the fix retired).
///
/// The literal `30_000_000` here is intentional: it mirrors revm's
/// hard-coded `SystemCallTx::new_system_tx_with_caller` gas limit and the
/// constant in [`mega_evm::constants::rex5::SYSTEM_CALL_GAS_LIMIT_FLOOR`].
/// We don't import the constant so that this assertion is a structural
/// check on the upstream literal, not a circular check on our floor.
#[test]
fn test_rex5_apply_pending_changes_oogs_under_upstream_30m_cap() {
    let mut db = seed_db_with_pending_change();

    let context = MegaContext::new(&mut db, MegaSpecId::REX5)
        .with_external_envs(heavy_external_envs().into())
        .with_block(BlockEnv {
            number: U256::from(ACTIVATION_BLOCK),
            timestamp: U256::from(1_800_000_000),
            gas_limit: BLOCK_GAS_LIMIT,
            ..Default::default()
        });
    let mut evm = MegaEvm::new(context);

    let calldata = ISequencerRegistry::applyPendingChangesCall {}.abi_encode();
    let result = evm
        .transact_system_call_with_gas_limit(
            alloy_eips::eip4788::SYSTEM_ADDRESS,
            SEQUENCER_REGISTRY_ADDRESS,
            Bytes::from(calldata),
            30_000_000, // revm upstream's hard-coded system-call gas limit.
        )
        .expect("system call should not surface an EVMError");

    assert_30m_system_call_oog(&result.result, "applyPendingChanges()");
}

/// REX5 only widens `SequencerRegistry.applyPendingChanges()`. The EIP-2935
/// default system-call path must keep using revm's 30M budget, even when the
/// live block gas limit is larger.
#[test]
fn test_rex5_eip2935_blockhashes_keeps_upstream_30m_cap_under_heavy_storage_gas() {
    let mut db = MemoryDatabase::default();
    let bytecode = Bytecode::new_raw(alloy_eips::eip2935::HISTORY_STORAGE_CODE.clone());
    db.insert_account_info(
        alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
        AccountInfo { code_hash: bytecode.hash_slow(), code: Some(bytecode), ..Default::default() },
    );
    let context = MegaContext::new(&mut db, MegaSpecId::REX5)
        .with_external_envs(heavy_external_envs().into())
        .with_block(BlockEnv {
            number: U256::from(ACTIVATION_BLOCK),
            timestamp: U256::from(1_800_000_000),
            gas_limit: BLOCK_GAS_LIMIT,
            ..Default::default()
        });
    let mut evm = MegaEvm::new(context);
    let result = evm
        .transact_system_call(
            alloy_eips::eip4788::SYSTEM_ADDRESS,
            alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
            B256::from([0x29; 32]).0.into(),
        )
        .expect("EIP-2935 system call should not surface an EVMError");

    assert_30m_system_call_oog(&result.result, "EIP-2935 blockhashes system call");
}

/// Same invariant for EIP-4788: REX5's widened budget must not leak into the
/// default beacon-roots system-call path.
#[test]
fn test_rex5_eip4788_beacon_root_keeps_upstream_30m_cap_under_heavy_storage_gas() {
    let mut db = MemoryDatabase::default();
    let bytecode = Bytecode::new_raw(alloy_eips::eip4788::BEACON_ROOTS_CODE.clone());
    db.insert_account_info(
        alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
        AccountInfo { code_hash: bytecode.hash_slow(), code: Some(bytecode), ..Default::default() },
    );
    let context = MegaContext::new(&mut db, MegaSpecId::REX5)
        .with_external_envs(heavy_external_envs().into())
        .with_block(BlockEnv {
            number: U256::from(ACTIVATION_BLOCK),
            timestamp: U256::from(1_800_000_000),
            gas_limit: BLOCK_GAS_LIMIT,
            ..Default::default()
        });
    let mut evm = MegaEvm::new(context);
    let result = evm
        .transact_system_call(
            alloy_eips::eip4788::SYSTEM_ADDRESS,
            alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
            B256::from([0x47; 32]).0.into(),
        )
        .expect("EIP-4788 system call should not surface an EVMError");

    assert_30m_system_call_oog(&result.result, "EIP-4788 beacon-roots system call");
}
