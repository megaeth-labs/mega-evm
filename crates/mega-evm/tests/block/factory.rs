//! The two ways to reach a block executor, and the limits both install.
//!
//! The limits a block's transactions run under are the spec's ([`ProtocolLimits::for_spec`]), and
//! the EVM is what enforces them. Whichever route builds the executor — the constructor a node
//! calls directly, the factory trait its block-building and reorg paths call with an EVM they
//! built, or the trusted-inspector constructor — installs them, so a caller that applied others
//! to the EVM does not run a block under whatever limits the EVM happened to carry.

use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    EvmFactory,
};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::U256;
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{BytecodeBuilder, GasInspector},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaBlockExecutor, MegaEvmFactory,
    MegaLimitExceeded, MegaSpecId, ProtocolLimits,
};
use revm::{context::result::ExecutionResult, database::State};

use crate::common::{self, user_tx};

/// The limits every route installs: Satin's.
fn limits() -> ProtocolLimits {
    ProtocolLimits::for_spec(MegaSpecId::SATIN)
}

/// Limits a caller applied to the EVM, which are not the spec's: every field differs.
fn others() -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::no_limits()
        .with_tx_data_size_limit(1_234_567)
        .with_frame_data_size_limit(7_654_321)
        .with_block_env_access_compute_gas_limit(1)
        .with_oracle_access_compute_gas_limit(2)
}

/// The factory trait installs the spec's limits, every field of them, on an EVM the caller built
/// with others. An EVM the EVM factory creates runs under them before any executor installs them.
#[test]
fn test_trait_path_applies_the_specs_runtime_limits() {
    let mut state = common::state();
    let factory = common::factory();
    let evm = factory.evm_factory().create_evm(&mut state, common::evm_env());
    assert_eq!(*evm.tx_runtime_limits(), limits().tx_runtime_limits, "before any executor");
    let evm = evm.with_tx_runtime_limits(others());

    let executor = factory.create_executor(evm, common::unlimited_ctx());

    assert_eq!(
        *executor.evm().tx_runtime_limits(),
        limits().tx_runtime_limits,
        "the EVM runs under exactly the spec's limits"
    );
    assert_eq!(executor.protocol_limits(), &limits());
}

/// The routes to an executor install the same limits, so neither is a way around them.
#[test]
fn test_inspector_and_plain_paths_apply_same_runtime_limits() {
    let trait_path = {
        let mut state = common::state();
        let factory = common::factory();
        let evm = factory
            .evm_factory()
            .create_evm(&mut state, common::evm_env())
            .with_tx_runtime_limits(others());
        let executor = factory.create_executor(evm, common::unlimited_ctx());
        *executor.evm().tx_runtime_limits()
    };

    let inspector_path = {
        let mut state = common::state();
        let factory = common::factory();
        let evm = factory.evm_factory().create_evm_with_inspector(
            &mut state,
            common::evm_env(),
            GasInspector::new(),
        );
        let executor = factory.create_executor(evm, common::unlimited_ctx());
        *executor.evm().tx_runtime_limits()
    };

    assert_eq!(trait_path, limits().tx_runtime_limits);
    assert_eq!(inspector_path, limits().tx_runtime_limits);
}

/// The constructor a node calls directly installs the spec's limits, so it and the factory
/// routes agree.
#[test]
fn test_direct_construction_applies_the_specs_runtime_limits() {
    let mut state = common::state();
    let evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_tx_runtime_limits(others());

    let executor = MegaBlockExecutor::new(
        evm,
        common::unlimited_ctx(),
        common::chain_spec(),
        OpAlloyReceiptBuilder::default(),
    );

    assert_eq!(
        *executor.evm().tx_runtime_limits(),
        limits().tx_runtime_limits,
        "the EVM runs under exactly the spec's limits"
    );
}

/// And they are enforced, not only carried: a transaction that keeps more data-size bytes than the
/// block's limits allow is stopped with `MegaLimitExceeded`, even when the caller loosened the
/// EVM's limits after building the executor — every transaction runs under the block's.
#[test]
fn test_direct_construction_stops_a_transaction_over_the_blocks_data_size_limit() {
    // The body is 310 bytes. One storage write fits; the second crosses.
    const DATA_SIZE_LIMIT: u64 = mega_evm::TX_BODY_SIZE + 40;

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
    // `common::executor_with_limits` builds the executor with `MegaBlockExecutor::new`, and
    // holds the block to these limits in place of the spec's.
    let mut executor = common::executor_with_limits(
        &mut state,
        common::loosest()
            .with_tx_runtime_limits(common::loosest_tx().with_tx_data_size_limit(DATA_SIZE_LIMIT)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.evm_mut().set_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());

    // The second write is counted once its opcode completed, so the gas covers both slots, their
    // records and the body at the byte prices in effect.
    let gas_limit = 1_000_000 +
        2 * common::slot_state_gas() +
        common::body_history(0) +
        mega_evm::write_record_history_gas(2).expect("two records have a price");
    let outcome = executor
        .execute_transaction_without_commit(&user_tx(0, gas_limit))
        .expect("the transaction runs");

    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: DATA_SIZE_LIMIT - mega_evm::TX_BODY_SIZE,
            used: 2 * 40,
            frame_local: true,
        }),
        "the transaction's own frame crosses its budget, what the body leaves of the limit"
    );
    match &outcome.result {
        ExecutionResult::Revert { output, .. } => assert_eq!(
            output.as_ref(),
            MegaLimitExceeded {
                kind: LimitKind::DataSize.as_u8(),
                limit: DATA_SIZE_LIMIT - mega_evm::TX_BODY_SIZE,
            }
            .abi_encode(),
            "the stop's revert data"
        ),
        other => panic!("expected a revert-class stop, got {other:?}"),
    }
}
