//! Tests for the `alloy_evm::block::BlockExecutorFactory` trait impl on
//! `MegaBlockExecutorFactory`.
//!
//! The factory exposes two paths that produce a `MegaBlockExecutor`:
//!
//! 1. The inherent `create_executor` / `create_executor_with_inspector` methods, which build the
//!    EVM internally and apply `block_ctx.block_limits.to_evm_tx_runtime_limits()` before
//!    constructing the executor.
//!
//! 2. The trait method `<MegaBlockExecutorFactory as
//!    alloy_evm::block::BlockExecutorFactory>::create_executor` which receives a prebuilt EVM and
//!    is what downstream block-builder / reorg paths in `mega-reth` call into.
//!
//! Historically only path (1) synchronized the EVM tx runtime limits with the
//! block-context limits, so callers of (2) that forgot to chain
//! `.with_tx_runtime_limits(...)` on the EVM ran against whatever default
//! limits the EVM was constructed with. These tests pin the invariant that the
//! trait impl now applies the same synchronization unconditionally, on every
//! spec, so the two construction routes are observationally equivalent.

use std::convert::Infallible;

use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    EvmEnv, EvmFactory,
};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{Bytes, B256, U256};
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, EvmTxRuntimeLimits, MegaBlockExecutionCtx,
    MegaBlockExecutorFactory, MegaEvmFactory, MegaHardfork, MegaHardforkConfig, MegaSpecId,
    TestExternalEnvs,
};
use revm::{context::BlockEnv, database::State, handler::EvmTr};

/// A deliberately non-default per-tx compute-gas cap. Chosen well below any
/// spec default and any saturation boundary so that all four tests use the
/// same recognizable value across specs.
const NON_DEFAULT_TX_COMPUTE_GAS_LIMIT: u64 = 1_234_567;

fn evm_env_for_spec(spec: MegaSpecId) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = spec;
    cfg_env.chain_id = 8453;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

fn chain_spec_at(hardfork: MegaHardfork) -> MegaHardforkConfig {
    MegaHardforkConfig::default().with(hardfork, ForkCondition::Timestamp(0))
}

fn block_ctx_with_compute_gas_limit(limit: u64) -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_tx_compute_gas_limit(limit),
    )
}

/// The trait factory must apply `block_ctx.block_limits.to_evm_tx_runtime_limits()`
/// to the prebuilt EVM under REX5.
#[test]
fn test_trait_path_applies_block_context_compute_gas_limit_rex5() {
    let mut db = MemoryDatabase::default();
    let mut state = State::builder().with_database(&mut db).build();

    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = chain_spec_at(MegaHardfork::Rex5);
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let factory = MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    // Build the EVM without any caller-side .with_tx_runtime_limits(...) chain.
    // This is exactly the trigger condition of the original bug: a caller of
    // the trait factory that forgot to pre-apply the block-context limits.
    let evm = factory.evm_factory().create_evm(&mut state, evm_env_for_spec(MegaSpecId::REX5));

    let ctx = block_ctx_with_compute_gas_limit(NON_DEFAULT_TX_COMPUTE_GAS_LIMIT);

    // Route via the trait method. Fully-qualified syntax disambiguates from the
    // inherent `create_executor(db, ctx, evm_env)` method that takes three args.
    let executor = <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
        &factory, evm, ctx,
    );

    assert_eq!(
        executor.evm().ctx_ref().additional_limit.borrow().limits.tx_compute_gas_limit,
        NON_DEFAULT_TX_COMPUTE_GAS_LIMIT,
        "trait factory must propagate block_ctx.block_limits.tx_compute_gas_limit \
         to the prebuilt EVM's runtime limits under REX5",
    );
}

/// The same invariant must hold on a spec other than REX5. This is the
/// load-bearing assertion that the trait/inherent asymmetry has been
/// eliminated on every spec, not just on the latest one.
#[test]
fn test_trait_path_applies_block_context_compute_gas_limit_rex4() {
    let mut db = MemoryDatabase::default();
    let mut state = State::builder().with_database(&mut db).build();

    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = chain_spec_at(MegaHardfork::Rex4);
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let factory = MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let evm = factory.evm_factory().create_evm(&mut state, evm_env_for_spec(MegaSpecId::REX4));

    let ctx = block_ctx_with_compute_gas_limit(NON_DEFAULT_TX_COMPUTE_GAS_LIMIT);

    let executor = <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
        &factory, evm, ctx,
    );

    assert_eq!(
        executor.evm().ctx_ref().additional_limit.borrow().limits.tx_compute_gas_limit,
        NON_DEFAULT_TX_COMPUTE_GAS_LIMIT,
        "trait factory must propagate block_ctx.block_limits.tx_compute_gas_limit \
         to the prebuilt EVM's runtime limits under REX4",
    );
}

/// Inherent and trait construction routes must produce executors with the same
/// configured runtime limits for the same `BlockLimits`. This directly pins the
/// no-asymmetry invariant: there must be no observable difference between
/// `factory.create_executor(db, ctx, evm_env)` and
/// `<_ as BlockExecutorFactory>::create_executor(&factory, evm, ctx)` for
/// equivalent inputs.
#[test]
fn test_inherent_and_trait_paths_apply_same_runtime_limits() {
    let chain_spec = chain_spec_at(MegaHardfork::Rex5);
    let receipt_builder = OpAlloyReceiptBuilder::default();

    let inherent_limit: u64 = {
        let mut db = MemoryDatabase::default();
        let mut state = State::builder().with_database(&mut db).build();
        let external_envs = TestExternalEnvs::<Infallible>::new();
        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
        let factory =
            MegaBlockExecutorFactory::new(chain_spec.clone(), evm_factory, receipt_builder);

        let ctx = block_ctx_with_compute_gas_limit(NON_DEFAULT_TX_COMPUTE_GAS_LIMIT);
        let executor = factory.create_executor(&mut state, ctx, evm_env_for_spec(MegaSpecId::REX5));
        let limit = executor.evm().ctx_ref().additional_limit.borrow().limits.tx_compute_gas_limit;
        limit
    };

    let trait_limit: u64 = {
        let mut db = MemoryDatabase::default();
        let mut state = State::builder().with_database(&mut db).build();
        let external_envs = TestExternalEnvs::<Infallible>::new();
        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
        let factory = MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

        let evm = factory.evm_factory().create_evm(&mut state, evm_env_for_spec(MegaSpecId::REX5));
        let ctx = block_ctx_with_compute_gas_limit(NON_DEFAULT_TX_COMPUTE_GAS_LIMIT);
        let executor = <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
            &factory, evm, ctx,
        );
        let limit = executor.evm().ctx_ref().additional_limit.borrow().limits.tx_compute_gas_limit;
        limit
    };

    assert_eq!(
        inherent_limit, NON_DEFAULT_TX_COMPUTE_GAS_LIMIT,
        "inherent factory must apply the configured tx_compute_gas_limit",
    );
    assert_eq!(
        trait_limit, NON_DEFAULT_TX_COMPUTE_GAS_LIMIT,
        "trait factory must apply the configured tx_compute_gas_limit",
    );
    assert_eq!(
        inherent_limit, trait_limit,
        "inherent and trait construction routes must produce identical \
         tx_compute_gas_limit values for the same BlockLimits",
    );
}

/// If the caller has already chained `with_tx_runtime_limits(...)` on the EVM
/// before invoking the trait factory, the factory's own synchronization step
/// must be a semantic no-op: the configured limit (derived from the same
/// `block_ctx.block_limits`) is just re-applied at the same value. This pins
/// the idempotence property the unconditional-sync rationale relies on.
#[test]
fn test_trait_path_idempotent_when_caller_pre_applied_runtime_limits() {
    let mut db = MemoryDatabase::default();
    let mut state = State::builder().with_database(&mut db).build();

    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec = chain_spec_at(MegaHardfork::Rex5);
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let factory = MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    let block_limits =
        BlockLimits::no_limits().with_tx_compute_gas_limit(NON_DEFAULT_TX_COMPUTE_GAS_LIMIT);
    let pre_applied: EvmTxRuntimeLimits = block_limits.to_evm_tx_runtime_limits();

    // Caller pre-applies the same limits derived from block_limits — the
    // canonical pattern used today by mega-reth's RPC paths.
    let evm = factory
        .evm_factory()
        .create_evm(&mut state, evm_env_for_spec(MegaSpecId::REX5))
        .with_tx_runtime_limits(pre_applied);

    let ctx = MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), block_limits);

    let executor = <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
        &factory, evm, ctx,
    );

    assert_eq!(
        executor.evm().ctx_ref().additional_limit.borrow().limits.tx_compute_gas_limit,
        NON_DEFAULT_TX_COMPUTE_GAS_LIMIT,
        "trait factory's runtime-limit synchronization must be idempotent for \
         callers that already applied the same limits via with_tx_runtime_limits",
    );
}
