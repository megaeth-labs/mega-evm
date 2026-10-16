//! The two ways to reach a block executor, and the limits both install.
//!
//! A block's transaction-level limits live in its execution context, and the EVM is what
//! enforces them. Whichever route builds the executor — the constructor a node calls directly,
//! the factory trait its block-building and reorg paths call with an EVM they built, or the
//! trusted-inspector constructor — installs them, so a caller that did not pre-apply them does
//! not run a block under whatever limits the EVM happened to carry.

use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    EvmFactory,
};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::U256;
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{BytecodeBuilder, GasInspector},
    BlockLimits, DeclaredObserver, EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaBlockExecutor,
    MegaEvmFactory, MegaLimitExceeded,
};
use revm::{context::result::ExecutionResult, database::State};

use crate::common::{self, user_tx};

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

/// The constructor a node calls directly installs the block's limits, so it and the factory
/// routes agree.
#[test]
fn test_direct_construction_applies_block_context_runtime_limits() {
    let mut state = common::state();
    // Built without any caller-side limits: the case a caller that forgot them produces.
    let evm = MegaEvmFactory::new().create_evm(&mut state, common::evm_env());
    assert_eq!(evm.tx_runtime_limits().tx_data_size_limit, u64::MAX);

    let executor = MegaBlockExecutor::new(
        evm,
        common::block_ctx(limits()),
        common::chain_spec(),
        OpAlloyReceiptBuilder::default(),
    );

    assert_eq!(
        *executor.evm().tx_runtime_limits(),
        limits().to_evm_tx_runtime_limits(),
        "the EVM runs under exactly the block's limits"
    );
}

/// And they are enforced, not only carried: a transaction that keeps more data-size bytes than
/// the block allows is stopped with `MegaLimitExceeded`.
#[test]
fn test_direct_construction_stops_a_transaction_over_the_block_data_size_limit() {
    const DATA_SIZE_LIMIT: u64 = 40;

    let mut db = common::database();
    db.set_account_code(
        common::CONTRACT,
        BytecodeBuilder::default()
            .sstore(U256::from(1), U256::from(1))
            .sstore(U256::from(2), U256::from(1))
            .stop()
            .build(),
    );
    let mut state = State::builder().with_database(db).build();
    let block_limits = BlockLimits::no_limits().with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(DATA_SIZE_LIMIT),
    );
    // `common::executor` builds the executor with `MegaBlockExecutor::new` and an EVM that
    // carries no limits of its own.
    let mut executor = common::executor(&mut state, common::block_ctx(block_limits));
    executor.apply_pre_execution_changes().expect("the block starts");

    let outcome = executor
        .execute_transaction_without_commit(&user_tx(0, 1_000_000))
        .expect("the transaction runs");

    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: DATA_SIZE_LIMIT,
            used: 2 * DATA_SIZE_LIMIT,
            frame_local: false,
        })
    );
    match &outcome.result {
        ExecutionResult::Revert { output, .. } => assert_eq!(
            output.as_ref(),
            MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: DATA_SIZE_LIMIT }
                .abi_encode(),
            "the stop's revert data"
        ),
        other => panic!("expected a revert-class stop, got {other:?}"),
    }
}
