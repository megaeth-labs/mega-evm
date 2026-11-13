//! The activation-block rule as a node applies it.
//!
//! Block execution works out for itself whether a block admits only deposits and whether it is
//! Satin's activation block, from its chain schedule, the block's own timestamp and the parent
//! block's timestamp ([`MegaBlockExecutionCtx::new`]), which is all a node
//! supplies. The tests here build each block's context from the parent's timestamp alone and run
//! the block: a user transaction is refused exactly in the blocks the rule names, and a deposit
//! runs in every block.

use alloy_evm::block::{BlockExecutionError, BlockExecutor, BlockValidationError};
use alloy_hardforks::{EthereumHardfork, ForkCondition};
use alloy_op_hardforks::OpHardfork;
use alloy_primitives::{Bytes, B256, U256};
use mega_evm::{
    system::SYSTEM_CONTRACT_COUNT,
    test_utils::{CommittedStates, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutionError, MegaHardfork, MegaHardforkConfig,
};
use revm::{context::ContextSetters, context_interface::ContextTr, database::State};

use crate::common::{
    self, block_ctx, deposit_tx, empty_call_gas, executor_with_env_and_spec, user_tx,
};

/// The timestamp the fork each case schedules activates at.
const ACTIVATION: u64 = common::BLOCK_TIMESTAMP;

/// A timestamp after [`ACTIVATION`] that a case moves a later fork to, so the schedule keeps the
/// forks in their order without that fork activating in the blocks the case runs.
const LATER: u64 = ACTIVATION + 1_000;

/// A schedule with every Ethereum and Optimism fork up to Karst at genesis and Satin at `satin`.
fn schedule(satin: u64) -> MegaHardforkConfig {
    MegaHardforkConfig::default().with(MegaHardfork::Satin, ForkCondition::Timestamp(satin))
}

/// The context of a block whose parent is at `parent_timestamp`. Nothing else is said: the
/// executor works out from its schedule and the block's own timestamp whether a fork activates
/// in the block.
fn node_ctx(parent_timestamp: u64) -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
        Some(parent_timestamp),
    )
}

/// Whether `error` is the activation-block refusal.
fn is_activation_refusal(error: &BlockExecutionError) -> bool {
    matches!(
        error.as_validation(),
        Some(BlockValidationError::Other(error))
            if error.downcast_ref::<MegaBlockExecutionError>()
                == Some(&MegaBlockExecutionError::UnexpectedNonDepositTxInActivationBlock)
    )
}

/// Runs the block at `timestamp` on the chain `spec` describes, its parent at
/// `parent_timestamp`: a user transaction, then a deposit. Answers whether the user transaction
/// was refused by the activation-block rule, having checked that the deposit runs either way and
/// that a user transaction the rule admits runs.
fn refuses_a_user_transaction(
    name: &str,
    spec: MegaHardforkConfig,
    parent_timestamp: u64,
    timestamp: u64,
) -> bool {
    let mut env = common::evm_env();
    env.block_env.timestamp = U256::from(timestamp);
    let ctx = node_ctx(parent_timestamp);
    let mut state = common::state();
    let mut executor = executor_with_env_and_spec(&mut state, ctx, env, spec);
    executor.apply_pre_execution_changes().unwrap_or_else(|e| panic!("{name}: starts: {e}"));

    let refused = match executor.execute_transaction(&user_tx(0, empty_call_gas())) {
        Ok(_) => false,
        Err(error) => {
            assert!(is_activation_refusal(&error), "{name}: refused for another reason: {error}");
            true
        }
    };
    executor
        .execute_transaction(&deposit_tx(Bytes::new(), empty_call_gas()))
        .unwrap_or_else(|e| panic!("{name}: a deposit runs in every block: {e}"));

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    let packed = if refused { 1 } else { 2 };
    assert_eq!(result.receipts().len(), packed, "{name}: what the block packed");
    assert!(result.receipts().iter().all(|receipt| receipt.status()), "{name}: both succeed");
    refused
}

/// [S1.8] [S21.12] [S1] A node's context, built from its schedule and the parent's timestamp,
/// refuses a user transaction in the block in which Satin or an Optimism fork at or after Jovian
/// activates, and in no other block; a deposit runs in all of them.
///
/// A fork activates in a block when it is active at the block's timestamp and not at the
/// parent's: the first block at the activation, and the first block after a gap over it, but
/// not the block after the activation block. Isthmus, the Optimism fork before Jovian, and an
/// Ethereum fork are not in the rule. Each case keeps the Optimism forks in their order, moving
/// the ones after the activating fork to a later timestamp.
///
/// Expected values: independent (the rule's clauses, case by case).
#[test]
fn test_a_node_context_refuses_a_user_transaction_where_a_fork_activates() {
    let at = ACTIVATION;
    let satin_at_genesis = || schedule(0);
    let cases = [
        ("Satin's activation block", schedule(at), at - 1, at, true),
        (
            "the first block after a gap over Satin's activation",
            schedule(at),
            at - 10,
            at + 5,
            true,
        ),
        ("the block after Satin's activation block", schedule(at), at, at + 1, false),
        (
            "Jovian's activation block",
            satin_at_genesis()
                .with(OpHardfork::Jovian, ForkCondition::Timestamp(at))
                .with(OpHardfork::Karst, ForkCondition::Timestamp(LATER)),
            at - 1,
            at,
            true,
        ),
        (
            "the block after Jovian's activation block",
            satin_at_genesis()
                .with(OpHardfork::Jovian, ForkCondition::Timestamp(at))
                .with(OpHardfork::Karst, ForkCondition::Timestamp(LATER)),
            at,
            at + 1,
            false,
        ),
        (
            "Isthmus's activation block",
            satin_at_genesis()
                .with(OpHardfork::Isthmus, ForkCondition::Timestamp(at))
                .with(OpHardfork::Jovian, ForkCondition::Timestamp(LATER))
                .with(OpHardfork::Karst, ForkCondition::Timestamp(LATER)),
            at - 1,
            at,
            false,
        ),
        (
            "Osaka's activation block",
            satin_at_genesis()
                .with(EthereumHardfork::Osaka, ForkCondition::Timestamp(at))
                .with(OpHardfork::Karst, ForkCondition::Timestamp(LATER)),
            at - 1,
            at,
            false,
        ),
    ];
    for (name, spec, parent, timestamp, expected) in cases {
        assert_eq!(refuses_a_user_transaction(name, spec, parent, timestamp), expected, "{name}");
    }
}

/// [S1] The rule follows the Optimism fork order the dependency exports: on a chain running Satin
/// from genesis, the block in which an Optimism fork activates refuses a user transaction exactly
/// when the fork is Jovian or comes after it in [`OpHardfork::VARIANTS`], and every fork the
/// dependency lists after Jovian is one.
///
/// The expected value is the fork's position against Jovian's, not the dependency's own
/// predicate. Expected values: independent.
#[test]
fn test_the_activation_rule_follows_the_optimism_fork_order() {
    let jovian = OpHardfork::VARIANTS
        .iter()
        .position(|fork| *fork == OpHardfork::Jovian)
        .expect("the dependency lists Jovian");
    let later = &OpHardfork::VARIANTS[jovian + 1..];
    assert!(!later.is_empty(), "the dependency lists a fork after Jovian");
    assert!(later.contains(&OpHardfork::Karst), "Karst, Satin's base, comes after Jovian");

    for (position, fork) in OpHardfork::VARIANTS.iter().enumerate() {
        let spec = schedule(0).with(*fork, ForkCondition::Timestamp(ACTIVATION));
        let name = fork.name();
        let refused = refuses_a_user_transaction(name, spec.clone(), ACTIVATION - 1, ACTIVATION);
        assert_eq!(refused, position >= jovian, "{name}'s activation block");
        assert!(
            !refuses_a_user_transaction(name, spec, ACTIVATION, ACTIVATION + 1),
            "{name}: the block after its activation block admits a user transaction"
        );
    }
}

/// The predeploys a node's context has the block check, from its schedule and the parent's
/// timestamp: how many of the states the pre-block steps committed are reads of a predeploy.
fn predeploys_checked(spec: MegaHardforkConfig, parent_timestamp: u64, timestamp: u64) -> usize {
    let mut env = common::evm_env();
    env.block_env.timestamp = U256::from(timestamp);
    let ctx = node_ctx(parent_timestamp);
    let mut state = common::state();
    let committed = CommittedStates::install(&mut state);
    let mut executor = executor_with_env_and_spec(&mut state, ctx, env, spec);
    executor.apply_pre_execution_changes().expect("the block starts");
    committed.states().iter().filter(|state| common::is_predeploy_check(state)).count()
}

/// [S18] A node's context has the predeploys checked in Satin's activation block and in no other:
/// the first block at the activation and the first after a gap over it read all seven; the block
/// after the activation block, every block of a chain with Satin from genesis, and Jovian's
/// activation block on such a chain, which admits only deposits, read none.
///
/// Expected values: independent (the schedule's own timestamps and the rule).
#[test]
fn test_a_node_context_checks_the_predeploys_where_satin_activates() {
    let at = ACTIVATION;
    let cases = [
        ("Satin's activation block", schedule(at), at - 1, at, SYSTEM_CONTRACT_COUNT),
        ("the first block after a gap", schedule(at), at - 10, at + 5, SYSTEM_CONTRACT_COUNT),
        ("the block after Satin's activation block", schedule(at), at, at + 1, 0),
        ("a block of a chain with Satin from genesis", schedule(0), at - 1, at, 0),
        (
            "Jovian's activation block",
            schedule(0)
                .with(OpHardfork::Jovian, ForkCondition::Timestamp(at))
                .with(OpHardfork::Karst, ForkCondition::Timestamp(LATER)),
            at - 1,
            at,
            0,
        ),
    ];
    for (name, spec, parent, timestamp, expected) in cases {
        assert_eq!(predeploys_checked(spec, parent, timestamp), expected, "{name}");
    }
}

/// The `MegaBlockExecutionError` an internal error carries.
fn internal(error: &BlockExecutionError) -> Option<&MegaBlockExecutionError> {
    error.as_internal().and_then(|error| error.downcast_other::<MegaBlockExecutionError>())
}

/// An executor for the block at `timestamp` on the chain `spec` describes, whose context names
/// `parent_timestamp`.
fn executor_at<'a>(
    state: &'a mut State<MemoryDatabase>,
    spec: MegaHardforkConfig,
    parent_timestamp: Option<u64>,
    timestamp: u64,
) -> common::TestExecutor<'a> {
    let mut env = common::evm_env();
    env.block_env.timestamp = U256::from(timestamp);
    let mut ctx = block_ctx(BlockLimits::no_limits());
    ctx.parent_timestamp = parent_timestamp;
    executor_with_env_and_spec(state, ctx, env, spec)
}

/// A context that names no parent timestamp, for a block in which a fork the schedule activates
/// after genesis is active, cannot be told from a block that has a parent: whether the fork
/// activates in it depends on the parent. Executing such a block is refused with a typed error,
/// by the block's start, by every transaction and by both rules, instead of running the block as
/// an ordinary one. The same block with the parent named is the activation block.
///
/// Expected values: independent (the schedule's own timestamps and the documented refusal).
#[test]
fn test_an_absent_parent_timestamp_is_refused_where_it_could_change_the_answer() {
    let at = ACTIVATION;
    let missing = MegaBlockExecutionError::MissingParentTimestamp { block_timestamp: at };

    let mut state = common::state();
    let mut executor = executor_at(&mut state, schedule(at), None, at);
    assert_eq!(executor.admits_only_deposits(), Err(missing.clone()));
    assert_eq!(executor.is_satin_activation_block(), Err(missing.clone()));
    let error = executor.apply_pre_execution_changes().expect_err("the block does not start");
    assert_eq!(internal(&error), Some(&missing), "an internal error: the caller's omission");
    assert_eq!(
        internal(&error).unwrap().to_string(),
        format!(
            "the parent block's timestamp is missing: a fork the schedule activates after \
             genesis is active at the block's timestamp {at}, and whether it activates in this \
             block depends on the parent's timestamp"
        ),
        "the diagnostic identifies the missing context and the affected block"
    );
    let error = executor
        .execute_transaction(&deposit_tx(Bytes::new(), empty_call_gas()))
        .expect_err("not even a deposit runs");
    assert_eq!(internal(&error), Some(&missing));

    let mut state = common::state();
    let executor = executor_at(&mut state, schedule(at), Some(at - 1), at);
    assert_eq!(
        (executor.admits_only_deposits(), executor.is_satin_activation_block()),
        (Ok(true), Ok(true)),
        "the same block with its parent named is the activation block"
    );
}

/// A context that names no parent timestamp is accepted where no parent could change the answer,
/// and then neither rule applies: a schedule whose forks are all active from genesis, a schedule
/// with no `MegaETH` fork, and a block before the fork's activation. The block runs a user
/// transaction.
///
/// Expected values: independent (the schedule's own timestamps and the rule's clauses).
#[test]
fn test_an_absent_parent_timestamp_is_accepted_where_no_parent_could_change_the_answer() {
    let at = ACTIVATION;
    let cases = [
        ("Satin from genesis", schedule(0), at),
        ("no MegaETH fork", MegaHardforkConfig::default(), at),
        ("a block before the activation", schedule(at), at - 1),
    ];
    for (name, spec, timestamp) in cases {
        let mut state = common::state();
        let mut executor = executor_at(&mut state, spec, None, timestamp);
        assert_eq!(executor.admits_only_deposits(), Ok(false), "{name}");
        assert_eq!(executor.is_satin_activation_block(), Ok(false), "{name}");
        executor.apply_pre_execution_changes().unwrap_or_else(|e| panic!("{name}: starts: {e}"));
        executor
            .execute_transaction(&user_tx(0, empty_call_gas()))
            .unwrap_or_else(|e| panic!("{name}: a user transaction runs: {e}"));
    }
}

/// Both rules are decided when the block starts and not again for each transaction: moving the
/// block environment's timestamp after the executor was built changes neither answer, and the
/// transactions after it are held to the rule the block began with.
///
/// Expected values: independent (the schedule's own timestamps).
#[test]
fn test_the_activation_rules_are_decided_once_for_the_block() {
    let at = ACTIVATION;
    let mut state = common::state();
    let mut executor = executor_at(&mut state, schedule(at), Some(at - 1), at);
    let mut block = executor.evm().ctx().block().clone();
    block.timestamp = U256::from(at + 1_000);
    executor.evm_mut().ctx_mut().set_block(block);
    assert_eq!(executor.admits_only_deposits(), Ok(true), "decided for the timestamp it began at");
    assert_eq!(executor.is_satin_activation_block(), Ok(true));
    executor.apply_pre_execution_changes().expect("the block starts");
    let error = executor
        .execute_transaction(&user_tx(0, empty_call_gas()))
        .expect_err("a user transaction is still refused");
    assert!(is_activation_refusal(&error), "{error}");
}
