#[cfg(not(feature = "std"))]
use alloc as std;

use alloy_hardforks::{hardfork, EthereumHardfork, EthereumHardforks, ForkCondition, Hardfork};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use alloy_primitives::{BlockTimestamp, U256};
use auto_impl::auto_impl;
use core::any::Any;
use std::{boxed::Box, sync::Arc, vec::Vec};

use crate::{MegaSpecId, SequencerRegistryConfig, SequencerRegistryRex6Config};

hardfork! {
    /// The name of MegaETH hardforks. It is expected to mix with [`EthereumHardfork`] and
    /// [`OpHardfork`].
    #[derive(serde::Serialize, serde::Deserialize)]
    MegaHardfork {
        /// The first hardfork.
        MiniRex,
        /// The first alias hardfork: rolls behavior back to Equivalence on its own rung.
        MiniRex1,
        /// The second alias hardfork: restores MiniRex behavior on its own rung.
        MiniRex2,
        /// The fourth hardfork.
        Rex,
        /// The fifth hardfork (first patch to Rex).
        Rex1,
        /// The sixth hardfork (second patch to Rex).
        Rex2,
        /// The seventh hardfork (third patch to Rex).
        Rex3,
        /// The eighth hardfork (fourth patch to Rex).
        Rex4,
        /// The ninth hardfork (fifth patch to Rex).
        Rex5,
        /// The tenth hardfork (sixth patch to Rex).
        Rex6,
        /// The eleventh hardfork (seventh patch to Rex).
        Rex7,
    }
}

impl MegaHardfork {
    /// Whether this fork introduces new behavior — its spec is not an alias. Alias forks
    /// (`MiniRex1`, `MiniRex2`) schedule a rung whose behavior belongs to an earlier spec.
    pub(crate) fn introduces_behavior(self) -> bool {
        !self.spec_id().is_alias()
    }

    /// Gets the `MegaSpecId` associated with this hardfork.
    #[allow(clippy::match_same_arms)]
    pub const fn spec_id(&self) -> MegaSpecId {
        // Note: MiniRex1 and MiniRex2 map to alias specs: rungs of their own whose
        // behavior projects to previously released specs.
        match self {
            Self::MiniRex => MegaSpecId::MINI_REX,
            Self::MiniRex1 => MegaSpecId::MINI_REX_1,
            Self::MiniRex2 => MegaSpecId::MINI_REX_2,
            Self::Rex => MegaSpecId::REX,
            Self::Rex1 => MegaSpecId::REX1,
            Self::Rex2 => MegaSpecId::REX2,
            Self::Rex3 => MegaSpecId::REX3,
            Self::Rex4 => MegaSpecId::REX4,
            Self::Rex5 => MegaSpecId::REX5,
            Self::Rex6 => MegaSpecId::REX6,
            Self::Rex7 => MegaSpecId::REX7,
        }
    }
}

/// Whether each fork in `forks` maps to a strictly higher spec rung than the fork before it.
///
/// This is the invariant the fork→spec map rests on: the reverse scan in
/// [`MegaHardforks::hardfork`] and the skipped-rung rule in
/// [`MegaHardforks::validate_schedule`] assume declaration order climbs the ladder, and
/// strictness pins injectivity — no two forks share a rung, so the map stays 1:1 and the
/// resolved spec is monotone on any ordered schedule.
///
/// Shared by the compile-time assertion below and by the test that feeds it malformed lists,
/// so the checker itself is exercised — the real `VARIANTS` always satisfies the property it
/// checks.
const fn climbs_the_spec_ladder(forks: &[MegaHardfork]) -> bool {
    let mut i = 1;
    while i < forks.len() {
        if forks[i - 1].spec_id() as u8 >= forks[i].spec_id() as u8 {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = assert!(
    climbs_the_spec_ladder(MegaHardfork::VARIANTS),
    "every hardfork must map to a strictly higher spec rung than the fork declared before it — \
     express a rollback as a new alias rung, not by reusing an earlier spec"
);

/// Validation error returned by [`HardforkParams::validate`].
#[derive(Debug, Clone, PartialEq, Eq, derive_more::Display, derive_more::Error)]
#[display("{message}")]
pub struct HardforkParamsError {
    /// Human-readable description of the invalid field or invariant.
    pub message: std::string::String,
}

/// Marker trait for per-fork parameters.
///
/// Each params type is pinned to exactly one [`MegaHardfork`] variant via `FORK`.
/// This lets [`MegaHardforks::fork_params`] return a typed reference without
/// requiring the caller to specify the fork separately.
pub trait HardforkParams: Any + core::fmt::Debug + Send + Sync {
    /// The hardfork this params type belongs to.
    const FORK: MegaHardfork;

    /// Validates construction-time invariants (no cross-fork context required).
    ///
    /// Called by [`MegaHardforkConfig::with_params`] so misconfiguration is caught
    /// at chain-config load time rather than at the first block where the fork activates.
    /// The default implementation accepts any value.
    fn validate(&self) -> Result<(), HardforkParamsError> {
        Ok(())
    }
}

/// Extends [`OpHardforks`] with `MegaETH` helper methods.
///
/// Forks map 1:1 onto specs, so [`spec_id`](Self::spec_id) — the latest activated fork's spec —
/// is monotone: it only ever climbs. Rollbacks are expressed by *alias specs* (`MINI_REX_1`,
/// `MINI_REX_2`): rungs of their own whose behavior projects to an earlier spec. One resolved
/// value is therefore read through two projections:
///
/// - `spec.is_enabled(X)` — the BEHAVIOR gate: both sides project through [`MegaSpecId::behavior`],
///   so execution semantics roll back inside an alias window.
/// - `spec.reaches(X)` — the POSITION gate: raw rung comparison for one-way chain setup
///   (predeploys, bytecode versions, pre-block rules), which a rollback does not retract.
///
/// The `is_<fork>_active_at_timestamp` predicates are position projections for
/// behavior-introducing forks (additive on schedules that omit a predecessor) and raw event
/// queries for alias forks, whose occurrence is not recoverable from the ladder. To ask about
/// the raw scheduling event of any fork, use `mega_fork_activation` directly. To check a
/// schedule for well-formedness (rung gaps, ordering, required params), use
/// [`validate_schedule`](Self::validate_schedule).
#[auto_impl(&, Box, Arc)]
pub trait MegaHardforks: OpHardforks {
    /// Retrieves [`ForkCondition`] by a [`MegaHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition;

    /// Returns a type-erased reference to per-fork parameters, if configured.
    ///
    /// Most forks carry no parameters and the default implementation returns `None`.
    fn fork_params_any(&self, _fork: MegaHardfork) -> Option<&(dyn Any + Send + Sync)> {
        None
    }

    /// Returns a typed reference to per-fork parameters.
    ///
    /// `P::FORK` identifies the fork. Returns `None` if the fork has no params configured.
    fn fork_params<P: HardforkParams>(&self) -> Option<&P> {
        self.fork_params_any(P::FORK)?.downcast_ref::<P>()
    }

    /// Returns the current `MegaHardfork` active at the given timestamp.
    ///
    /// Resolution walks the declaration ladder from the top: the latest-declared fork whose
    /// activation event has occurred wins. Declaration order is chronological, so at equal
    /// activation timestamps the later-declared fork prevails. Driven off
    /// [`MegaHardfork::VARIANTS`] so a newly declared fork joins resolution without a second
    /// hand-written ladder here.
    ///
    /// Resolution reads raw activation events
    /// ([`mega_fork_activation`](Self::mega_fork_activation)); with the 1:1 ascending fork->spec
    /// map, the resolved spec is monotone in time on any ordered schedule.
    fn hardfork(&self, timestamp: BlockTimestamp) -> Option<MegaHardfork> {
        MegaHardfork::VARIANTS
            .iter()
            .rev()
            .find(|fork| self.mega_fork_activation(**fork).active_at_timestamp(timestamp))
            .copied()
    }

    /// Returns the current `MegaSpecId` for the given block timestamp.
    fn spec_id(&self, timestamp: BlockTimestamp) -> MegaSpecId {
        self.hardfork(timestamp).map_or(MegaSpecId::EQUIVALENCE, |h| h.spec_id())
    }

    /// Returns the highest [`MegaSpecId`] among all [`MegaHardfork`]s activated at or before
    /// `timestamp` — which, under the 1:1 ascending fork->spec map, is exactly
    /// [`spec_id`](Self::spec_id).
    ///
    /// Kept as an alias so call sites can state "one-way setup" intent explicitly; pair it with
    /// [`MegaSpecId::reaches`] (position) rather than `is_enabled` (behavior).
    fn max_activated_spec_id(&self, timestamp: BlockTimestamp) -> MegaSpecId {
        // With the 1:1 ascending fork->spec map, the latest activated fork IS the maximum:
        // the floor coincides with `spec_id` on every schedule. Kept as an alias for the
        // transition; call sites can migrate to `spec_id` + `reaches`.
        self.spec_id(timestamp)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::MINI_REX`], the
    /// spec introduced by [`MegaHardfork::MiniRex`]. Position-projected (`reaches`) — see the trait
    /// docs; for the raw activation event use
    /// [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_mini_rex_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::MINI_REX)
    }

    /// Returns `true` if the [`MegaHardfork::MiniRex1`] activation event has occurred at the
    /// given block timestamp.
    ///
    /// `MiniRex1` schedules the alias rung [`MegaSpecId::MINI_REX_1`], whose occurrence is not
    /// recoverable from the ladder position (later rungs stand above it whether or not it was
    /// ever scheduled), so this predicate stays a raw event query, unlike the
    /// position-projected predicates of behavior-introducing forks.
    fn is_mini_rex_1_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.mega_fork_activation(MegaHardfork::MiniRex1).active_at_timestamp(timestamp)
    }

    /// Returns `true` if the [`MegaHardfork::MiniRex2`] activation event has occurred at the
    /// given block timestamp.
    ///
    /// `MiniRex2` schedules the alias rung [`MegaSpecId::MINI_REX_2`], whose occurrence is not
    /// recoverable from the ladder position, so this predicate stays a raw event query, unlike
    /// the position-projected predicates of behavior-introducing forks.
    fn is_mini_rex_2_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.mega_fork_activation(MegaHardfork::MiniRex2).active_at_timestamp(timestamp)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::REX`], the rung
    /// introduced by [`MegaHardfork::Rex`]. Floor-projected — see the trait docs; for the raw
    /// activation event use [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_rex_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::REX)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::REX1`], the rung
    /// introduced by [`MegaHardfork::Rex1`]. Floor-projected — see the trait docs; for the raw
    /// activation event use [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_rex_1_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::REX1)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::REX2`], the rung
    /// introduced by [`MegaHardfork::Rex2`]. Floor-projected — see the trait docs; for the raw
    /// activation event use [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_rex_2_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::REX2)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::REX3`], the rung
    /// introduced by [`MegaHardfork::Rex3`]. Floor-projected — see the trait docs; for the raw
    /// activation event use [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_rex_3_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::REX3)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::REX4`], the rung
    /// introduced by [`MegaHardfork::Rex4`]. Floor-projected — see the trait docs; for the raw
    /// activation event use [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_rex_4_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::REX4)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::REX5`], the rung
    /// introduced by [`MegaHardfork::Rex5`]. Floor-projected — see the trait docs; for the raw
    /// activation event use [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_rex_5_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::REX5)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::REX6`], the rung
    /// introduced by [`MegaHardfork::Rex6`]. Floor-projected — see the trait docs; for the raw
    /// activation event use [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_rex_6_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::REX6)
    }

    /// Returns `true` once the scheduled spec has reached [`MegaSpecId::REX7`], the rung
    /// introduced by [`MegaHardfork::Rex7`]. Floor-projected — see the trait docs; for the raw
    /// activation event use [`mega_fork_activation`](Self::mega_fork_activation).
    fn is_rex_7_active_at_timestamp(&self, timestamp: BlockTimestamp) -> bool {
        self.max_activated_spec_id(timestamp).reaches(MegaSpecId::REX7)
    }

    /// Checks the schedule for well-formedness, i.e., that it describes a chain climbing the
    /// spec ladder rung by rung.
    ///
    /// This is the fail-fast complement to the fail-safe execution layer: block execution
    /// tolerates a malformed schedule (setup gates compare rung positions, so a skipped rung's
    /// setup still runs), but a real chain's published schedule with a skipped rung is almost
    /// certainly a configuration mistake. Chain-config authors and node startup
    /// paths should call this and refuse a schedule that does not validate.
    ///
    /// Checked, in order:
    ///
    /// - Every registered `MegaHardfork` activates by [`ForkCondition::Timestamp`] (or is
    ///   [`ForkCondition::Never`]). Block-number and TTD conditions never report active to the
    ///   timestamp-scoped resolution here and would silently deactivate the fork.
    /// - Scheduled forks activate in declaration order: a later-declared fork must not activate
    ///   strictly before an earlier-declared one.
    /// - No skipped rungs: a behavior-introducing fork may be unscheduled only if no scheduled fork
    ///   maps to an equal-or-higher rung. Alias forks are exempt as the *missing* side — testnet
    ///   legitimately never schedules `MiniRex1`/`MiniRex2` — but an alias scheduled without its
    ///   base is itself a skipped rung (the alias's rung sits above the base's).
    /// - Per-fork parameters required by a scheduled fork are present (`Rex5` requires
    ///   [`SequencerRegistryConfig`](crate::SequencerRegistryConfig), `Rex6` requires
    ///   [`SequencerRegistryRex6Config`](crate::SequencerRegistryRex6Config)). This surfaces a
    ///   missing config at load time rather than at the first block of the fork. A new
    ///   [`HardforkParams`] type must be registered here.
    fn validate_schedule(&self) -> Result<(), ScheduleError> {
        let scheduled =
            |fork: MegaHardfork| self.mega_fork_activation(fork) != ForkCondition::Never;

        let mut prev: Option<(MegaHardfork, u64)> = None;
        for fork in MegaHardfork::VARIANTS {
            match self.mega_fork_activation(*fork) {
                ForkCondition::Never => {}
                ForkCondition::Timestamp(timestamp) => {
                    if let Some((earlier, earlier_timestamp)) = prev {
                        if timestamp < earlier_timestamp {
                            return Err(ScheduleError::UnorderedForks {
                                earlier,
                                earlier_timestamp,
                                later: *fork,
                                later_timestamp: timestamp,
                            });
                        }
                    }
                    prev = Some((*fork, timestamp));
                }
                _ => return Err(ScheduleError::NonTimestampActivation { fork: *fork }),
            }
        }

        for fork in MegaHardfork::VARIANTS {
            if !fork.introduces_behavior() || scheduled(*fork) {
                continue;
            }
            if let Some(above) = MegaHardfork::VARIANTS
                .iter()
                .find(|g| scheduled(**g) && g.spec_id() >= fork.spec_id())
            {
                return Err(ScheduleError::SkippedRung { missing: *fork, scheduled: *above });
            }
        }

        if scheduled(MegaHardfork::Rex5) && self.fork_params::<SequencerRegistryConfig>().is_none()
        {
            return Err(ScheduleError::MissingParams {
                fork: MegaHardfork::Rex5,
                params: "SequencerRegistryConfig",
            });
        }
        if scheduled(MegaHardfork::Rex6) &&
            self.fork_params::<SequencerRegistryRex6Config>().is_none()
        {
            return Err(ScheduleError::MissingParams {
                fork: MegaHardfork::Rex6,
                params: "SequencerRegistryRex6Config",
            });
        }

        Ok(())
    }
}

/// A malformed hardfork schedule, as reported by [`MegaHardforks::validate_schedule`].
#[derive(Debug, Clone, PartialEq, Eq, derive_more::Display, derive_more::Error)]
pub enum ScheduleError {
    /// A later-declared fork activates strictly before an earlier-declared one.
    #[display(
        "hardfork {later:?} (t={later_timestamp}) activates before {earlier:?} (t={earlier_timestamp})"
    )]
    UnorderedForks {
        /// The earlier-declared fork.
        earlier: MegaHardfork,
        /// Activation timestamp of the earlier-declared fork.
        earlier_timestamp: u64,
        /// The later-declared fork that activates too early.
        later: MegaHardfork,
        /// Activation timestamp of the later-declared fork.
        later_timestamp: u64,
    },
    /// A spec-introducing fork is unscheduled while a fork of an equal-or-higher spec is
    /// scheduled — the ladder skips a rung.
    #[display("hardfork {missing:?} is unscheduled but {scheduled:?} is scheduled above it")]
    SkippedRung {
        /// The unscheduled spec-introducing fork.
        missing: MegaHardfork,
        /// A scheduled fork whose spec is equal or higher.
        scheduled: MegaHardfork,
    },
    /// A `MegaHardfork` is registered with a block-number or TTD condition, which the
    /// timestamp-scoped resolution never reports as active.
    #[display("hardfork {fork:?} must activate by timestamp, not block number or TTD")]
    NonTimestampActivation {
        /// The fork with a non-timestamp activation condition.
        fork: MegaHardfork,
    },
    /// A scheduled fork requires per-fork parameters that are not attached.
    #[display("hardfork {fork:?} is scheduled but its {params} params are not configured")]
    MissingParams {
        /// The scheduled fork whose params are missing.
        fork: MegaHardfork,
        /// The required params type.
        params: &'static str,
    },
}

/// A single fork entry: identity, activation condition, and optional per-fork parameters.
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

/// Configuration of the hardforks for `MegaETH`. It by default includes no `MegaHardfork` but
/// includes all hardforks before and including Optimism Isthmus. Optimism Isthmus is the hardfork
/// where `MegaETH` is established.
///
/// Per-fork parameters (e.g., [`SequencerRegistryConfig`](crate::SequencerRegistryConfig)) are
/// embedded in the corresponding fork entry via [`with_params`](Self::with_params).
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
                .map(|(h, c)| ForkEntry {
                    fork: Box::new(h) as Box<dyn Hardfork>,
                    condition: c,
                    params: None,
                })
                .collect(),
        }
    }
}

impl MegaHardforkConfig {
    /// Creates a new hardfork configuration with the default hardforks, i.e., all hardforks before
    /// and including Optimism Isthmus are enabled. Optimism Isthmus is the hardfork where
    /// `MegaETH` is established.
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
        ];
        Self {
            entries: forks
                .into_iter()
                .map(|(fork, condition)| ForkEntry { fork, condition, params: None })
                .collect(),
        }
    }

    /// Sets all `MegaHardfork` to be activated at timestamp 0.
    pub fn with_all_activated(self) -> Self {
        self.with_all_activated_through(MegaSpecId::default())
    }

    /// Activates every `MegaHardfork` whose spec is enabled under `spec` at timestamp 0, and
    /// leaves every later fork unregistered.
    ///
    /// This is how to express "a chain running spec N": the resulting config resolves to `spec`
    /// at any timestamp. Removing only the next fork up is not equivalent — the ladder runs past
    /// it, so the config would resolve to the newest fork still registered rather than to `spec`,
    /// and it would silently drift again the next time a spec is introduced. On top of the wrong
    /// resolved spec, the leftover later forks also keep the scheduled spec high, so every
    /// pre-block setup gate below them stays open.
    ///
    /// The *activations* in the result are a function of `spec` alone, not of what the config
    /// held before: a later fork already registered is removed rather than left in place, so the
    /// resolved spec does not depend on builder call order. Per-fork params are the exception:
    /// params on a kept fork are preserved, while params on a removed fork are dropped with its
    /// entry and are not restored by a later climb back up — re-attach them with
    /// [`with_params`](Self::with_params).
    ///
    /// Alias forks sit on their own rungs above their base (`MINI_REX_1`/`MINI_REX_2` above
    /// `MINI_REX`), so climbing through a rung includes everything below it — aliases and
    /// bases alike — with no special casing.
    pub fn with_all_activated_through(mut self, spec: MegaSpecId) -> Self {
        for fork in MegaHardfork::VARIANTS {
            if spec.reaches(fork.spec_id()) {
                self.insert(*fork, ForkCondition::Timestamp(0));
            } else {
                self = self.without(*fork);
            }
        }
        self
    }

    /// Attaches per-fork parameters to the entry identified by `P::FORK`.
    ///
    /// The fork must already exist in the config (via [`with`](Self::with) or default).
    /// Panics if the fork is not found or if `params.validate()` returns an error.
    pub fn with_params<P: HardforkParams>(mut self, params: P) -> Self {
        if let Err(e) = params.validate() {
            panic!("Invalid params for fork {:?}: {}", P::FORK, e.message,);
        }
        let entry =
            self.entries.iter_mut().find(|e| e.fork.name() == P::FORK.name()).unwrap_or_else(
                || {
                    panic!(
                        "Cannot attach params to fork {:?}: fork not registered in config. \
                     Call .with({:?}, condition) first.",
                        P::FORK,
                        P::FORK,
                    )
                },
            );
        entry.params = Some(Arc::new(params));
        self
    }

    /// Removes a `MegaHardfork` from the configuration, i.e., equivalent to setting the fork
    /// condition to [`ForkCondition::Never`].
    ///
    /// Deliberately private: `with_all_activated().without(fork)` reads as "a chain running the
    /// spec below `fork`" but does not express it — the forks above `fork` stay registered, so
    /// the config resolves above the intended rung and silently climbs again with the next spec.
    /// Say it with [`with_all_activated_through`](Self::with_all_activated_through) instead.
    ///
    /// This narrows an idiom, not a capability: [`with`](Self::with) with
    /// [`ForkCondition::Never`] produces the same unregistered fork, and must stay public because
    /// the canonical schedules use it for exactly that (testnet's `MiniRex1` / `MiniRex2` never
    /// activate). Writing a gap that way is at least visibly deliberate, and it keeps the entry
    /// so [`with_params`](Self::with_params) can still attach to it.
    fn without(mut self, hardfork: MegaHardfork) -> Self {
        self.entries.retain(|e| e.fork.name() != hardfork.name());
        self
    }

    /// Creates a new hardfork configuration with the given hardfork and condition.
    pub fn with(mut self, hardfork: impl Hardfork, condition: ForkCondition) -> Self {
        self.insert(hardfork, condition);
        self
    }

    /// Inserts a new hardfork into the configuration. If the hardfork is already present, it will
    /// be overwritten (condition updated, params preserved).
    pub fn insert(&mut self, hardfork: impl Hardfork, condition: ForkCondition) {
        let index = self.entries.iter().position(|e| e.fork.name() == hardfork.name());
        if let Some(index) = index {
            self.entries[index].condition = condition;
        } else {
            self.entries.push(ForkEntry { fork: Box::new(hardfork), condition, params: None });
        }
    }

    /// Gets `ForkCondition` by a [`Hardfork`]. If the hardfork is not present, returns `None`.
    pub fn get(&self, hardfork: impl Hardfork) -> Option<&ForkCondition> {
        self.entries.iter().find(|e| e.fork.name() == hardfork.name()).map(|e| &e.condition)
    }
}

impl EthereumHardforks for MegaHardforkConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }
}

impl OpHardforks for MegaHardforkConfig {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }
}

impl MegaHardforks for MegaHardforkConfig {
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }

    fn fork_params_any(&self, fork: MegaHardfork) -> Option<&(dyn Any + Send + Sync)> {
        self.entries.iter().find(|e| e.fork.name() == fork.name()).and_then(|e| e.params.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SequencerRegistryConfig;

    #[test]
    fn test_climbs_the_spec_ladder_rejects_malformed_lists() {
        // The real `VARIANTS` is checked at compile time by the const assertion; only
        // rejection cases can detect a weakened guard inside the checker.
        assert!(climbs_the_spec_ladder(MegaHardfork::VARIANTS));
        assert!(climbs_the_spec_ladder(&[]), "the empty list climbs trivially");
        assert!(climbs_the_spec_ladder(&[MegaHardfork::Rex]), "a single fork climbs trivially");
        assert!(
            climbs_the_spec_ladder(&[MegaHardfork::MiniRex, MegaHardfork::Rex5]),
            "gaps are fine — strictly ascending is the property, contiguity is not"
        );

        assert!(
            !climbs_the_spec_ladder(&[MegaHardfork::Rex, MegaHardfork::MiniRex]),
            "a fork mapping below its predecessor must be rejected"
        );
        assert!(
            !climbs_the_spec_ladder(&[MegaHardfork::Rex, MegaHardfork::Rex]),
            "two forks sharing a rung must be rejected — the map must stay 1:1"
        );
    }

    #[test]
    fn test_mega_hardfork_spec_ids_match_expected_specs() {
        // Note: MiniRex1 and MiniRex2 map to alias rungs whose behavior reverts to earlier specs.
        let cases = [
            (MegaHardfork::MiniRex, MegaSpecId::MINI_REX),
            (MegaHardfork::MiniRex1, MegaSpecId::MINI_REX_1),
            (MegaHardfork::MiniRex2, MegaSpecId::MINI_REX_2),
            (MegaHardfork::Rex, MegaSpecId::REX),
            (MegaHardfork::Rex1, MegaSpecId::REX1),
            (MegaHardfork::Rex2, MegaSpecId::REX2),
            (MegaHardfork::Rex3, MegaSpecId::REX3),
            (MegaHardfork::Rex4, MegaSpecId::REX4),
            (MegaHardfork::Rex5, MegaSpecId::REX5),
            (MegaHardfork::Rex6, MegaSpecId::REX6),
            (MegaHardfork::Rex7, MegaSpecId::REX7),
        ];

        for (hardfork, expected_spec) in cases {
            assert_eq!(hardfork.spec_id(), expected_spec);
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
        assert_eq!(config.op_fork_activation(OpHardfork::Isthmus), ForkCondition::Timestamp(0));
        assert_eq!(config.mega_fork_activation(MegaHardfork::MiniRex), ForkCondition::Never);
        assert!(config.fork_params::<SequencerRegistryConfig>().is_none());
    }

    #[test]
    fn test_config_builder_helpers_override_and_remove_hardforks() {
        let mut config = MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(10))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(80));

        assert_eq!(config.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(10)));
        assert_eq!(config.get(MegaHardfork::Rex4), Some(&ForkCondition::Timestamp(80)));

        config.insert(MegaHardfork::MiniRex, ForkCondition::Timestamp(20));
        assert_eq!(config.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(20)));

        let config = config.without(MegaHardfork::MiniRex);
        assert_eq!(config.get(MegaHardfork::MiniRex), None);

        let from_iter = MegaHardforkConfig::from(
            [
                (MegaHardfork::MiniRex, ForkCondition::Timestamp(1)),
                (MegaHardfork::Rex2, ForkCondition::Timestamp(2)),
            ]
            .into_iter(),
        );
        assert_eq!(from_iter.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(1)));
        assert_eq!(from_iter.get(MegaHardfork::Rex2), Some(&ForkCondition::Timestamp(2)));
    }

    #[test]
    fn test_with_all_activated_enables_all_mega_hardforks() {
        let config = MegaHardforkConfig::default().with_all_activated();

        // Driven off `VARIANTS`, which the `hardfork!` macro generates from the same variant list
        // that declares the enum. A second hand-written list here would assert only that the
        // forks someone remembered to name are activated — a new fork missing from both the
        // builder and the list would fail neither.
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
        let params = SequencerRegistryConfig {
            rex5_initial_sequencer: alloy_primitives::address!(
                "0x2222222222222222222222222222222222222222"
            ),
            rex5_initial_admin: alloy_primitives::address!(
                "0x3333333333333333333333333333333333333333"
            ),
        };

        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
            .with_params(params.clone());

        let retrieved = config
            .fork_params::<SequencerRegistryConfig>()
            .expect("should have SequencerRegistryConfig");
        assert_eq!(retrieved, &params);
    }

    #[test]
    fn test_fork_params_none_when_not_configured() {
        let config =
            MegaHardforkConfig::default().with(MegaHardfork::Rex5, ForkCondition::Timestamp(0));

        assert!(config.fork_params::<SequencerRegistryConfig>().is_none());
    }

    #[test]
    fn test_default_validate_accepts_any_value() {
        #[derive(Debug)]
        struct NullParams;

        impl HardforkParams for NullParams {
            const FORK: MegaHardfork = MegaHardfork::Rex4;
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
        #[derive(Debug)]
        struct AlwaysErrParams;

        impl HardforkParams for AlwaysErrParams {
            const FORK: MegaHardfork = MegaHardfork::Rex4;

            fn validate(&self) -> Result<(), HardforkParamsError> {
                Err(HardforkParamsError { message: "intentional test error".into() })
            }
        }

        MegaHardforkConfig::default().with_all_activated().with_params(AlwaysErrParams);
    }

    /// Mainnet runs a real behavior rollback: inside the `MiniRex1` window the scheduled spec
    /// is the alias rung `MINI_REX_1` — behavior projects to `EQUIVALENCE` while position keeps
    /// one-way setup (the Oracle predeploys) enabled.
    #[test]
    fn test_mainnet_rollback_window_behavior_rolls_back_setup_stays() {
        let hf = crate::mainnet_hardforks();
        let ForkCondition::Timestamp(rollback_start) =
            hf.mega_fork_activation(MegaHardfork::MiniRex1)
        else {
            panic!("mainnet must schedule MiniRex1 by timestamp");
        };
        let ForkCondition::Timestamp(rollback_end) =
            hf.mega_fork_activation(MegaHardfork::MiniRex2)
        else {
            panic!("mainnet must schedule MiniRex2 by timestamp");
        };
        assert!(rollback_start < rollback_end, "the rollback window must be non-empty");

        for ts in [rollback_start, rollback_start + 1, rollback_end - 1] {
            let spec = hf.spec_id(ts);
            assert_eq!(spec, MegaSpecId::MINI_REX_1, "the scheduled spec is monotone");
            assert_eq!(spec.behavior(), MegaSpecId::EQUIVALENCE, "behavior rolls back");
            // Behavior gates turn MiniRex features off; position gates keep its setup.
            assert!(!spec.is_enabled(MegaSpecId::MINI_REX));
            assert!(spec.reaches(MegaSpecId::MINI_REX));
            assert!(hf.is_mini_rex_active_at_timestamp(ts));
        }
    }

    /// The scheduled spec's position reproduces every per-fork activation event exactly, for
    /// every behavior-introducing fork, on every canonical schedule. This is what makes the
    /// position-projected predicates a no-op switch on well-formed ladders.
    ///
    /// Forks that introduce no new behavior — `MiniRex1` (rollback to `EQUIVALENCE`) and
    /// `MiniRex2` (restoration to `MINI_REX`) — may be omitted from a schedule whose ladder
    /// climbs past their rungs, so their activation events are not recoverable from position and
    /// nothing may gate on them this way. The predicate is derived rather than hardcoded so a
    /// future rollback fork is classified automatically.
    #[test]
    fn test_position_matches_per_fork_activation_for_behavior_introducing_forks() {
        for hf in [
            crate::mainnet_hardforks(),
            crate::testnet_hardforks(),
            crate::all_activated_hardforks(),
        ] {
            let mut stamps = std::vec![0u64, u64::MAX];
            for fork in MegaHardfork::VARIANTS {
                if let ForkCondition::Timestamp(t) = hf.mega_fork_activation(*fork) {
                    stamps.extend([t.saturating_sub(1), t, t.saturating_add(1)]);
                }
            }

            for ts in stamps {
                let floor = hf.max_activated_spec_id(ts);
                for fork in MegaHardfork::VARIANTS {
                    if !fork.introduces_behavior() {
                        continue;
                    }
                    assert_eq!(
                        floor.reaches(fork.spec_id()),
                        hf.mega_fork_activation(*fork).active_at_timestamp(ts),
                        "floor disagrees with per-fork activation for {fork:?} at ts={ts}"
                    );
                }
            }
        }
    }

    /// On a partial ladder — a config that schedules a later fork without its predecessors — the
    /// floor still enables every lower spec, and the spec-introducing predicates, being floor
    /// projections, report active for forks that were never scheduled. Gating on either can
    /// therefore not silently skip a predecessor's behavior; only the raw activation events
    /// distinguish the missing rungs.
    #[test]
    fn test_partial_ladder_floor_enables_unscheduled_predecessors() {
        let hf = MegaHardforkConfig::new()
            .with(MegaHardfork::Rex5, ForkCondition::Never)
            .with(MegaHardfork::Rex6, ForkCondition::Timestamp(0));

        assert_eq!(hf.mega_fork_activation(MegaHardfork::Rex5), ForkCondition::Never);
        assert_eq!(hf.mega_fork_activation(MegaHardfork::MiniRex), ForkCondition::Never);
        assert_eq!(hf.spec_id(0), MegaSpecId::REX6);

        let floor = hf.max_activated_spec_id(0);
        assert_eq!(floor, MegaSpecId::REX6);
        for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX2, MegaSpecId::REX4, MegaSpecId::REX5] {
            assert!(floor.is_enabled(spec), "floor must enable {spec:?} on a partial ladder");
        }
        // The predicates project the floor: unscheduled predecessors still report active.
        assert!(hf.is_rex_5_active_at_timestamp(0), "Rex5 predicate follows the floor");
        assert!(hf.is_mini_rex_active_at_timestamp(0), "MiniRex predicate follows the floor");
    }

    /// `with_all_activated_through` is the well-formed way to express "a chain running spec N":
    /// the resolved spec is exactly `N` under both projections, at any
    /// timestamp. The specs under test come from [`MegaSpecId::ALL`], so a newly introduced
    /// spec is covered here automatically instead of depending on a hand-written list.
    #[test]
    fn test_with_all_activated_through_resolves_to_that_spec() {
        for spec in MegaSpecId::ALL.iter().copied() {
            let config = MegaHardforkConfig::default().with_all_activated_through(spec);
            assert_eq!(config.spec_id(0), spec, "{spec:?} at genesis");
            assert_eq!(config.spec_id(u64::MAX), spec, "{spec:?} must be terminal");
            assert_eq!(config.max_activated_spec_id(0), spec, "{spec:?} floor at genesis");
            assert_eq!(
                config.max_activated_spec_id(u64::MAX),
                spec,
                "{spec:?} floor must be terminal"
            );

            // The same contract must hold when the config already carries later forks: the
            // builder states the whole ladder, so it removes what it does not activate. Without
            // that, the resolved spec would depend on which builder ran last.
            let downgraded =
                MegaHardforkConfig::default().with_all_activated().with_all_activated_through(spec);
            assert_eq!(downgraded.spec_id(u64::MAX), spec, "{spec:?} from an activated config");
            assert_eq!(
                downgraded.max_activated_spec_id(u64::MAX),
                spec,
                "{spec:?} floor from an activated config"
            );
        }
    }

    /// Removing a middle rung does NOT express "a chain running spec N". It is the partial-ladder
    /// shape: the executing spec follows the newest fork still registered, and the floor — with
    /// the predicate projected from it — keeps every lower gate open.
    #[test]
    fn test_removing_a_middle_rung_does_not_lower_the_spec() {
        let partial =
            MegaHardforkConfig::default().with_all_activated().without(MegaHardfork::Rex4);

        assert_eq!(
            partial.mega_fork_activation(MegaHardfork::Rex4),
            ForkCondition::Never,
            "Rex4 itself is unregistered"
        );
        assert_ne!(partial.spec_id(0), MegaSpecId::REX4, "the executing spec is not lowered");
        assert!(
            partial.max_activated_spec_id(0).is_enabled(MegaSpecId::REX4),
            "the floor still enables the removed fork's spec"
        );
        assert!(
            partial.is_rex_4_active_at_timestamp(0),
            "the projected predicate stays open with the floor"
        );
    }

    /// The floor and the executing spec agree on every spec at or above `REX5` across every
    /// canonical schedule, which is what makes the two-spec split in `resolve_system_address`
    /// inert today. A rollback hardfork scheduled *after* Rex5's activation that maps below
    /// `REX5` would break the agreement and must be caught here. (A rollback scheduled before
    /// Rex5 — mainnet's `MiniRex1` — keeps both sides below `REX5` and does not.)
    #[test]
    fn test_floor_and_executing_spec_agree_across_rex5_rex6_boundary() {
        for hf in [
            crate::mainnet_hardforks(),
            crate::testnet_hardforks(),
            crate::all_activated_hardforks(),
        ] {
            let mut stamps = std::vec![0u64, u64::MAX];
            for fork in MegaHardfork::VARIANTS {
                if let ForkCondition::Timestamp(t) = hf.mega_fork_activation(*fork) {
                    stamps.extend([t.saturating_sub(1), t, t.saturating_add(1)]);
                }
            }

            for ts in stamps {
                let (exec, floor) = (hf.spec_id(ts), hf.max_activated_spec_id(ts));
                for spec in
                    MegaSpecId::ALL.iter().copied().filter(|spec| spec.is_enabled(MegaSpecId::REX5))
                {
                    assert_eq!(
                        exec.is_enabled(spec),
                        floor.is_enabled(spec),
                        "executing spec and floor disagree on {spec:?} at ts={ts}"
                    );
                }
            }
        }
    }

    /// Documented domain limit: the floor is timestamp-scoped, so a `MegaHardfork` registered by
    /// block number never contributes its own spec to it. `spec_id`/`hardfork` share this
    /// limitation; every canonical schedule uses `Timestamp` or `Never`, and
    /// `validate_schedule` rejects anything else.
    #[test]
    fn test_floor_ignores_block_numbered_forks() {
        let hf = MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Block(0))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(0));

        assert!(
            !hf.mega_fork_activation(MegaHardfork::MiniRex).active_at_timestamp(0),
            "block-numbered forks are not timestamped"
        );
        // The floor comes from Rex alone; it still covers MINI_REX by ordinal inclusion, so the
        // projected predicate reports active even though the MiniRex event itself never fires.
        assert_eq!(hf.max_activated_spec_id(0), MegaSpecId::REX);
        assert!(hf.max_activated_spec_id(0).is_enabled(MegaSpecId::MINI_REX));
        assert!(hf.is_mini_rex_active_at_timestamp(0));
        assert_eq!(
            hf.validate_schedule(),
            Err(ScheduleError::NonTimestampActivation { fork: MegaHardfork::MiniRex })
        );
    }

    /// The early-exit descending scan in `max_activated_spec_id` must be observationally
    /// identical to the naive maximum over all activated forks, on every schedule shape:
    /// canonical ladders, rollback windows, partial ladders, block-numbered conditions, and the
    /// empty config.
    #[test]
    fn test_floor_early_exit_matches_naive_reference() {
        let configs = [
            crate::mainnet_hardforks(),
            crate::testnet_hardforks(),
            crate::all_activated_hardforks(),
            MegaHardforkConfig::new(),
            // Partial ladders: a lone top rung, and a lone alias fork.
            MegaHardforkConfig::new().with(MegaHardfork::Rex6, ForkCondition::Timestamp(7)),
            MegaHardforkConfig::new().with(MegaHardfork::MiniRex2, ForkCondition::Timestamp(7)),
            // Non-timestamp conditions never contribute.
            MegaHardforkConfig::new()
                .with(MegaHardfork::MiniRex, ForkCondition::Block(0))
                .with(MegaHardfork::Rex2, ForkCondition::Timestamp(7)),
        ];
        let rungs: Vec<MegaHardforkConfig> = MegaSpecId::ALL
            .iter()
            .map(|spec| MegaHardforkConfig::default().with_all_activated_through(*spec))
            .collect();

        for hf in configs.iter().chain(rungs.iter()) {
            let mut stamps = std::vec![0u64, u64::MAX];
            for fork in MegaHardfork::VARIANTS {
                if let ForkCondition::Timestamp(t) = hf.mega_fork_activation(*fork) {
                    stamps.extend([t.saturating_sub(1), t, t.saturating_add(1)]);
                }
            }
            for ts in stamps {
                let naive = MegaHardfork::VARIANTS
                    .iter()
                    .filter(|fork| hf.mega_fork_activation(**fork).active_at_timestamp(ts))
                    .map(|fork| fork.spec_id())
                    .max()
                    .unwrap_or(MegaSpecId::EQUIVALENCE);
                assert_eq!(hf.max_activated_spec_id(ts), naive, "floor diverges at ts={ts}");
            }
        }
    }

    /// Patch-fork predicates stay raw event queries: they are not recoverable from a spec
    /// ordinal, so they must not follow the floor. Testnet is the live case — its floor climbs
    /// the whole ladder while `MiniRex1`/`MiniRex2` are never scheduled.
    #[test]
    fn test_patch_fork_predicates_stay_event_scoped() {
        let hf = crate::testnet_hardforks();
        let ts = u64::MAX;
        assert!(hf.max_activated_spec_id(ts).is_enabled(MegaSpecId::REX5), "floor is high");
        assert!(!hf.is_mini_rex_1_active_at_timestamp(ts), "MiniRex1 never happened on testnet");
        assert!(!hf.is_mini_rex_2_active_at_timestamp(ts), "MiniRex2 never happened on testnet");

        // On mainnet both events did happen, and the predicates report them.
        let hf = crate::mainnet_hardforks();
        assert!(hf.is_mini_rex_1_active_at_timestamp(ts));
        assert!(hf.is_mini_rex_2_active_at_timestamp(ts));
    }

    /// Every canonical schedule and every `with_all_activated_through` rung is well-formed.
    /// `with_all_activated_through(EQUIVALENCE)` is the edge worth pinning: it registers no
    /// fork at all (a patch without its base would be an orphan) and resolves to
    /// `EQUIVALENCE` by default.
    #[test]
    fn test_validate_schedule_accepts_well_formed_ladders() {
        assert_eq!(crate::mainnet_hardforks().validate_schedule(), Ok(()));
        assert_eq!(crate::testnet_hardforks().validate_schedule(), Ok(()));
        assert_eq!(crate::all_activated_hardforks().validate_schedule(), Ok(()));
        assert_eq!(MegaHardforkConfig::new().validate_schedule(), Ok(()), "no mega forks is fine");

        for spec in MegaSpecId::ALL.iter().copied() {
            let mut config = MegaHardforkConfig::default().with_all_activated_through(spec);
            if spec.is_enabled(MegaSpecId::REX5) {
                config = config.with_params(SequencerRegistryConfig {
                    rex5_initial_sequencer: crate::MEGA_SYSTEM_ADDRESS,
                    rex5_initial_admin: crate::MEGA_SYSTEM_ADDRESS,
                });
            }
            if spec.is_enabled(MegaSpecId::REX6) {
                config =
                    config.with_params(SequencerRegistryRex6Config { rex6_min_rotation_delay: 1 });
            }
            assert_eq!(config.validate_schedule(), Ok(()), "{spec:?} rung must validate");
        }
    }

    /// A partial ladder is executable (the floor keeps setup additive) but not a valid published
    /// schedule: `validate_schedule` is the fail-fast side of that split.
    #[test]
    fn test_validate_schedule_rejects_skipped_rungs() {
        let hf = MegaHardforkConfig::new()
            .with(MegaHardfork::Rex6, ForkCondition::Timestamp(0))
            .with_params(SequencerRegistryRex6Config { rex6_min_rotation_delay: 1 });

        assert_eq!(
            hf.validate_schedule(),
            Err(ScheduleError::SkippedRung {
                missing: MegaHardfork::MiniRex,
                scheduled: MegaHardfork::Rex6
            })
        );
    }

    /// A gap in the middle of the ladder is flagged too, naming the first missing rung. Unlike
    /// the lowest rung (whose empty prefix makes it trivially spec-introducing), this exercises
    /// the prefix classification for real: `Rex` counts as a rung only because every
    /// earlier-declared fork maps strictly below it.
    #[test]
    fn test_validate_schedule_rejects_skipped_middle_rung() {
        let hf = MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(0));

        assert_eq!(
            hf.validate_schedule(),
            Err(ScheduleError::SkippedRung {
                missing: MegaHardfork::Rex,
                scheduled: MegaHardfork::Rex2
            })
        );
    }

    /// Scheduling an alias fork without its base is the rollback of a fork that never
    /// happened. Under the ascending 1:1 map this is just a skipped rung — the alias's spec
    /// sits above its base's — so the general rung check catches it without a dedicated rule.
    #[test]
    fn test_validate_schedule_rejects_alias_without_base() {
        let hf =
            MegaHardforkConfig::new().with(MegaHardfork::MiniRex1, ForkCondition::Timestamp(0));

        assert_eq!(hf.spec_id(0), MegaSpecId::MINI_REX_1);
        assert_eq!(hf.spec_id(0).behavior(), MegaSpecId::EQUIVALENCE, "execution unaffected");
        assert_eq!(
            hf.validate_schedule(),
            Err(ScheduleError::SkippedRung {
                missing: MegaHardfork::MiniRex,
                scheduled: MegaHardfork::MiniRex1
            })
        );
    }

    #[test]
    fn test_validate_schedule_rejects_unordered_forks() {
        let hf = MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(100))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(50));

        assert_eq!(
            hf.validate_schedule(),
            Err(ScheduleError::UnorderedForks {
                earlier: MegaHardfork::MiniRex,
                earlier_timestamp: 100,
                later: MegaHardfork::Rex,
                later_timestamp: 50,
            })
        );
    }

    /// A scheduled fork whose required params are missing fails at validation time instead of at
    /// the first block of the fork.
    #[test]
    fn test_validate_schedule_requires_scheduled_fork_params() {
        let rex5_no_params =
            MegaHardforkConfig::default().with_all_activated_through(MegaSpecId::REX5);
        assert_eq!(
            rex5_no_params.validate_schedule(),
            Err(ScheduleError::MissingParams {
                fork: MegaHardfork::Rex5,
                params: "SequencerRegistryConfig"
            })
        );

        let rex6_no_params = MegaHardforkConfig::default()
            .with_all_activated_through(MegaSpecId::REX6)
            .with_params(SequencerRegistryConfig {
                rex5_initial_sequencer: crate::MEGA_SYSTEM_ADDRESS,
                rex5_initial_admin: crate::MEGA_SYSTEM_ADDRESS,
            });
        assert_eq!(
            rex6_no_params.validate_schedule(),
            Err(ScheduleError::MissingParams {
                fork: MegaHardfork::Rex6,
                params: "SequencerRegistryRex6Config"
            })
        );
    }

    #[test]
    fn test_hardfork_and_spec_id_follow_latest_active_timestamp() {
        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(100))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(200))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(300))
            .with(MegaHardfork::Rex6, ForkCondition::Timestamp(400))
            .with(MegaHardfork::Rex7, ForkCondition::Timestamp(500));

        assert_eq!(config.hardfork(99), None);
        assert_eq!(config.hardfork(100), Some(MegaHardfork::MiniRex));
        assert_eq!(config.hardfork(200), Some(MegaHardfork::Rex4));
        assert_eq!(config.hardfork(300), Some(MegaHardfork::Rex5));
        assert_eq!(config.hardfork(400), Some(MegaHardfork::Rex6));
        assert_eq!(config.hardfork(500), Some(MegaHardfork::Rex7));
        assert_eq!(config.spec_id(99), MegaSpecId::EQUIVALENCE);
        assert_eq!(config.spec_id(100), MegaSpecId::MINI_REX);
        assert_eq!(config.spec_id(200), MegaSpecId::REX4);
        assert_eq!(config.spec_id(300), MegaSpecId::REX5);
        assert_eq!(config.spec_id(400), MegaSpecId::REX6);
        assert_eq!(config.spec_id(500), MegaSpecId::REX7);
    }
}
