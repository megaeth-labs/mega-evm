//! The schedule a block runs on, and what is checked of it before the block starts.

use core::any::{Any, TypeId};

use alloy_evm::{
    block::{BlockExecutionError, BlockExecutor},
    Evm, EvmFactory,
};
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use mega_evm::{
    chain_activation, mainnet_hardforks, system::SequencerRegistryConfig, testnet_hardforks,
    HardforkParams, MegaBlockExecutionError, MegaBlockExecutor, MegaEvmFactory, MegaHardfork,
    MegaHardforkConfig, MegaHardforks, ProtocolLimits, ScheduleError, MAINNET_CHAIN_ID,
    TESTNET_CHAIN_ID,
};

use crate::common::{self, empty_call_gas, registry_config, user_tx};

/// The canonical schedules are the chain activation table: the two are one source, so a
/// timestamp cannot be right in one and wrong in the other. What the published upgrade pages
/// say about them is the specification's side, not this crate's.
#[test]
fn test_canonical_schedules_match_the_chain_activation_table() {
    for (chain_id, schedule) in
        [(MAINNET_CHAIN_ID, mainnet_hardforks()), (TESTNET_CHAIN_ID, testnet_hardforks())]
    {
        let activation = chain_activation(chain_id).expect("a known chain");
        for fork in MegaHardfork::VARIANTS {
            assert_eq!(
                schedule.mega_fork_activation(*fork),
                activation.condition(*fork),
                "{fork:?} on chain {chain_id}"
            );
            // The schedule carries an entry for every fork, including the ones the chain has not
            // scheduled: attaching a fork's parameters needs its entry to be there.
            assert_eq!(
                schedule.get(*fork),
                Some(&activation.condition(*fork)),
                "{fork:?} is registered on chain {chain_id}"
            );
        }
        assert_eq!(schedule.validate_schedule(), Ok(()), "chain {chain_id} loads");
    }

    // Satin is not scheduled on either chain yet, so neither runs this engine.
    assert_eq!(mainnet_hardforks().spec_id(u64::MAX), None);
    assert_eq!(testnet_hardforks().spec_id(u64::MAX), None);
}

/// A schedule that activates a fork without the parameters it requires fails when the chain
/// configuration is loaded, not at the fork's first block: the block executor is never built
/// from one.
#[test]
fn test_a_schedule_missing_its_fork_params_is_refused_at_load() {
    let missing =
        MegaHardforkConfig::default().with(MegaHardfork::Satin, ForkCondition::Timestamp(0));
    assert_eq!(
        missing.require_params::<SequencerRegistryConfig>(),
        Err(ScheduleError::MissingParams {
            fork: MegaHardfork::Satin,
            params: "SequencerRegistryConfig",
        }),
        "the load refuses it"
    );
    assert_eq!(
        missing.validate_schedule(),
        Err(ScheduleError::MissingParams {
            fork: MegaHardfork::Satin,
            params: "SequencerRegistryConfig",
        }),
    );

    // The registry's alone is not enough: Satin requires its limits too.
    let registry_only = missing.with_params(registry_config());
    assert_eq!(registry_only.require_params::<SequencerRegistryConfig>(), Ok(()));
    assert_eq!(
        registry_only.validate_schedule(),
        Err(ScheduleError::MissingParams { fork: MegaHardfork::Satin, params: "ProtocolLimits" }),
    );

    // With both attached the schedule loads, and a block runs on it.
    let loaded = registry_only.with_params(ProtocolLimits::DEFAULT);
    assert_eq!(loaded.require_params::<ProtocolLimits>(), Ok(()));
    assert_eq!(loaded.validate_schedule(), Ok(()));
    assert_eq!(loaded.fork_params::<SequencerRegistryConfig>(), Some(&registry_config()));
    assert_eq!(loaded.fork_params::<ProtocolLimits>(), Some(&ProtocolLimits::DEFAULT));

    let mut state = common::state();
    let mut executor = common::executor_with_spec(&mut state, common::unlimited_ctx(), loaded);
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&user_tx(0, empty_call_gas())).expect("the transaction executes");
}

/// A fork the schedule does not activate needs no parameters.
#[test]
fn test_an_unscheduled_fork_needs_no_params() {
    let unscheduled = MegaHardforkConfig::default();

    assert_eq!(unscheduled.require_params::<SequencerRegistryConfig>(), Ok(()));
    assert_eq!(unscheduled.require_params::<ProtocolLimits>(), Ok(()));
    assert_eq!(unscheduled.validate_schedule(), Ok(()));
}

/// A node's own schedule type: it attaches its parameters without `with_params`, and answers with
/// whatever it holds, checked or not. This one holds `limits` for Satin and answers everything
/// else as [`common::chain_spec`] does.
struct NodeSchedule {
    inner: MegaHardforkConfig,
    limits: ProtocolLimits,
}

impl EthereumHardforks for NodeSchedule {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.inner.ethereum_fork_activation(fork)
    }
}

impl OpHardforks for NodeSchedule {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        self.inner.op_fork_activation(fork)
    }
}

impl MegaHardforks for NodeSchedule {
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition {
        self.inner.mega_fork_activation(fork)
    }

    fn fork_params_any(
        &self,
        fork: MegaHardfork,
        params: TypeId,
    ) -> Option<&(dyn Any + Send + Sync)> {
        if fork == MegaHardfork::Satin && params == TypeId::of::<ProtocolLimits>() {
            Some(&self.limits)
        } else {
            self.inner.fork_params_any(fork, params)
        }
    }
}

/// The refusal of this engine an internal error carries.
fn internal(error: &BlockExecutionError) -> &MegaBlockExecutionError {
    error
        .as_internal()
        .and_then(|error| error.downcast_other::<MegaBlockExecutionError>())
        .unwrap_or_else(|| panic!("not an internal refusal of this engine: {error:?}"))
}

/// Limits their own check refuses run no block, even when they reach the block executor without
/// the load-time check: a node's own schedule type hands them over as it holds them. The executor
/// installs nothing on the EVM and refuses the block at every entry point — its start, a
/// transaction, its end — as an internal error: the schedule is the node's configuration, not the
/// block.
#[test]
fn test_limits_the_check_refuses_run_no_block() {
    let invalid = ProtocolLimits::no_limits();
    let schedule = NodeSchedule { inner: common::chain_spec(), limits: invalid };
    let message = invalid.validate().expect_err("no chain may carry them").message;
    assert_eq!(
        schedule.validate_schedule(),
        Err(ScheduleError::InvalidParams {
            fork: MegaHardfork::Satin,
            params: "ProtocolLimits",
            message: message.clone(),
        }),
        "the load-time check refuses them, for a node that runs it"
    );

    let mut state = common::state();
    let evm = MegaEvmFactory::new().create_evm(&mut state, common::evm_env());
    let built_with = *evm.tx_runtime_limits();
    let mut executor = MegaBlockExecutor::new(
        evm,
        common::unlimited_ctx(),
        &schedule,
        OpAlloyReceiptBuilder::default(),
    );
    assert_eq!(executor.protocol_limits(), None);
    assert_eq!(*executor.evm().tx_runtime_limits(), built_with, "nothing is installed");

    let refusal = MegaBlockExecutionError::InvalidProtocolLimits {
        timestamp: common::BLOCK_TIMESTAMP,
        message,
    };
    let error = executor.apply_pre_execution_changes().expect_err("the block does not start");
    assert_eq!(internal(&error), &refusal);
    assert!(executor.evm().db().bundle_state.state.is_empty(), "no pre-block step ran");
    let error =
        executor.execute_transaction(&user_tx(0, 100_000)).expect_err("no transaction runs either");
    assert_eq!(internal(&error), &refusal);
    assert!(executor.receipts().is_empty());
    let Err(error) = executor.finish() else { panic!("nor is the block finished") };
    assert_eq!(internal(&error), &refusal);

    // The same schedule holding limits a chain may carry runs the block.
    let schedule = NodeSchedule { inner: common::chain_spec(), limits: ProtocolLimits::DEFAULT };
    assert_eq!(schedule.validate_schedule(), Ok(()));
    let mut state = common::state();
    let evm = MegaEvmFactory::new().create_evm(&mut state, common::evm_env());
    let mut executor = MegaBlockExecutor::new(
        evm,
        common::unlimited_ctx(),
        &schedule,
        OpAlloyReceiptBuilder::default(),
    );
    assert_eq!(executor.protocol_limits(), Some(&ProtocolLimits::DEFAULT));
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&user_tx(0, 100_000)).expect("the transaction executes");
}
