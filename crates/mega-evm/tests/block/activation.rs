//! The activation-block rule as a node applies it.
//!
//! Block execution does not work out whether a block admits only deposits: it is told, through
//! [`MegaBlockExecutionCtx::no_user_tx_activation_block`]. A node works it out from its schedule
//! and the parent block's timestamp ([`MegaHardforks::admits_only_deposits`]) for every block it
//! builds or validates. The tests here build each block's context that way, from a schedule, and
//! run the block: a user transaction is refused exactly in the blocks the rule names, and a
//! deposit runs in every block.

use alloy_evm::block::{BlockExecutionError, BlockExecutor, BlockValidationError};
use alloy_hardforks::{EthereumHardfork, ForkCondition};
use alloy_op_hardforks::OpHardfork;
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutionError, MegaHardfork, MegaHardforkConfig,
    MegaHardforks, ProtocolLimits,
};

use crate::common::{
    self, block_ctx, deposit_tx, empty_call_gas, executor_with_env_and_spec, registry_config,
    user_tx,
};

/// The timestamp the fork each case schedules activates at.
const ACTIVATION: u64 = common::BLOCK_TIMESTAMP;

/// A timestamp after [`ACTIVATION`] that a case moves a later fork to, so the schedule keeps the
/// forks in their order without that fork activating in the blocks the case runs.
const LATER: u64 = ACTIVATION + 1_000;

/// A schedule with every Ethereum and Optimism fork up to Karst at genesis, Satin at `satin`, the
/// registry parameters and the loosest limits a chain may carry.
fn schedule(satin: u64) -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with(MegaHardfork::Satin, ForkCondition::Timestamp(satin))
        .with_params(registry_config())
        .with_params(ProtocolLimits::loosest())
}

/// The context of the block at `timestamp`, whose parent is at `parent_timestamp`, as a node
/// builds it: the switch comes from the schedule and the parent's timestamp.
fn node_ctx(
    spec: &MegaHardforkConfig,
    parent_timestamp: u64,
    timestamp: u64,
) -> MegaBlockExecutionCtx {
    block_ctx(BlockLimits::no_limits())
        .with_no_user_tx_activation_block(spec.admits_only_deposits(parent_timestamp, timestamp))
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
    let ctx = node_ctx(&spec, parent_timestamp, timestamp);
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
