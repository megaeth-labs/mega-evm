//! `MegaETH` hardforks of the Satin engine, and the schedule a chain activates them on.
//!
//! [`MegaHardfork`] is the fork; [`MegaHardforkConfig`] is a schedule that mixes it with the
//! Ethereum and Optimism forks; [`MegaHardforks`] is what block execution reads a schedule
//! through. A schedule is checked once, when it is loaded
//! ([`validate_schedule`](MegaHardforks::validate_schedule)), so a malformed chain configuration
//! fails at load rather than at the fork's first block.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{any::Any, fmt};
use std::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};

use alloy_hardforks::{hardfork, EthereumHardfork, EthereumHardforks, ForkCondition, Hardfork};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use alloy_primitives::{BlockTimestamp, U256};
use auto_impl::auto_impl;

use crate::MegaSpecId;

hardfork! {
    /// `MegaETH` network upgrades that schedule a Satin-engine spec. It is expected to mix with
    /// `EthereumHardfork` and `OpHardfork`.
    ///
    /// The forks of the legacy engine (`MiniRex` through `Rex7`) are not variants here; a chain
    /// runs them on the legacy engine up to the Satin activation.
    #[derive(serde::Serialize, serde::Deserialize)]
    MegaHardfork {
        /// Activates [`MegaSpecId::SATIN`].
        Satin,
    }
}

impl MegaHardfork {
    /// The spec this hardfork activates.
    pub const fn spec_id(&self) -> MegaSpecId {
        match self {
            Self::Satin => MegaSpecId::SATIN,
        }
    }
}

/// An invalid value of a [`HardforkParams`] type, as its
/// [`validate`](HardforkParams::validate) reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HardforkParamsError {
    /// Which field or invariant is wrong.
    pub message: String,
}

impl fmt::Display for HardforkParamsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl core::error::Error for HardforkParamsError {}

/// Parameters a fork needs from the chain configuration.
///
/// A params type belongs to exactly one fork ([`FORK`](Self::FORK)), so
/// [`fork_params`](MegaHardforks::fork_params) returns it without the caller naming the fork
/// twice. [`validate`](Self::validate) states the invariants of the value itself;
/// [`MegaHardforkConfig::with_params`] runs it when the configuration is built, so a bad value
/// fails at load. A fork that cannot run without its parameters also registers the requirement in
/// [`validate_schedule`](MegaHardforks::validate_schedule).
pub trait HardforkParams: Any + fmt::Debug + Send + Sync {
    /// The hardfork these parameters belong to.
    const FORK: MegaHardfork;

    /// How [`ScheduleError::MissingParams`] names this type.
    const NAME: &'static str;

    /// Checks the invariants of this value. The default accepts anything.
    fn validate(&self) -> Result<(), HardforkParamsError> {
        Ok(())
    }
}

/// Extends [`OpHardforks`] with the `MegaETH` forks.
///
/// Satin is the only `MegaETH` fork, so a schedule answers two questions: whether the chain has
/// reached it at a timestamp, and which spec it runs there. Before the Satin activation the chain
/// does not run this engine at all, which is why
/// [`spec_id`](Self::spec_id) answers `None` rather than an earlier spec.
#[auto_impl(&, Box, Arc)]
pub trait MegaHardforks: OpHardforks {
    /// The activation condition of `fork`, or [`ForkCondition::Never`] if the schedule does not
    /// carry it.
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition;

    /// The parameters attached to `fork`, type-erased. Forks without parameters answer `None`.
    fn fork_params_any(&self, _fork: MegaHardfork) -> Option<&(dyn Any + Send + Sync)> {
        None
    }

    /// The parameters of the fork `P` belongs to, or `None` when the schedule carries none.
    fn fork_params<P: HardforkParams>(&self) -> Option<&P> {
        self.fork_params_any(P::FORK)?.downcast_ref::<P>()
    }

    /// The latest `MegaETH` fork active at `timestamp`, or `None` before the first one.
    ///
    /// Resolution reads raw activation events and walks [`MegaHardfork::VARIANTS`] from the top,
    /// so a fork declared later wins at an equal timestamp.
    fn hardfork(&self, timestamp: BlockTimestamp) -> Option<MegaHardfork> {
        MegaHardfork::VARIANTS
            .iter()
            .rev()
            .find(|fork| self.mega_fork_activation(**fork).active_at_timestamp(timestamp))
            .copied()
    }

    /// The spec this engine executes at `timestamp`, or `None` when no `MegaETH` fork is active
    /// there — the chain runs its pre-Satin engine, which this crate does not execute.
    fn spec_id(&self, timestamp: BlockTimestamp) -> Option<MegaSpecId> {
        self.hardfork(timestamp).map(|fork| fork.spec_id())
    }

    /// Whether [`MegaHardfork::Satin`] has activated at `timestamp`.
    fn is_satin_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.mega_fork_activation(MegaHardfork::Satin).active_at_timestamp(timestamp)
    }

    /// Whether the block at `block_timestamp`, whose parent is at `parent_timestamp`, admits
    /// only deposit transactions.
    ///
    /// A block in which a fork activates carries the chain's own transactions only. This is true
    /// of a `MegaETH` fork and of an Optimism fork at or after Jovian, which
    /// [`OpHardforks::is_no_user_tx_activation_block`] answers for; block execution reads one
    /// flag, so the two are asked together here.
    fn admits_only_deposits(&self, parent_timestamp: u64, block_timestamp: u64) -> bool {
        MegaHardfork::VARIANTS.iter().any(|fork| {
            let activation = self.mega_fork_activation(*fork);
            activation.active_at_timestamp(block_timestamp) &&
                !activation.active_at_timestamp(parent_timestamp)
        }) || self.is_no_user_tx_activation_block(parent_timestamp, block_timestamp)
    }

    /// Refuses a schedule that activates `P::FORK` without attaching `P`.
    ///
    /// This is the rule [`validate_schedule`](Self::validate_schedule) applies to every params
    /// type a fork requires; it is public so a params type can be checked on its own.
    fn require_params<P: HardforkParams>(&self) -> Result<(), ScheduleError> {
        if self.mega_fork_activation(P::FORK) != ForkCondition::Never &&
            self.fork_params::<P>().is_none()
        {
            return Err(ScheduleError::MissingParams { fork: P::FORK, params: P::NAME });
        }
        Ok(())
    }

    /// Checks that this schedule is one a chain can run, so a configuration mistake is caught
    /// when the chain configuration is loaded rather than at the fork's first block.
    ///
    /// Checked:
    ///
    /// - Every registered `MegaETH` fork activates by [`ForkCondition::Timestamp`] (or is
    ///   [`ForkCondition::Never`]). Resolution is timestamp-scoped, so a block-number or
    ///   total-difficulty condition would silently never activate the fork.
    /// - The parameters every scheduled fork requires are attached
    ///   ([`require_params`](Self::require_params)). Satin requires none yet: a mechanism that
    ///   brings a [`HardforkParams`] type registers it here, in the block marked below, and the
    ///   requirement is enforced from that change on.
    ///
    /// There is no ordering or gap check: a single fork has nothing to be out of order with. The
    /// spec that follows Satin brings the ladder back, and with it those checks.
    fn validate_schedule(&self) -> Result<(), ScheduleError> {
        for fork in MegaHardfork::VARIANTS {
            match self.mega_fork_activation(*fork) {
                ForkCondition::Never | ForkCondition::Timestamp(_) => {}
                _ => return Err(ScheduleError::NonTimestampActivation { fork: *fork }),
            }
        }

        // Required parameters, one `self.require_params::<P>()?` per params type. Satin has none.

        Ok(())
    }
}

/// A schedule a chain cannot run, as [`MegaHardforks::validate_schedule`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    /// A `MegaETH` fork is registered with a block-number or total-difficulty condition, which
    /// the timestamp-scoped resolution never reports as active.
    NonTimestampActivation {
        /// The fork with the wrong condition.
        fork: MegaHardfork,
    },
    /// A scheduled fork requires parameters that the schedule does not carry.
    MissingParams {
        /// The scheduled fork.
        fork: MegaHardfork,
        /// The required params type, as [`HardforkParams::NAME`] names it.
        params: &'static str,
    },
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonTimestampActivation { fork } => {
                write!(f, "hardfork {fork:?} must activate by timestamp, not block number or TTD")
            }
            Self::MissingParams { fork, params } => {
                write!(
                    f,
                    "hardfork {fork:?} is scheduled but its {params} params are not configured"
                )
            }
        }
    }
}

impl core::error::Error for ScheduleError {}

/// One fork of a schedule: which fork, when it activates, and the parameters it carries.
#[derive(Debug)]
struct ForkEntry {
    fork: Box<dyn Hardfork>,
    condition: ForkCondition,
    params: Option<Arc<dyn Any + Send + Sync>>,
}

impl Clone for ForkEntry {
    fn clone(&self) -> Self {
        Self { fork: self.fork.clone(), condition: self.condition, params: self.params.clone() }
    }
}

/// A chain's hardfork schedule.
///
/// [`new`](Self::new) activates every Ethereum and Optimism fork up to and including the base
/// Satin runs on (Osaka / Karst) at genesis, and no `MegaETH` fork:  those the caller schedules,
/// with [`with`](Self::with) or [`with_all_activated`](Self::with_all_activated).
#[derive(Debug, Clone)]
pub struct MegaHardforkConfig {
    entries: Vec<ForkEntry>,
}

impl Default for MegaHardforkConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, H> From<I> for MegaHardforkConfig
where
    I: Iterator<Item = (H, ForkCondition)>,
    H: Hardfork + 'static,
{
    fn from(iter: I) -> Self {
        Self {
            entries: iter
                .map(|(fork, condition)| ForkEntry {
                    fork: Box::new(fork) as Box<dyn Hardfork>,
                    condition,
                    params: None,
                })
                .collect(),
        }
    }
}

impl MegaHardforkConfig {
    /// A schedule with every Ethereum and Optimism fork up to Osaka / Karst active at genesis,
    /// and no `MegaETH` fork.
    ///
    /// Karst is the base [`MegaSpecId::SATIN`] runs on, so a chain executing Satin has already
    /// passed every fork below it; the schedule states that rather than leaving the node to
    /// repeat it.
    pub fn new() -> Self {
        let forks: Vec<(Box<dyn Hardfork>, ForkCondition)> = vec![
            (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Dao.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
            (
                EthereumHardfork::Paris.boxed(),
                ForkCondition::TTD {
                    activation_block_number: 0,
                    fork_block: None,
                    total_difficulty: U256::ZERO,
                },
            ),
            (OpHardfork::Bedrock.boxed(), ForkCondition::Block(0)),
            (OpHardfork::Regolith.boxed(), ForkCondition::Timestamp(0)),
            (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Canyon.boxed(), ForkCondition::Timestamp(0)),
            (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Ecotone.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Fjord.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Granite.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Holocene.boxed(), ForkCondition::Timestamp(0)),
            (EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Isthmus.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Jovian.boxed(), ForkCondition::Timestamp(0)),
            (EthereumHardfork::Osaka.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Karst.boxed(), ForkCondition::Timestamp(0)),
        ];
        Self {
            entries: forks
                .into_iter()
                .map(|(fork, condition)| ForkEntry { fork, condition, params: None })
                .collect(),
        }
    }

    /// Activates every `MegaETH` fork at timestamp 0.
    pub fn with_all_activated(mut self) -> Self {
        for fork in MegaHardfork::VARIANTS {
            self.insert(*fork, ForkCondition::Timestamp(0));
        }
        self
    }

    /// Attaches `params` to the fork they belong to.
    ///
    /// # Panics
    ///
    /// If the fork is not registered in this schedule, or if `params.validate()` refuses the
    /// value — a chain configuration is built once, at load, and a bad one must not start.
    pub fn with_params<P: HardforkParams>(mut self, params: P) -> Self {
        if let Err(e) = params.validate() {
            panic!("Invalid params for fork {:?}: {}", P::FORK, e.message);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.fork.name() == P::FORK.name())
            .unwrap_or_else(|| {
                panic!(
                    "Cannot attach params to fork {:?}: fork not registered in config. \
                     Call .with({:?}, condition) first.",
                    P::FORK,
                    P::FORK,
                )
            });
        entry.params = Some(Arc::new(params));
        self
    }

    /// Removes a `MegaETH` fork, which is what [`ForkCondition::Never`] means.
    pub fn without(mut self, hardfork: MegaHardfork) -> Self {
        self.entries.retain(|entry| entry.fork.name() != hardfork.name());
        self
    }

    /// Adds `hardfork` with `condition`, or replaces its condition if it is already there.
    pub fn with(mut self, hardfork: impl Hardfork, condition: ForkCondition) -> Self {
        self.insert(hardfork, condition);
        self
    }

    /// Adds `hardfork` with `condition`. An entry that is already there keeps its parameters and
    /// takes the new condition.
    pub fn insert(&mut self, hardfork: impl Hardfork, condition: ForkCondition) {
        let index = self.entries.iter().position(|entry| entry.fork.name() == hardfork.name());
        if let Some(index) = index {
            self.entries[index].condition = condition;
        } else {
            self.entries.push(ForkEntry { fork: Box::new(hardfork), condition, params: None });
        }
    }

    /// The condition `hardfork` activates on, or `None` if the schedule does not carry it.
    pub fn get(&self, hardfork: impl Hardfork) -> Option<&ForkCondition> {
        self.entries
            .iter()
            .find(|entry| entry.fork.name() == hardfork.name())
            .map(|entry| &entry.condition)
    }
}

impl EthereumHardforks for MegaHardforkConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.get(fork).copied().unwrap_or(ForkCondition::Never)
    }
}

impl OpHardforks for MegaHardforkConfig {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        self.get(fork).copied().unwrap_or(ForkCondition::Never)
    }
}

impl MegaHardforks for MegaHardforkConfig {
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition {
        self.get(fork).copied().unwrap_or(ForkCondition::Never)
    }

    fn fork_params_any(&self, fork: MegaHardfork) -> Option<&(dyn Any + Send + Sync)> {
        self.entries
            .iter()
            .find(|entry| entry.fork.name() == fork.name())
            .and_then(|entry| entry.params.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use core::str::FromStr;

    /// A params type of this test module alone: the fork-requires-params rule is a mechanism
    /// here, and Satin has no params type of its own yet.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestParams {
        sequencer: alloy_primitives::Address,
    }

    impl HardforkParams for TestParams {
        const FORK: MegaHardfork = MegaHardfork::Satin;
        const NAME: &'static str = "TestParams";

        fn validate(&self) -> Result<(), HardforkParamsError> {
            if self.sequencer.is_zero() {
                return Err(HardforkParamsError { message: "sequencer must be set".into() });
            }
            Ok(())
        }
    }

    fn params() -> TestParams {
        TestParams { sequencer: address!("0x2222222222222222222222222222222222222222") }
    }

    #[test]
    fn test_satin_fork_schedules_the_satin_spec() {
        assert_eq!(MegaHardfork::VARIANTS, &[MegaHardfork::Satin]);
        assert_eq!(MegaHardfork::Satin.spec_id(), MegaSpecId::SATIN);
        assert_eq!(Hardfork::name(&MegaHardfork::Satin), "Satin");
        assert_eq!(MegaHardfork::Satin.to_string(), "Satin");
    }

    #[test]
    fn test_legacy_fork_names_do_not_parse() {
        assert_eq!(MegaHardfork::from_str("Satin").unwrap(), MegaHardfork::Satin);
        for legacy in ["MiniRex", "MiniRex1", "MiniRex2", "Rex", "Rex1", "Rex6", "Rex7"] {
            assert!(MegaHardfork::from_str(legacy).is_err(), "{legacy} must not parse");
        }
    }

    #[test]
    fn test_default_config_contains_upstream_forks_and_no_mega_forks() {
        let config = MegaHardforkConfig::default();

        assert_eq!(
            config.ethereum_fork_activation(EthereumHardfork::Frontier),
            ForkCondition::Block(0)
        );
        assert_eq!(
            config.ethereum_fork_activation(EthereumHardfork::Prague),
            ForkCondition::Timestamp(0)
        );
        assert_eq!(
            config.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(0)
        );
        assert_eq!(config.op_fork_activation(OpHardfork::Isthmus), ForkCondition::Timestamp(0));
        assert_eq!(config.op_fork_activation(OpHardfork::Karst), ForkCondition::Timestamp(0));
        assert_eq!(config.mega_fork_activation(MegaHardfork::Satin), ForkCondition::Never);
        assert!(config.fork_params::<TestParams>().is_none());
    }

    #[test]
    fn test_config_builder_helpers_override_and_remove_hardforks() {
        let mut config =
            MegaHardforkConfig::new().with(MegaHardfork::Satin, ForkCondition::Timestamp(10));

        assert_eq!(config.get(MegaHardfork::Satin), Some(&ForkCondition::Timestamp(10)));

        config.insert(MegaHardfork::Satin, ForkCondition::Timestamp(20));
        assert_eq!(config.get(MegaHardfork::Satin), Some(&ForkCondition::Timestamp(20)));

        // Removing one fork leaves the rest of the schedule as it was.
        config.insert(EthereumHardfork::Prague, ForkCondition::Timestamp(30));
        let config = config.without(MegaHardfork::Satin);
        assert_eq!(config.get(MegaHardfork::Satin), None);
        assert_eq!(
            config.get(EthereumHardfork::Prague),
            Some(&ForkCondition::Timestamp(30)),
            "the other entries are untouched"
        );

        let from_iter = MegaHardforkConfig::from(
            [(MegaHardfork::Satin, ForkCondition::Timestamp(1))].into_iter(),
        );
        assert_eq!(from_iter.get(MegaHardfork::Satin), Some(&ForkCondition::Timestamp(1)));
    }

    #[test]
    fn test_with_all_activated_enables_all_mega_hardforks() {
        let config = MegaHardforkConfig::default().with_all_activated();

        // Driven off `VARIANTS`, which the `hardfork!` macro generates from the same variant list
        // that declares the enum. A second hand-written list here would assert only that the
        // forks someone remembered to name are activated.
        for hardfork in MegaHardfork::VARIANTS {
            assert_eq!(
                config.mega_fork_activation(*hardfork),
                ForkCondition::Timestamp(0),
                "{hardfork:?}"
            );
        }
    }

    #[test]
    fn test_fork_params_typed_access() {
        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::Satin, ForkCondition::Timestamp(0))
            .with_params(params());

        let retrieved = config.fork_params::<TestParams>().expect("params were attached");
        assert_eq!(retrieved, &params());
    }

    #[test]
    fn test_fork_params_none_when_not_configured() {
        let config =
            MegaHardforkConfig::default().with(MegaHardfork::Satin, ForkCondition::Timestamp(0));

        assert!(config.fork_params::<TestParams>().is_none());
    }

    #[test]
    fn test_default_validate_accepts_any_value() {
        #[derive(Debug)]
        struct NullParams;

        impl HardforkParams for NullParams {
            const FORK: MegaHardfork = MegaHardfork::Satin;
            const NAME: &'static str = "NullParams";
        }

        assert!(NullParams.validate().is_ok());
    }

    #[test]
    fn test_hardfork_params_error_display() {
        let e = HardforkParamsError { message: "something went wrong".into() };
        assert_eq!(e.to_string(), "something went wrong");
    }

    #[test]
    #[should_panic(expected = "Invalid params for fork")]
    fn test_with_params_panics_on_validation_error() {
        MegaHardforkConfig::default()
            .with_all_activated()
            .with_params(TestParams { sequencer: alloy_primitives::Address::ZERO });
    }

    /// Documented domain limit: resolution is timestamp-scoped, so a `MegaHardfork` registered by
    /// block number never contributes its spec. `validate_schedule` rejects such a schedule at
    /// load rather than leaving the fork silently inactive.
    #[test]
    fn test_resolution_ignores_block_numbered_forks() {
        let hf = MegaHardforkConfig::new().with(MegaHardfork::Satin, ForkCondition::Block(0));

        assert!(
            !hf.mega_fork_activation(MegaHardfork::Satin).active_at_timestamp(0),
            "block-numbered forks are not timestamped"
        );
        assert_eq!(hf.spec_id(0), None);
        assert!(!hf.is_satin_active_at_timestamp(0));
        assert_eq!(
            hf.validate_schedule(),
            Err(ScheduleError::NonTimestampActivation { fork: MegaHardfork::Satin })
        );
    }

    /// A scheduled fork whose required params are missing fails at validation time instead of at
    /// the first block of the fork.
    #[test]
    fn test_validate_schedule_requires_scheduled_fork_params() {
        let unscheduled = MegaHardforkConfig::default();
        assert_eq!(unscheduled.require_params::<TestParams>(), Ok(()));

        let no_params = MegaHardforkConfig::default().with_all_activated();
        assert_eq!(
            no_params.require_params::<TestParams>(),
            Err(ScheduleError::MissingParams { fork: MegaHardfork::Satin, params: "TestParams" })
        );
        assert_eq!(
            no_params.require_params::<TestParams>().unwrap_err().to_string(),
            "hardfork Satin is scheduled but its TestParams params are not configured"
        );

        let with_params = no_params.with_params(params());
        assert_eq!(with_params.require_params::<TestParams>(), Ok(()));
        assert_eq!(with_params.validate_schedule(), Ok(()));
    }

    /// The block a fork activates in carries the chain's own transactions only, and only that
    /// block: the one before it and the ones after it are ordinary.
    #[test]
    fn test_activation_block_admits_only_deposits() {
        let config =
            MegaHardforkConfig::default().with(MegaHardfork::Satin, ForkCondition::Timestamp(100));

        assert!(config.admits_only_deposits(99, 100), "the first block at the activation");
        assert!(config.admits_only_deposits(99, 101), "and the first block after a gap");
        assert!(!config.admits_only_deposits(98, 99), "not before the activation");
        assert!(!config.admits_only_deposits(100, 101), "and not once the parent had it too");
    }

    #[test]
    fn test_hardfork_and_spec_id_follow_latest_active_timestamp() {
        let config =
            MegaHardforkConfig::default().with(MegaHardfork::Satin, ForkCondition::Timestamp(100));

        assert_eq!(config.hardfork(99), None);
        assert_eq!(config.spec_id(99), None);
        assert!(!config.is_satin_active_at_timestamp(99));
        assert_eq!(config.hardfork(100), Some(MegaHardfork::Satin));
        assert_eq!(config.spec_id(100), Some(MegaSpecId::SATIN));
        assert!(config.is_satin_active_at_timestamp(100));
        assert_eq!(config.spec_id(u64::MAX), Some(MegaSpecId::SATIN));
    }
}
