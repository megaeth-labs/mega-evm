//! The schedule a block runs on, and what is checked of it before the block starts.

use alloy_evm::block::BlockExecutor;
use alloy_hardforks::ForkCondition;
use mega_evm::{
    chain_activation, mainnet_hardforks, system::SequencerRegistryConfig, testnet_hardforks,
    MegaHardfork, MegaHardforkConfig, MegaHardforks, ProtocolLimits, ScheduleError,
    MAINNET_CHAIN_ID, TESTNET_CHAIN_ID,
};

use crate::common::{self, registry_config, user_tx};

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
    executor.execute_transaction(&user_tx(0, 100_000)).expect("the transaction executes");
}

/// A fork the schedule does not activate needs no parameters.
#[test]
fn test_an_unscheduled_fork_needs_no_params() {
    let unscheduled = MegaHardforkConfig::default();

    assert_eq!(unscheduled.require_params::<SequencerRegistryConfig>(), Ok(()));
    assert_eq!(unscheduled.require_params::<ProtocolLimits>(), Ok(()));
    assert_eq!(unscheduled.validate_schedule(), Ok(()));
}
