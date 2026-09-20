//! The two ways to reach a block executor, and the limits both install.
//!
//! A block's transaction-level limits live in its execution context, and the EVM is what
//! enforces them. Whichever route builds the executor — the factory trait a node's block-building
//! and reorg paths call with an EVM they built, or the trusted-inspector constructor — must
//! install them, so a caller that did not pre-apply them does not run a block under whatever
//! limits the EVM happened to carry.

use alloy_evm::{block::BlockExecutorFactory, EvmFactory};
use mega_evm::{test_utils::GasInspector, BlockLimits, DeclaredObserver, EvmTxRuntimeLimits};

use crate::common;

/// A limit no default carries, so seeing it proves it travelled from the block context.
const TX_DATA_SIZE_LIMIT: u64 = 1_234_567;

/// A second one, so a route that installs only the first is caught.
const FRAME_DATA_SIZE_LIMIT: u64 = 7_654_321;

fn limits() -> BlockLimits {
    BlockLimits::no_limits().with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(TX_DATA_SIZE_LIMIT)
            .with_frame_data_size_limit(FRAME_DATA_SIZE_LIMIT),
    )
}

/// The factory trait installs the block context's limits on an EVM the caller built without
/// them.
#[test]
fn test_trait_path_applies_block_context_runtime_limits() {
    let mut state = common::state();
    let factory = common::factory();
    // Built without any caller-side limits: the case a caller that forgot them produces.
    let evm = factory.evm_factory().create_evm(&mut state, common::evm_env());
    assert_eq!(evm.tx_runtime_limits().tx_data_size_limit, u64::MAX);

    let executor = factory.create_executor(evm, common::block_ctx(limits()));

    assert_eq!(executor.evm().tx_runtime_limits().tx_data_size_limit, TX_DATA_SIZE_LIMIT);
}

/// Every field travels, not just the one a test happens to look at.
#[test]
fn test_trait_path_applies_every_runtime_limit_field() {
    let mut state = common::state();
    let factory = common::factory();
    let evm = factory.evm_factory().create_evm(&mut state, common::evm_env());

    let executor = factory.create_executor(evm, common::block_ctx(limits()));

    assert_eq!(
        *executor.evm().tx_runtime_limits(),
        limits().to_evm_tx_runtime_limits(),
        "the EVM runs under exactly the block's limits"
    );
}

/// The two routes to an executor install the same limits, so neither is a way around them.
#[test]
fn test_trusted_inspector_and_trait_paths_apply_same_runtime_limits() {
    let trait_path = {
        let mut state = common::state();
        let factory = common::factory();
        let evm = factory.evm_factory().create_evm(&mut state, common::evm_env());
        let executor = factory.create_executor(evm, common::block_ctx(limits()));
        *executor.evm().tx_runtime_limits()
    };

    let inspector_path = {
        let mut state = common::state();
        let factory = common::factory();
        let executor = factory.create_executor_with_trusted_inspector(
            &mut state,
            common::evm_env(),
            common::block_ctx(limits()),
            DeclaredObserver::new(GasInspector::new()),
        );
        *executor.evm().tx_runtime_limits()
    };

    assert_eq!(trait_path, limits().to_evm_tx_runtime_limits());
    assert_eq!(inspector_path, limits().to_evm_tx_runtime_limits());
    assert_eq!(trait_path, inspector_path);
}

/// Installing them again over the same values changes nothing, so a caller that did pre-apply
/// them is not penalised for it.
#[test]
fn test_trait_path_idempotent_when_caller_pre_applied_runtime_limits() {
    let mut state = common::state();
    let factory = common::factory();
    let evm = factory
        .evm_factory()
        .create_evm(&mut state, common::evm_env())
        .with_tx_runtime_limits(limits().to_evm_tx_runtime_limits());

    let executor = factory.create_executor(evm, common::block_ctx(limits()));

    assert_eq!(*executor.evm().tx_runtime_limits(), limits().to_evm_tx_runtime_limits());
}
