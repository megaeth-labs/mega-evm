//! `MegaETH` hardforks of the Satin engine, and the schedule a chain activates them on.
//!
//! [`MegaHardfork`] is the fork; [`MegaHardforkConfig`] is a schedule that mixes it with the
//! Ethereum and Optimism forks; [`MegaHardforks`] is what block execution reads a schedule
//! through. A schedule is checked once, when it is loaded
//! ([`validate_schedule`](MegaHardforks::validate_schedule)), so a malformed chain configuration
//! fails at load rather than at the fork's first block.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::fmt;
use std::{boxed::Box, vec, vec::Vec};

use alloy_hardforks::{hardfork, EthereumHardfork, EthereumHardforks, ForkCondition, Hardfork};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use alloy_primitives::{BlockTimestamp, U256};
use auto_impl::auto_impl;

use crate::MegaSpecId;

/// The Ethereum forks Satin runs on that a schedule activates by timestamp: every one from the
/// first after the Merge up to Osaka.
const SATIN_BASE_ETHEREUM_FORKS: [EthereumHardfork; 4] = [
    EthereumHardfork::Shanghai,
    EthereumHardfork::Cancun,
    EthereumHardfork::Prague,
    EthereumHardfork::Osaka,
];

/// The Optimism forks Satin runs on that a schedule activates by timestamp: every one from the
/// first after Bedrock up to Karst.
const SATIN_BASE_OP_FORKS: [OpHardfork; 9] = [
    OpHardfork::Regolith,
    OpHardfork::Canyon,
    OpHardfork::Ecotone,
    OpHardfork::Fjord,
    OpHardfork::Granite,
    OpHardfork::Holocene,
    OpHardfork::Isthmus,
    OpHardfork::Jovian,
    OpHardfork::Karst,
];

hardfork! {
    /// `MegaETH` network upgrades that schedule a Satin-engine spec. It is expected to mix with
    /// `EthereumHardfork` and `OpHardfork`.
    ///
    /// The forks of the legacy engine (`MiniRex` through `Rex6`) are not variants here; a chain
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

    /// Checks that this schedule is one a chain can run, so a configuration mistake is caught
    /// when the chain configuration is loaded rather than at the fork's first block.
    ///
    /// Checked:
    ///
    /// - Every registered `MegaETH` fork activates by [`ForkCondition::Timestamp`] (or is
    ///   [`ForkCondition::Never`]). Resolution is timestamp-scoped, so a block-number or
    ///   total-difficulty condition would silently never activate the fork.
    /// - Every fork Satin runs on that the schedule activates by timestamp — the Ethereum forks
    ///   from Shanghai to Osaka and the Optimism forks from Regolith to Karst — is active at
    ///   Satin's activation, so the engine and the rest of the node, which reads the same schedule,
    ///   do not apply different rules to one block. The other half of that timing rule, that
    ///   Jovian, Karst and Osaka do not activate before Satin, is the node's to check: before Satin
    ///   the chain runs its pre-Satin engine, which this crate does not execute.
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

        require_base_forks(self)
    }
}

/// Refuses a schedule that activates Satin at a timestamp at which a fork it runs on is not
/// active: the half of the timing rule
/// [`validate_schedule`](MegaHardforks::validate_schedule) states that the engine checks.
///
/// The forks checked are those a schedule activates by timestamp, the Ethereum forks from Shanghai
/// to Osaka and the Optimism forks from Regolith to Karst, each read as block execution reads
/// it: active at Satin's activation timestamp. A fork scheduled by block number or total
/// difficulty instead, or not at all, is not active at any timestamp, and is refused. The forks
/// up to the Merge, and Bedrock, are scheduled by block number or total difficulty, which a
/// timestamp cannot be compared with, and every one of them precedes the first fork scheduled
/// by timestamp; they are not checked. A schedule that does not activate Satin is not checked
/// either.
fn require_base_forks<H: MegaHardforks + ?Sized>(schedule: &H) -> Result<(), ScheduleError> {
    let ForkCondition::Timestamp(satin_time) = schedule.mega_fork_activation(MegaHardfork::Satin)
    else {
        return Ok(());
    };
    let ethereum = SATIN_BASE_ETHEREUM_FORKS
        .iter()
        .map(|fork| (fork.name(), schedule.ethereum_fork_activation(*fork)));
    let op =
        SATIN_BASE_OP_FORKS.iter().map(|fork| (fork.name(), schedule.op_fork_activation(*fork)));
    for (base, condition) in ethereum.chain(op) {
        if !condition.active_at_timestamp(satin_time) {
            return Err(ScheduleError::InactiveBaseFork {
                fork: MegaHardfork::Satin,
                activation: satin_time,
                base,
            });
        }
    }
    Ok(())
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
    /// A fork a scheduled `MegaETH` fork runs on is not active when that fork activates.
    InactiveBaseFork {
        /// The scheduled fork.
        fork: MegaHardfork,
        /// The timestamp it activates at.
        activation: BlockTimestamp,
        /// The fork it runs on that is not active then, as [`Hardfork::name`] names it.
        base: &'static str,
    },
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonTimestampActivation { fork } => {
                write!(f, "hardfork {fork:?} must activate by timestamp, not block number or TTD")
            }
            Self::InactiveBaseFork { fork, activation, base } => {
                write!(
                    f,
                    "hardfork {fork:?} activates at timestamp {activation}, where the {base} fork \
                     it runs on is not active"
                )
            }
        }
    }
}

impl core::error::Error for ScheduleError {}

/// One fork of a schedule: which fork, and when it activates.
#[derive(Debug)]
struct ForkEntry {
    fork: Box<dyn Hardfork>,
    condition: ForkCondition,
}

impl Clone for ForkEntry {
    fn clone(&self) -> Self {
        Self { fork: self.fork.clone(), condition: self.condition }
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
                .map(|(fork, condition)| ForkEntry { fork, condition })
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

    /// Adds `hardfork` with `condition`. An entry that is already there takes the new condition.
    pub fn insert(&mut self, hardfork: impl Hardfork, condition: ForkCondition) {
        let index = self.entries.iter().position(|entry| entry.fork.name() == hardfork.name());
        if let Some(index) = index {
            self.entries[index].condition = condition;
        } else {
            self.entries.push(ForkEntry { fork: Box::new(hardfork), condition });
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::str::FromStr;

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
        for legacy in ["MiniRex", "MiniRex1", "MiniRex2", "Rex", "Rex1", "Rex6"] {
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
