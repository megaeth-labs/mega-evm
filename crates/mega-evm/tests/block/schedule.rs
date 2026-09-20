//! The schedule a block runs on, and what is checked of it before the block starts.

use alloy_evm::block::BlockExecutor;
use alloy_hardforks::ForkCondition;
use alloy_primitives::{address, Address};
use mega_evm::{
    chain_activation, mainnet_hardforks, testnet_hardforks, HardforkParams, HardforkParamsError,
    MegaHardfork, MegaHardforkConfig, MegaHardforks, ScheduleError, MAINNET_CHAIN_ID,
    TESTNET_CHAIN_ID,
};

use crate::common::{self, user_tx};

/// Parameters a fork requires from the chain configuration. Satin requires none yet, so this
/// stands in for the ones the mechanisms that bring the system contracts will register.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryParams {
    sequencer: Address,
}

impl HardforkParams for RegistryParams {
    const FORK: MegaHardfork = MegaHardfork::Satin;
    const NAME: &'static str = "RegistryParams";

    fn validate(&self) -> Result<(), HardforkParamsError> {
        if self.sequencer.is_zero() {
            return Err(HardforkParamsError { message: "sequencer must be set".into() });
        }
        Ok(())
    }
}

fn params() -> RegistryParams {
    RegistryParams { sequencer: address!("0x4444444444444444444444444444444444444444") }
}

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
        missing.require_params::<RegistryParams>(),
        Err(ScheduleError::MissingParams { fork: MegaHardfork::Satin, params: "RegistryParams" }),
        "the load refuses it"
    );

    // With the parameters attached the schedule loads, and a block runs on it.
    let loaded = missing.with_params(params());
    assert_eq!(loaded.require_params::<RegistryParams>(), Ok(()));
    assert_eq!(loaded.validate_schedule(), Ok(()));
    assert_eq!(loaded.fork_params::<RegistryParams>(), Some(&params()));

    let mut state = common::state();
    let mut executor = common::executor_with_spec(&mut state, common::unlimited_ctx(), loaded);
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&user_tx(0, 100_000)).expect("the transaction executes");
}

/// A fork the schedule does not activate needs no parameters.
#[test]
fn test_an_unscheduled_fork_needs_no_params() {
    let unscheduled = MegaHardforkConfig::default();

    assert_eq!(unscheduled.require_params::<RegistryParams>(), Ok(()));
    assert_eq!(unscheduled.validate_schedule(), Ok(()));
}
