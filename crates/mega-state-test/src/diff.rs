//! Differential execution: run one fixture under two specs and judge any disagreement.
//!
//! A state-test fixture pins what a transaction must produce, but only for a spec someone has
//! already computed an expectation for. For an unstable spec there is no such expectation, so a
//! corpus sweep can only check that execution stays self-consistent — that no invariant trips.
//! This module supplies the missing half: it executes the same fixture under the unstable spec
//! and under the frozen spec it inherits from, and asks whether the two agree.
//!
//! Disagreement is not by itself a defect — the new spec is new precisely because it changes
//! something. What makes the question decidable is that Rex7 states the conditions under which it
//! may *not* differ (`docs/spec/upgrades/rex7.md`, "Precision invariant"):
//!
//! > For every transaction that stays within every runtime resource limit, in which no frame ends
//! > in an exceptional halt, and in which no `disableVolatileDataAccess` guard rejects an opcode,
//! > a node MUST produce the same recorded compute-gas total, the same four-dimension resource
//! > usage, the same receipt `gas_used`, the same execution result, and the same state under Rex7
//! > as under Rex6.
//!
//! Read as a contrapositive, that sentence is a classifier. The quantities after "MUST produce"
//! are what [`SpecOutcome`] compares; the three clauses before it are the hypotheses
//! [`Hypothesis`] enumerates. Two specs that disagree on a compared quantity must therefore show
//! that one of the three hypotheses is false — and the classifier demands positive evidence of
//! that from the execution itself, never a list of fixtures allowed to differ.
//!
//! A disagreement with no such evidence is [`DiffClass::Unexplained`]: either the implementation
//! deviates from the spec, or the spec's invariant is wrong. Both are findings.
//!
//! # What counts as evidence
//!
//! The fixture is the input under test, so nothing the fixture authors may license a difference —
//! otherwise a contract that writes four chosen bytes to its revert buffer would buy itself an
//! exemption from the whole comparison. Every observation the classifier makes therefore carries
//! a [`Provenance`], and only [`Provenance::Execution`] observations — the EVM's own verdict on a
//! frame, the typed halt reason, the `MegaETH` trackers' own counters — can falsify a hypothesis.
//! Observations read out of revert-payload bytes are [`Provenance::Payload`]: they are reported,
//! because they are what a human triaging a flagged unit wants to see, and they license nothing.
//! `test_only_execution_provenance_licenses` holds that line for mechanisms added later.
//!
//! One hypothesis has no producer under that rule. A `disableVolatileDataAccess` rejection is
//! visible only as revert-payload bytes: `MegaETH` writes the guard's payload into the frame
//! result, and a contract can write the same bytes with a plain `REVERT`. Telling the two apart
//! needs a signal the runner cannot read — the trackers' latch is `pub(crate)`, and the one
//! public entry point that reports it (`AdditionalLimit::check_limit`) latches as a side effect
//! and would change the execution under observation. So a difference that only a guard rejection
//! explains is reported for a human rather than licensed, which is the safe direction: the gate
//! over-reports instead of granting an exemption on the strength of bytes the fixture chose.

use crate::{
    panic_capture,
    runner::{
        configure_max_blobs, execution_status, external_envs_for, find_all_json_tests, halt_reason,
        inject_block_hashes, prune_base_fee_vault_changes, resolve_chain_id,
        set_cfg_spec_and_mainnet_gas_params, skip_test, vector_label, FixtureScan, TestError,
        TestErrorKind, UnitStatus,
    },
    types::{tx_env_at, SpecName, TestSuite, TestUnit, TxPartIndices},
    utils::{log_rlp_hash, state_merkle_trie_root},
};
use indicatif::{ProgressBar, ProgressDrawTarget};
use mega_evm::{
    alloy_sol_types::SolError,
    revm::{
        context::{cfg::CfgEnv, result::ExecutionResult},
        database,
        database_interface::DatabaseCommit,
        handler::FrameResult,
        inspector::Inspector,
        interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInput},
        primitives::{Bytes, B256},
    },
    MegaContext, MegaEvm, MegaHaltReason, MegaLimitExceeded, MegaTransaction,
    MegaTransactionNew as _, VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

/// A hypothesis of the Rex7 precision invariant.
///
/// The invariant holds the two specs to identical output only while all three are true, so
/// evidence that one is false is what licenses a difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hypothesis {
    /// "stays within every runtime resource limit".
    WithinLimits,
    /// "no frame ends in an exceptional halt".
    NoExceptionalHalt,
    /// "no `disableVolatileDataAccess` guard rejects an opcode".
    NoDisabledVolatileReject,
}

/// Where an observation came from, and therefore whether the fixture could have authored it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provenance {
    /// Read from the execution itself: the EVM's verdict on a frame, the typed halt reason, or a
    /// `MegaETH` tracker's own counter. A fixture can cause such an observation — that is what
    /// running it means — but it cannot fabricate one without the machinery actually firing.
    Execution,
    /// Inferred from revert-payload bytes, which any contract can write with a plain `REVERT`.
    Payload,
}

/// A `MegaETH` mechanism observed in a differential run.
///
/// Each variant is a fact read off an execution, not an interpretation of one. A variant that
/// falsifies a hypothesis of the precision invariant reports it through
/// [`Mechanism::falsifies`]; the rest are recorded for the mechanism distribution but never
/// explain a difference on their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mechanism {
    /// The transaction halted on a `MegaETH` resource limit (compute gas, data size, KV
    /// updates, state growth).
    ResourceLimitHalt,
    /// The transaction halted on the detained compute-gas limit.
    DetentionHalt,
    /// A resource-limit exceed rescued the transaction's remaining gas for the sender.
    ///
    /// The rescue is the tell for a limit exceed whose halt the outer accounting has already
    /// rewritten — a failed deposit, for instance, reports its whole gas limit.
    GasRescued,
    /// A frame reverted carrying the `MegaLimitExceeded` selector.
    ///
    /// Reported, never licensing: `MegaETH` writes that payload into a frame result, and so can
    /// any contract. See the module docs on what counts as evidence.
    LimitRevertPayload,
    /// A frame ended in an exceptional halt.
    ///
    /// Counted at the frame the EVM finished, so it covers an inner frame the caller absorbed, a
    /// precompile that failed, and a call or creation refused before a frame opened — none of
    /// which the transaction's own result shows.
    ExceptionalHalt,
    /// Rex7 booked a destroyed compute-gas remainder: an envelope was lost without being
    /// executed.
    ///
    /// Recorded, never licensing. The remainder is not observed but *derived*, from a
    /// conservation law over the transaction's whole envelope, so a missing term in that law
    /// produces a non-zero remainder with no halt behind it. Letting it license the difference it
    /// causes would make the one number a defect would move into that defect's own alibi. The
    /// halt it claims has an independent witness — the frame the EVM finished — and that witness
    /// is what licenses.
    DestroyedComputeGas,
    /// A frame reverted carrying the `VolatileDataAccessDisabled` selector.
    ///
    /// Reported, never licensing, for the same reason as [`Mechanism::LimitRevertPayload`].
    VolatileDisabledPayload,
    /// The two specs recorded different volatile-data access marks.
    ///
    /// Rex7 moves the beneficiary / oracle mark to the point where the target account is loaded,
    /// so a frame that cannot afford the fees charged before that load marks under Rex6 and not
    /// under Rex7. On its own this changes nothing observable — it changes the *limit*, and only
    /// crossing that limit changes an outcome — so it is recorded, not accepted as an
    /// explanation.
    DetentionMarkDiff,
    /// A detention cap was in force at the end of the transaction.
    ///
    /// Informational for the same reason as [`Mechanism::DetentionMarkDiff`]: a cap nobody
    /// reached explains nothing.
    DetentionInForce,
}

impl Mechanism {
    /// The invariant hypothesis this mechanism falsifies, if any.
    ///
    /// Only an [`Provenance::Execution`] observation may return `Some`; see
    /// [`Mechanism::provenance`] and the module docs.
    pub const fn falsifies(self) -> Option<Hypothesis> {
        match self {
            Self::ResourceLimitHalt | Self::DetentionHalt | Self::GasRescued => {
                Some(Hypothesis::WithinLimits)
            }
            Self::ExceptionalHalt => Some(Hypothesis::NoExceptionalHalt),
            Self::LimitRevertPayload |
            Self::VolatileDisabledPayload |
            Self::DestroyedComputeGas |
            Self::DetentionMarkDiff |
            Self::DetentionInForce => None,
        }
    }

    /// Whether this observation is read off the execution or off bytes the fixture chose.
    pub const fn provenance(self) -> Provenance {
        match self {
            Self::ResourceLimitHalt |
            Self::DetentionHalt |
            Self::GasRescued |
            Self::ExceptionalHalt |
            Self::DestroyedComputeGas |
            Self::DetentionMarkDiff |
            Self::DetentionInForce => Provenance::Execution,
            Self::LimitRevertPayload | Self::VolatileDisabledPayload => Provenance::Payload,
        }
    }

    /// Stable lower-case label, for tallies and reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ResourceLimitHalt => "resource_limit_halt",
            Self::DetentionHalt => "detention_halt",
            Self::GasRescued => "gas_rescued",
            Self::LimitRevertPayload => "limit_revert_payload",
            Self::ExceptionalHalt => "exceptional_halt",
            Self::DestroyedComputeGas => "destroyed_compute_gas",
            Self::VolatileDisabledPayload => "volatile_disabled_payload",
            Self::DetentionMarkDiff => "detention_mark_diff",
            Self::DetentionInForce => "detention_in_force",
        }
    }
}

/// What kind of halt a `MegaHaltReason` is.
///
/// Classified by matching the reason's own variants, with no catch-all arm: a `MegaHaltReason`
/// added later fails to compile here until someone decides what it means for the invariant.
/// The rule it replaces — "a halt whose `Debug` form does not start with `Base` is a resource
/// limit" — granted every future variant, and today's `SystemTxInvalidCallee`, the standing of a
/// crossed resource limit, which licenses a difference on any quantity at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HaltKind {
    /// One of the four `MegaETH` per-transaction resource limits.
    ResourceLimit,
    /// The detained compute-gas limit.
    Detention,
    /// A halt that is not a resource limit: the inherited EVM's own halts, and the
    /// `MegaETH`-specific halts that are not metering failures.
    Other,
}

/// Classifies a halt reason for the [`Mechanism`] it produces.
pub const fn halt_kind(reason: &MegaHaltReason) -> HaltKind {
    match reason {
        MegaHaltReason::DataLimitExceeded { .. } |
        MegaHaltReason::KVUpdateLimitExceeded { .. } |
        MegaHaltReason::ComputeGasLimitExceeded { .. } |
        MegaHaltReason::StateGrowthLimitExceeded { .. } => HaltKind::ResourceLimit,
        MegaHaltReason::VolatileDataAccessOutOfGas { .. } => HaltKind::Detention,
        MegaHaltReason::Base(_) | MegaHaltReason::SystemTxInvalidCallee { .. } => HaltKind::Other,
    }
}

/// A quantity the precision invariant requires the two specs to agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiffField {
    /// Post-state trie root over the fixture's account closure.
    StateRoot,
    /// RLP hash of the emitted logs.
    LogsRoot,
    /// Receipt `gas_used`.
    GasUsed,
    /// `success` / `revert` / `halt`.
    Status,
    /// Halt reason, when the status is `halt`.
    HaltReason,
    /// Transaction output bytes.
    Output,
    /// Reported compute-gas total.
    ComputeGasUsed,
    /// Data-size dimension usage.
    DataSize,
    /// KV-update dimension usage.
    KvUpdates,
    /// State-growth dimension usage.
    StateGrowth,
}

impl DiffField {
    /// Stable lower-case label, for reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::StateRoot => "state_root",
            Self::LogsRoot => "logs_root",
            Self::GasUsed => "gas_used",
            Self::Status => "status",
            Self::HaltReason => "halt_reason",
            Self::Output => "output",
            Self::ComputeGasUsed => "compute_gas_used",
            Self::DataSize => "data_size",
            Self::KvUpdates => "kv_updates",
            Self::StateGrowth => "state_growth",
        }
    }

    /// Whether an exceptional halt, on its own, may move this field.
    ///
    /// The exceptional-halt carve-out settles a halted frame's whole budget as compute gas, which
    /// raises the *reported* compute total and nothing else: the receipt, the state and the other
    /// three dimensions are explicitly unchanged by it. Every other field needs the transaction
    /// to have taken a different path, which under Rex7 means a resource limit was crossed or a
    /// guard rejected an opcode.
    const fn movable_by_halt_alone(self) -> bool {
        matches!(self, Self::ComputeGasUsed)
    }
}

/// One side of a differential run.
///
/// The first group is what the precision invariant compares; the rest is the evidence a
/// disagreement is judged against.
#[derive(Debug, Clone)]
pub struct SpecOutcome {
    /// Post-state trie root over the fixture's account closure.
    pub state_root: B256,
    /// RLP hash of the emitted logs.
    pub logs_root: B256,
    /// Receipt `gas_used`.
    pub gas_used: u64,
    /// `success` / `revert` / `halt`.
    pub status: String,
    /// Halt reason (`Debug` form) when the status is `halt`.
    ///
    /// Compared as a quantity and printed in reports. What the halt *means* to the classifier is
    /// [`SpecOutcome::halt_kind`], read off the typed reason rather than off this rendering.
    pub halt_reason: Option<String>,
    /// What kind of halt the transaction ended in, when it halted.
    pub halt_kind: Option<HaltKind>,
    /// Transaction output bytes, if any.
    pub output: Option<Bytes>,
    /// Reported compute-gas total.
    pub compute_gas_used: u64,
    /// Data-size dimension usage.
    pub data_size: u64,
    /// KV-update dimension usage.
    pub kv_updates: u64,
    /// State-growth dimension usage.
    pub state_growth: u64,

    /// The part of the reported compute total that was destroyed rather than performed (Rex7+).
    pub compute_gas_destroyed: u64,
    /// The part of the reported compute total that every compute-gas limit is evaluated against.
    pub compute_gas_enforced: u64,
    /// Gas a resource-limit exceed rescued for the sender.
    pub rescued_gas: u64,
    /// The detained compute-gas limit in force at the end of the transaction, if any.
    pub detained_limit: Option<u64>,
    /// Bitmap of the volatile data the transaction accessed.
    pub volatile_access: u16,
    /// Per-frame evidence, when the run collected it (see [`FrameEvidence`]).
    pub frames: Option<FrameEvidence>,
}

impl SpecOutcome {
    /// Mechanisms visible in this single execution.
    fn mechanisms(&self) -> Vec<Mechanism> {
        let mut found = Vec::new();
        match self.halt_kind {
            Some(HaltKind::ResourceLimit) => found.push(Mechanism::ResourceLimitHalt),
            Some(HaltKind::Detention) => found.push(Mechanism::DetentionHalt),
            Some(HaltKind::Other) | None => {}
        }
        if self.rescued_gas > 0 {
            found.push(Mechanism::GasRescued);
        }
        if self.compute_gas_destroyed > 0 {
            found.push(Mechanism::DestroyedComputeGas);
        }
        if self.status == "halt" {
            found.push(Mechanism::ExceptionalHalt);
        }
        if self.detained_limit.is_some() {
            found.push(Mechanism::DetentionInForce);
        }
        if let Some(frames) = &self.frames {
            if frames.halted > 0 {
                found.push(Mechanism::ExceptionalHalt);
            }
            if frames.limit_revert_payloads > 0 {
                found.push(Mechanism::LimitRevertPayload);
            }
            if frames.volatile_disabled_payloads > 0 {
                found.push(Mechanism::VolatileDisabledPayload);
            }
        }
        found
    }
}

/// Per-frame facts collected by [`FrameEvidenceInspector`].
///
/// A transaction's own result hides most of what happens below it: an inner frame that halted and
/// was absorbed by its caller, a precompile that failed, a call refused before a frame opened.
/// All three falsify a hypothesis of the precision invariant and none of them is visible from the
/// outside, so the classifier collects them from the frames themselves when the cheap evidence
/// runs out.
///
/// The three counters do not carry equal weight. [`FrameEvidence::halted`] is the EVM's own
/// verdict on the frame; the other two are what the frame put in its revert buffer, which a
/// contract writes as freely as `MegaETH` does. They are counted for the report and classified as
/// [`Provenance::Payload`], so they never license a difference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameEvidence {
    /// Frames whose final result was an exceptional halt.
    pub halted: u32,
    /// Frames that reverted carrying the `MegaLimitExceeded` selector, whoever wrote it.
    pub limit_revert_payloads: u32,
    /// Frames that reverted carrying the `VolatileDataAccessDisabled` selector, whoever wrote it.
    pub volatile_disabled_payloads: u32,
}

/// Read-only inspector that records each frame's final result.
///
/// It implements `frame_end` and nothing else, so it neither rewrites a frame result nor touches
/// the interpreter's gas counter — the two things the Rex7 accounting notes call out as making an
/// inspected execution diverge from an uninspected one.
#[derive(Debug, Default)]
pub struct FrameEvidenceInspector {
    evidence: FrameEvidence,
}

impl FrameEvidenceInspector {
    /// The facts collected so far.
    pub const fn evidence(&self) -> FrameEvidence {
        self.evidence
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for FrameEvidenceInspector {
    fn frame_end(
        &mut self,
        _context: &mut CTX,
        _frame_input: &FrameInput,
        frame_result: &mut FrameResult,
    ) {
        // `frame_end` is the one hook revm calls for *every* frame outcome — an interpreter frame
        // that ran, a precompile answered without a frame, and a frame init the EVM refused —
        // and it runs after the create-return processing that can still turn a successful
        // constructor into a halt.
        let result = frame_result.interpreter_result();
        if result.result.is_halt() {
            self.evidence.halted += 1;
            return;
        }
        if !result.result.is_revert() {
            return;
        }
        // A selector match says what the frame's revert buffer starts with and nothing more:
        // `REVERT` copies whatever memory the contract points it at. Both counters are recorded
        // as claims, for a human reading a flagged unit, and are never treated as evidence.
        match result.output.get(..4) {
            Some(s) if s == MegaLimitExceeded::SELECTOR => {
                self.evidence.limit_revert_payloads += 1;
            }
            Some(s) if s == VOLATILE_DATA_ACCESS_DISABLED_SELECTOR => {
                self.evidence.volatile_disabled_payloads += 1;
            }
            _ => {}
        }
    }
}

/// How a fixture unit's two executions compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffClass {
    /// Both specs agreed on every compared quantity.
    Pass,
    /// The specs disagreed, and the disagreement carries evidence that a hypothesis of the
    /// precision invariant does not hold.
    Explained,
    /// The specs disagreed with no such evidence.
    ///
    /// Either the implementation deviates from the spec, or the spec's invariant is wrong.
    Unexplained,
    /// Neither spec executed the transaction, and both declined it the same way.
    ///
    /// A fixture the runner rejects before execution (an intrinsic-gas overrun, an unsupported
    /// transaction shape, a fixture defect) says nothing about either spec's semantics as long as
    /// both sides reject it identically — validation is spec-independent here, so an *asymmetric*
    /// rejection is a difference and is classified as one.
    Skipped,
    /// Executing the unit panicked on at least one side.
    Panic,
}

impl DiffClass {
    /// Stable upper-case label, for tallies and reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Explained => "EXPLAINED",
            Self::Unexplained => "UNEXPLAINED",
            Self::Skipped => "SKIPPED",
            Self::Panic => "PANIC",
        }
    }
}

/// The verdict on one fixture unit.
#[derive(Debug, Clone)]
pub struct UnitDiff {
    /// The unit's key in the fixture's test-suite map.
    pub name: String,
    /// The fixture file the unit came from.
    pub path: String,
    /// How the two executions compare.
    pub class: DiffClass,
    /// Quantities the two specs disagreed on.
    pub fields: Vec<DiffField>,
    /// Mechanisms observed on either side.
    pub mechanisms: Vec<Mechanism>,
    /// Why the unit is [`DiffClass::Skipped`] or [`DiffClass::Panic`], or what the two sides
    /// disagreed on in detail.
    pub detail: Option<String>,
}

/// Which specs a differential run compares.
///
/// Only [`DiffSpecs::SUPPORTED`] can be constructed. The classifier is not a general-purpose
/// two-spec comparator: every rule in it is a reading of one sentence, the Rex7 precision
/// invariant, which relates Rex7 to Rex6 and says nothing about any other pair. Pointed at
/// Rex5-against-Rex4 it would apply Rex7's licence to a pair that never had one — deciding, from
/// mechanisms that are not evidence for anything there, that a difference is fine.
///
/// [`DiffSpecs::new`] is that restriction, so the fields it validates are private: a public field
/// is a second way to build the value, and the classifier cannot tell a pair that came through the
/// check from one that was assembled around it.
///
/// ```
/// use state_test::{diff::DiffSpecs, types::SpecName};
///
/// let (target, base) = DiffSpecs::SUPPORTED;
/// let specs = DiffSpecs::new(target, base).expect("the supported pair");
/// assert_eq!((specs.target(), specs.base()), (SpecName::Rex7, SpecName::Rex6));
/// assert!(DiffSpecs::new(SpecName::Rex6, SpecName::Rex5).is_err());
/// ```
///
/// The same pair the constructor refuses, assembled directly, does not compile:
///
/// ```compile_fail
/// use state_test::{diff::DiffSpecs, types::SpecName};
///
/// let specs = DiffSpecs { target: SpecName::Rex6, base: SpecName::Rex5 };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffSpecs {
    /// The spec under test, normally the unstable one.
    target: SpecName,
    /// The frozen spec the target inherits from.
    base: SpecName,
}

impl DiffSpecs {
    /// The one pair the classifier has an invariant for: Rex7 against Rex6.
    pub const SUPPORTED: (SpecName, SpecName) = (SpecName::Rex7, SpecName::Rex6);

    /// Builds the spec pair, rejecting any pair the classifier has no invariant for.
    ///
    /// # Errors
    ///
    /// Returns a message naming the supported pair when `target` / `base` is not it.
    pub fn new(target: SpecName, base: SpecName) -> Result<Self, String> {
        let (want_target, want_base) = Self::SUPPORTED;
        if (target, base) != (want_target, want_base) {
            return Err(format!(
                "a differential run is only defined for {want_target:?} against {want_base:?}, \
                 not {target:?} against {base:?}: the precision invariant that decides when a \
                 difference is licensed is {want_target:?}'s, and no other pair states one"
            ));
        }
        Ok(Self { target, base })
    }

    /// The spec under test.
    pub const fn target(&self) -> SpecName {
        self.target
    }

    /// The frozen spec the target is judged against.
    pub const fn base(&self) -> SpecName {
        self.base
    }
}

/// Runs one unit's transaction vector under both specs and classifies the result.
///
/// `collect_evidence` decides whether the second, inspected pass runs when the cheap evidence does
/// not settle the case; see the body for why it is staged.
pub fn diff_unit(
    unit: &TestUnit,
    indexes: TxPartIndices,
    specs: DiffSpecs,
    collect_evidence: bool,
) -> UnitDiffOutcome {
    let (target_spec, base_spec) = (specs.target(), specs.base());
    let target = execute_unit_outcome(unit, indexes, &target_spec, false);
    let base = execute_unit_outcome(unit, indexes, &base_spec, false);

    let (target, base) = match (target, base) {
        (Ok(t), Ok(b)) => (t, b),
        (Err(t), Err(b)) => {
            let (t, b) = (t.to_string(), b.to_string());
            return if t == b {
                UnitDiffOutcome::skipped(t)
            } else {
                // Validation is the same code on both specs, so two different rejections are a
                // difference in their own right — and one no execution evidence can explain,
                // because neither side executed anything.
                UnitDiffOutcome::unexplained(
                    vec![],
                    vec![],
                    format!("both specs rejected the transaction, differently: {t} != {b}"),
                )
            };
        }
        (Ok(_), Err(e)) => {
            return UnitDiffOutcome::unexplained(
                vec![],
                vec![],
                format!(
                    "{} executed but {} rejected the transaction: {e}",
                    label(target_spec),
                    label(base_spec)
                ),
            )
        }
        (Err(e), Ok(_)) => {
            return UnitDiffOutcome::unexplained(
                vec![],
                vec![],
                format!(
                    "{} rejected the transaction but {} executed it: {e}",
                    label(target_spec),
                    label(base_spec)
                ),
            )
        }
    };

    let fields = compare(&target, &base);
    if fields.is_empty() {
        return UnitDiffOutcome::pass(collect_mechanisms(&target, &base));
    }

    let verdict = judge(&fields, &target, &base);
    if verdict.class != DiffClass::Unexplained || !collect_evidence {
        return verdict;
    }

    // Stage two. The cheap evidence found nothing, so re-run both sides with the frame inspector,
    // which sees the frames the transaction's own result hides. It costs an inspected execution
    // only for the units that reach here, instead of on every unit in the corpus.
    let (Ok(target), Ok(base)) = (
        execute_unit_outcome(unit, indexes, &target_spec, true),
        execute_unit_outcome(unit, indexes, &base_spec, true),
    ) else {
        return verdict;
    };
    let inspected_fields = compare(&target, &base);
    if inspected_fields.is_empty() {
        // The inspected pair agrees where the uninspected pair did not: the inspector moved the
        // execution, so its evidence does not describe the difference under judgement.
        return UnitDiffOutcome {
            detail: Some(
                "uninspected runs disagreed but inspected runs agreed; frame evidence discarded"
                    .to_string(),
            ),
            ..verdict
        };
    }
    judge(&inspected_fields, &target, &base)
}

/// The verdict body of [`UnitDiff`], before the unit's name and path are attached.
#[derive(Debug, Clone)]
pub struct UnitDiffOutcome {
    /// How the two executions compare.
    pub class: DiffClass,
    /// Quantities the two specs disagreed on.
    pub fields: Vec<DiffField>,
    /// Mechanisms observed on either side.
    pub mechanisms: Vec<Mechanism>,
    /// Supporting detail for the verdict.
    pub detail: Option<String>,
}

impl UnitDiffOutcome {
    fn pass(mechanisms: Vec<Mechanism>) -> Self {
        Self { class: DiffClass::Pass, fields: vec![], mechanisms, detail: None }
    }

    fn skipped(detail: String) -> Self {
        Self { class: DiffClass::Skipped, fields: vec![], mechanisms: vec![], detail: Some(detail) }
    }

    fn unexplained(fields: Vec<DiffField>, mechanisms: Vec<Mechanism>, detail: String) -> Self {
        Self { class: DiffClass::Unexplained, fields, mechanisms, detail: Some(detail) }
    }

    /// Attaches the unit's identity to the verdict.
    pub fn named(self, name: String, path: String) -> UnitDiff {
        UnitDiff {
            name,
            path,
            class: self.class,
            fields: self.fields,
            mechanisms: self.mechanisms,
            detail: self.detail,
        }
    }
}

/// Human-facing name of a spec, for report text.
fn label(spec: SpecName) -> String {
    format!("{spec:?}")
}

/// Mechanisms visible on either side, deduplicated and ordered.
fn collect_mechanisms(target: &SpecOutcome, base: &SpecOutcome) -> Vec<Mechanism> {
    let mut found = target.mechanisms();
    found.extend(base.mechanisms());
    if target.volatile_access != base.volatile_access {
        found.push(Mechanism::DetentionMarkDiff);
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// The quantities on which the two sides disagree.
///
/// Public together with [`judge`] so the classifier's verdict can be exercised against outcomes
/// taken from real executions, rather than only against hand-built ones.
pub fn compare(target: &SpecOutcome, base: &SpecOutcome) -> Vec<DiffField> {
    let mut fields = Vec::new();
    let mut push = |differs: bool, field: DiffField| {
        if differs {
            fields.push(field);
        }
    };
    push(target.state_root != base.state_root, DiffField::StateRoot);
    push(target.logs_root != base.logs_root, DiffField::LogsRoot);
    push(target.gas_used != base.gas_used, DiffField::GasUsed);
    push(target.status != base.status, DiffField::Status);
    push(target.halt_reason != base.halt_reason, DiffField::HaltReason);
    push(target.output != base.output, DiffField::Output);
    push(target.compute_gas_used != base.compute_gas_used, DiffField::ComputeGasUsed);
    push(target.data_size != base.data_size, DiffField::DataSize);
    push(target.kv_updates != base.kv_updates, DiffField::KvUpdates);
    push(target.state_growth != base.state_growth, DiffField::StateGrowth);
    fields
}

/// Decides whether the observed mechanisms license the observed differences.
///
/// The two tiers come straight from what each hypothesis can move. Falsifying "within every
/// resource limit" or "no guard rejected an opcode" changes which opcodes ran, so it can move any
/// compared quantity. Falsifying "no frame ended in an exceptional halt" only re-attributes a
/// halted frame's budget, which the spec confines to the reported compute total: "The receipt
/// `gas_used`, the halt or revert reported, and the execution success or failure of the outer
/// transaction are unchanged by the destroyed half of that carve-out." An exceptional halt is
/// therefore not accepted as the explanation for a state-root or receipt difference.
pub fn judge(fields: &[DiffField], target: &SpecOutcome, base: &SpecOutcome) -> UnitDiffOutcome {
    let mechanisms = collect_mechanisms(target, base);
    let falsified: Vec<Hypothesis> = {
        let mut h: Vec<_> = mechanisms.iter().filter_map(|m| m.falsifies()).collect();
        h.sort_unstable();
        h.dedup();
        h
    };
    let path_changed = falsified
        .iter()
        .any(|h| matches!(h, Hypothesis::WithinLimits | Hypothesis::NoDisabledVolatileReject));
    let halted = falsified.contains(&Hypothesis::NoExceptionalHalt);

    let unexplained: Vec<DiffField> = fields
        .iter()
        .copied()
        .filter(|f| !(path_changed || (halted && f.movable_by_halt_alone())))
        .collect();

    if unexplained.is_empty() {
        return UnitDiffOutcome {
            class: DiffClass::Explained,
            fields: fields.to_vec(),
            mechanisms,
            detail: None,
        };
    }
    let detail = format!(
        "no evidence licenses a difference on: {}",
        unexplained.iter().map(|f| f.label()).collect::<Vec<_>>().join(", ")
    );
    UnitDiffOutcome {
        class: DiffClass::Unexplained,
        fields: fields.to_vec(),
        mechanisms,
        detail: Some(detail),
    }
}

/// Executes one unit's given transaction vector under `spec` and collects its outcome and
/// evidence.
///
/// Mirrors the validation path exactly — the same config, block environment, external
/// environment, block hashes and `BaseFeeVault` pruning — so the roots it computes are the roots
/// validation would check.
///
/// `collect_evidence` runs the execution under [`FrameEvidenceInspector`], which is what makes an
/// inner frame's outcome visible; it costs an inspected interpreter loop, so the differential
/// classifier turns it on only for the units it cannot settle without it.
pub fn execute_unit_outcome(
    unit: &TestUnit,
    indexes: TxPartIndices,
    spec: &SpecName,
    collect_evidence: bool,
) -> Result<SpecOutcome, TestErrorKind> {
    let mut cfg = CfgEnv::default();
    // See `execute_test_suite`: revm-27 chain-id gate-off (revm 40 default is true).
    cfg.tx_chain_id_check = false;
    cfg.chain_id = resolve_chain_id(&unit.env)?;
    set_cfg_spec_and_mainnet_gas_params(
        &mut cfg,
        spec.to_spec_id().map_err(|e| TestErrorKind::FixtureError(format!("spec: {e}")))?,
    );
    configure_max_blobs(&mut cfg);

    let block = unit.block_env(&cfg);
    let tx = tx_env_at(unit, indexes)?;

    let cache = unit.state();
    let mut state =
        database::State::builder().with_cached_prestate(cache).with_bundle_update().build();
    inject_block_hashes(&mut state, unit)?;

    let evm_context = MegaContext::default()
        .with_db(&mut state)
        .with_cfg(cfg)
        .with_block(block)
        .with_external_envs(external_envs_for(unit)?.into());
    let mut megatx = MegaTransaction::new(tx);
    megatx.enveloped_tx = Some(Bytes::default());

    let (executed, frames, ctx) = if collect_evidence {
        let mut evm = MegaEvm::new(evm_context).with_inspector(FrameEvidenceInspector::default());
        let executed = evm.execute_transaction(megatx);
        let inner = evm.into_inner();
        let frames = Some(inner.inspector.evidence());
        (executed, frames, inner.ctx)
    } else {
        let mut evm = MegaEvm::new(evm_context);
        let executed = evm.execute_transaction(megatx);
        let inner = evm.into_inner();
        (executed, None, inner.ctx)
    };

    // Read the trackers before the context is dismantled: they carry the transaction's final
    // limit and detention state, which no execution result exposes.
    let rescued_gas = ctx.additional_limit.borrow().rescued_gas;
    let (detained_limit, volatile_access) = {
        let tracker = ctx.volatile_data_tracker.borrow();
        (tracker.get_compute_gas_limit(), tracker.get_volatile_data_accessed().bits())
    };
    let db = ctx.into_inner().journaled_state.database;

    let outcome = executed.map_err(|e| TestErrorKind::FixtureError(e.to_string()))?;
    let compute_gas_used = outcome.compute_gas_used;
    let compute_gas_destroyed = outcome.compute_gas_destroyed;
    let compute_gas_enforced = outcome.compute_gas_enforced;
    let data_size = outcome.data_size;
    let kv_updates = outcome.kv_updates;
    let state_growth = outcome.state_growth_used;
    let result = outcome.result_and_state.result;

    // `execute_transaction` finalizes but does not commit; the roots are taken over the committed
    // cache, exactly as the `transact_commit` validation path does.
    db.commit(outcome.result_and_state.state);
    prune_base_fee_vault_changes(db);

    Ok(SpecOutcome {
        state_root: state_merkle_trie_root(db.cache.trie_account()),
        logs_root: log_rlp_hash(result.logs()),
        gas_used: result.tx_gas_used(),
        status: execution_status(&result).to_string(),
        halt_reason: halt_reason(&result),
        halt_kind: match &result {
            ExecutionResult::Halt { reason, .. } => Some(halt_kind(reason)),
            _ => None,
        },
        output: result.output().cloned(),
        compute_gas_used,
        data_size,
        kv_updates,
        state_growth,
        compute_gas_destroyed,
        compute_gas_enforced,
        rescued_gas,
        detained_limit,
        volatile_access,
        frames,
    })
}

/// Runs the differential comparison over every transaction vector of every unit of one fixture
/// file.
///
/// A unit that panics is recorded as [`DiffClass::Panic`] and the rest of the file still runs;
/// see [`panic_capture`] for why that matters at corpus scale.
pub fn diff_test_suite(
    path: &Path,
    specs: DiffSpecs,
    collect_evidence: bool,
) -> Result<Vec<UnitDiff>, TestError> {
    let path_str = path.to_string_lossy().into_owned();
    if skip_test(path) {
        return Ok(vec![]);
    }

    let fixture_err = |msg: String| TestError {
        name: "diff".to_string(),
        path: path_str.clone(),
        kind: TestErrorKind::FixtureError(msg),
    };
    let s = std::fs::read_to_string(path).map_err(|e| fixture_err(format!("read: {e}")))?;
    let suite: TestSuite = serde_json::from_str(&s).map_err(|e| TestError {
        name: "Unknown".to_string(),
        path: path_str.clone(),
        kind: e.into(),
    })?;

    let mut diffs = Vec::with_capacity(suite.0.len());
    for (name, unit) in suite.0 {
        // One verdict per vector the unit declares: `post` entries at different `indexes` are
        // different transactions over the same pre-state, and judging only index `{0,0,0}` would
        // report a green unit while never running the rest.
        let vectors = unit.vectors();
        let multi = vectors.len() > 1;
        for indexes in vectors {
            let outcome =
                match panic_capture::catch(|| diff_unit(&unit, indexes, specs, collect_evidence)) {
                    Ok(outcome) => outcome,
                    Err(report) => UnitDiffOutcome {
                        class: DiffClass::Panic,
                        fields: vec![],
                        mechanisms: vec![],
                        detail: Some(report),
                    },
                };
            let name = if multi { vector_label(&name, indexes) } else { name.clone() };
            diffs.push(outcome.named(name, path_str.clone()));
        }
    }
    Ok(diffs)
}

/// Counts of every verdict and mechanism seen over a corpus.
#[derive(Debug, Clone, Default)]
pub struct DiffTally {
    /// Units per [`DiffClass`], keyed by [`DiffClass::label`].
    pub classes: BTreeMap<&'static str, usize>,
    /// Units per [`Mechanism`] over the explained differences, keyed by [`Mechanism::label`].
    pub mechanisms: BTreeMap<&'static str, usize>,
    /// Units per set of disagreeing quantities over the explained differences, keyed by the
    /// comma-joined [`DiffField::label`]s.
    ///
    /// The shape of an explained difference is what a reviewer reads to see whether the corpus is
    /// exercising the deviations the spec describes — one entry per distinct shape, rather than
    /// one line per unit, which at corpus scale is tens of thousands of identical lines.
    pub explained_fields: BTreeMap<String, usize>,
    /// Every unit that needs a human: an unexplained difference or a panic.
    pub flagged: Vec<UnitDiff>,
    /// Files the runner could not read or parse at all, as rendered errors.
    pub file_errors: Vec<String>,
    /// Files validation skips by filename, and which the sweep therefore judged no unit of.
    ///
    /// Counted rather than ignored: it is the difference between the number of units this sweep
    /// reports and the number a driver that splits the corpus into one file per unit would, and
    /// leaving it implicit turns every comparison against such a run into a manual subtraction.
    pub skipped_files: usize,
}

impl DiffTally {
    /// Number of units in a class.
    pub fn count(&self, class: DiffClass) -> usize {
        self.classes.get(class.label()).copied().unwrap_or(0)
    }

    /// Total number of units judged.
    pub fn total(&self) -> usize {
        self.classes.values().sum()
    }

    /// Whether the run should fail its gate: a panic, an unexplained difference, a file the sweep
    /// could not read, or a run that judged nothing at all.
    ///
    /// The last one is what makes the other three mean something. A sweep whose corpus never
    /// arrived, or whose discovery walked into an unreadable directory, reaches the gate with an
    /// empty tally — zero panics, zero unexplained differences — and every count it prints is
    /// truthful. Reading that as a pass is how a broken corpus becomes a green nightly.
    pub fn is_failure(&self) -> bool {
        self.total() == 0 ||
            self.count(DiffClass::Panic) > 0 ||
            self.count(DiffClass::Unexplained) > 0 ||
            !self.file_errors.is_empty()
    }

    /// Records one unit's verdict.
    pub fn record(&mut self, diff: UnitDiff) {
        *self.classes.entry(diff.class.label()).or_insert(0) += 1;
        if diff.class == DiffClass::Explained {
            for m in &diff.mechanisms {
                *self.mechanisms.entry(m.label()).or_insert(0) += 1;
            }
            let shape = diff.fields.iter().map(|f| f.label()).collect::<Vec<_>>().join(",");
            *self.explained_fields.entry(shape).or_insert(0) += 1;
        }
        if matches!(diff.class, DiffClass::Unexplained | DiffClass::Panic) {
            self.flagged.push(diff);
        }
    }

    /// Merges another tally into this one.
    pub fn merge(&mut self, other: Self) {
        for (k, v) in other.classes {
            *self.classes.entry(k).or_insert(0) += v;
        }
        for (k, v) in other.mechanisms {
            *self.mechanisms.entry(k).or_insert(0) += v;
        }
        for (k, v) in other.explained_fields {
            *self.explained_fields.entry(k).or_insert(0) += v;
        }
        self.flagged.extend(other.flagged);
        self.file_errors.extend(other.file_errors);
        self.skipped_files += other.skipped_files;
    }
}

/// How a corpus-wide differential run behaves.
#[derive(Debug, Clone, Copy)]
pub struct DiffRunConfig {
    /// The specs to compare.
    pub specs: DiffSpecs,
    /// Run every file on one thread.
    pub single_thread: bool,
    /// Re-run an otherwise unexplained difference with the frame inspector.
    pub collect_evidence: bool,
    /// Draw a progress bar.
    pub progress: bool,
}

/// Runs the differential comparison over every fixture file, in parallel.
///
/// Installs the panic capture hook: a `debug_assert!` one fixture trips becomes that fixture's
/// verdict instead of taking down a worker thread, which is what makes a single-process
/// full-corpus sweep possible.
///
/// `scan.errors` — anything the discovery walk could not read — is seeded into the tally's file
/// errors before a single fixture runs, so a corpus the sweep only partly reached fails the gate
/// however well the part it did reach behaves.
pub fn run_diff(scan: FixtureScan, config: DiffRunConfig) -> DiffTally {
    panic_capture::install_capture_hook();

    let FixtureScan { files, errors } = scan;
    let n_files = files.len();
    let bar = Arc::new(ProgressBar::with_draw_target(
        Some(n_files as u64),
        if config.progress { ProgressDrawTarget::stdout() } else { ProgressDrawTarget::hidden() },
    ));
    let queue = Arc::new(Mutex::new(files));
    let next = Arc::new(AtomicUsize::new(0));
    let threads = if config.single_thread {
        1
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(n_files.max(1)))
    };

    let mut handles = Vec::with_capacity(threads);
    for i in 0..threads {
        let (queue, next, bar) = (queue.clone(), next.clone(), bar.clone());
        handles.push(
            std::thread::Builder::new()
                .name(format!("diff-{i}"))
                .spawn(move || {
                    let mut tally = DiffTally::default();
                    loop {
                        let idx = next.fetch_add(1, Ordering::SeqCst);
                        let Some(path) = queue.lock().unwrap().get(idx).cloned() else {
                            return tally;
                        };
                        if crate::runner::is_skipped_fixture(&path) {
                            tally.skipped_files += 1;
                            bar.inc(1);
                            continue;
                        }
                        match diff_test_suite(&path, config.specs, config.collect_evidence) {
                            Ok(diffs) => {
                                for diff in diffs {
                                    tally.record(diff);
                                }
                            }
                            Err(e) => tally.file_errors.push(e.to_string()),
                        }
                        bar.inc(1);
                    }
                })
                .expect("spawn diff worker"),
        );
    }

    let mut tally = DiffTally { file_errors: errors, ..DiffTally::default() };
    for handle in handles {
        match handle.join() {
            Ok(worker) => tally.merge(worker),
            // A worker thread that unwound past `diff_test_suite` lost the files it had taken;
            // surface that rather than reporting a short tally as a clean run.
            Err(_) => tally
                .file_errors
                .push("a diff worker thread panicked; its files were not judged".to_string()),
        }
    }
    bar.finish_and_clear();
    tally
}

/// Collects every JSON fixture under each path, rejecting a path that does not exist.
///
/// Directories the walk could not read come back in [`FixtureScan::errors`] rather than as a
/// quietly shorter file list; [`run_diff`] carries them into the tally, where they fail the gate.
pub fn collect_fixture_files(paths: &[PathBuf]) -> Result<FixtureScan, TestError> {
    let mut scan = FixtureScan::default();
    for path in paths {
        if !path.exists() {
            return Err(TestError {
                name: "Path validation".to_string(),
                path: path.display().to_string(),
                kind: TestErrorKind::InvalidPath,
            });
        }
        let found = find_all_json_tests(path);
        scan.files.extend(found.files);
        scan.errors.extend(found.errors);
    }
    if scan.files.is_empty() {
        return Err(TestError {
            name: "Path validation".to_string(),
            path: paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
            kind: TestErrorKind::NoJsonFiles,
        });
    }
    Ok(scan)
}

/// Bridges a keep-going fill's per-unit status into the sweep's own vocabulary.
///
/// A fill sweep and a differential sweep count the same corpus in the same three buckets, so they
/// report through one mapping rather than two that can drift.
pub const fn fill_status_class(status: &UnitStatus) -> DiffClass {
    match status {
        UnitStatus::Ok => DiffClass::Pass,
        UnitStatus::Error(_) => DiffClass::Skipped,
        UnitStatus::Panic(_) => DiffClass::Panic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An outcome that agrees with itself on every compared quantity and shows no mechanism.
    fn quiet() -> SpecOutcome {
        SpecOutcome {
            state_root: B256::ZERO,
            logs_root: B256::ZERO,
            gas_used: 21_000,
            status: "success".to_string(),
            halt_reason: None,
            halt_kind: None,
            output: None,
            compute_gas_used: 1_000,
            data_size: 10,
            kv_updates: 1,
            state_growth: 0,
            compute_gas_destroyed: 0,
            compute_gas_enforced: 1_000,
            rescued_gas: 0,
            detained_limit: None,
            volatile_access: 0,
            frames: None,
        }
    }

    /// Every mechanism, with the hypothesis it falsifies and where the observation comes from.
    ///
    /// Exhaustive by construction: `test_mechanism_table_is_exhaustive` fails if a variant is
    /// added without a row here.
    const MECHANISM_TABLE: [(Mechanism, Option<Hypothesis>, Provenance); 9] = [
        (Mechanism::ResourceLimitHalt, Some(Hypothesis::WithinLimits), Provenance::Execution),
        (Mechanism::DetentionHalt, Some(Hypothesis::WithinLimits), Provenance::Execution),
        (Mechanism::GasRescued, Some(Hypothesis::WithinLimits), Provenance::Execution),
        (Mechanism::ExceptionalHalt, Some(Hypothesis::NoExceptionalHalt), Provenance::Execution),
        (Mechanism::DestroyedComputeGas, None, Provenance::Execution),
        (Mechanism::DetentionMarkDiff, None, Provenance::Execution),
        (Mechanism::DetentionInForce, None, Provenance::Execution),
        (Mechanism::LimitRevertPayload, None, Provenance::Payload),
        (Mechanism::VolatileDisabledPayload, None, Provenance::Payload),
    ];

    // Every mechanism maps to exactly the hypothesis it observes. A mechanism silently gaining a
    // hypothesis would let it explain a difference it is not evidence for.
    #[test]
    fn test_mechanism_hypothesis_table() {
        for (mechanism, expected, _) in MECHANISM_TABLE {
            assert_eq!(mechanism.falsifies(), expected, "{}", mechanism.label());
        }
    }

    // The rule that makes the classifier un-gameable by a fixture: an observation read out of
    // revert-payload bytes never licenses anything, because a contract writes those bytes as
    // freely as MegaETH does. A future mechanism that sniffs a payload and claims a hypothesis
    // fails here rather than in a corpus sweep that quietly stops flagging.
    #[test]
    fn test_only_execution_provenance_licenses() {
        for (mechanism, _, provenance) in MECHANISM_TABLE {
            assert_eq!(mechanism.provenance(), provenance, "{}", mechanism.label());
            if provenance == Provenance::Payload {
                assert_eq!(
                    mechanism.falsifies(),
                    None,
                    "{} is read off fixture-authored bytes and must license nothing",
                    mechanism.label()
                );
            }
        }
    }

    // The table above is the test's own claim to completeness, so it has to cover every variant.
    // Labels are distinct and stable, which is what makes them usable as tally keys.
    #[test]
    fn test_mechanism_table_is_exhaustive() {
        let mut labels: Vec<&str> = MECHANISM_TABLE.iter().map(|(m, _, _)| m.label()).collect();
        labels.sort_unstable();
        assert_eq!(
            labels,
            [
                "destroyed_compute_gas",
                "detention_halt",
                "detention_in_force",
                "detention_mark_diff",
                "exceptional_halt",
                "gas_rescued",
                "limit_revert_payload",
                "resource_limit_halt",
                "volatile_disabled_payload",
            ],
            "every Mechanism variant needs a row in MECHANISM_TABLE, with a distinct label"
        );
    }

    /// A quantity to disturb, and how to disturb it.
    type FieldProbe = (DiffField, fn(&mut SpecOutcome));

    // `compare` covers every quantity the precision invariant names; a field left out of the
    // comparison is a difference the sweep can never see.
    #[test]
    fn test_compare_detects_every_field() {
        let base = quiet();
        let cases: [FieldProbe; 10] = [
            (DiffField::StateRoot, |o| o.state_root = B256::repeat_byte(1)),
            (DiffField::LogsRoot, |o| o.logs_root = B256::repeat_byte(2)),
            (DiffField::GasUsed, |o| o.gas_used += 1),
            (DiffField::Status, |o| o.status = "revert".to_string()),
            (DiffField::HaltReason, |o| o.halt_reason = Some("Base(OutOfGas)".to_string())),
            (DiffField::Output, |o| o.output = Some(Bytes::from_static(b"\x01"))),
            (DiffField::ComputeGasUsed, |o| o.compute_gas_used += 1),
            (DiffField::DataSize, |o| o.data_size += 1),
            (DiffField::KvUpdates, |o| o.kv_updates += 1),
            (DiffField::StateGrowth, |o| o.state_growth += 1),
        ];
        assert!(compare(&base, &base).is_empty(), "an outcome must agree with itself");
        for (field, mutate) in cases {
            let mut target = base.clone();
            mutate(&mut target);
            assert_eq!(compare(&target, &base), vec![field], "{}", field.label());
        }
    }

    /// Frame evidence holding `n` halted frames and no revert payloads.
    fn halted_frames(n: u32) -> Option<FrameEvidence> {
        Some(FrameEvidence { halted: n, limit_revert_payloads: 0, volatile_disabled_payloads: 0 })
    }

    // The exceptional-halt carve-out raises the reported compute total and is explicitly
    // forbidden from moving the receipt or the state, so it licenses one field and not the other.
    #[test]
    fn test_exceptional_halt_explains_only_the_reported_compute_total() {
        let base = quiet();
        let mut target = base.clone();
        target.frames = halted_frames(1);
        target.compute_gas_destroyed = 5_000;
        target.compute_gas_used += 5_000;
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Explained);
        assert!(verdict.mechanisms.contains(&Mechanism::DestroyedComputeGas));

        // Same evidence, a state-root difference: not licensed.
        let mut target = base.clone();
        target.frames = halted_frames(1);
        target.compute_gas_destroyed = 5_000;
        target.state_root = B256::repeat_byte(9);
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Unexplained);
        assert!(
            verdict.detail.as_deref().is_some_and(|d| d.contains("state_root")),
            "detail should name the unlicensed field: {:?}",
            verdict.detail
        );
    }

    // A destroyed remainder is derived from a conservation law over the envelope, not observed.
    // A defect in that law shows up as a non-zero remainder with no halt behind it, and if the
    // remainder licensed the compute-total difference it causes, that defect would be exactly the
    // shape the sweep stops reporting. The halt it claims must come from the frame the EVM
    // finished; here it is booked with no frame that halted, and the difference stays a finding.
    #[test]
    fn test_destroyed_compute_gas_needs_an_independent_halted_frame() {
        let base = quiet();
        let mut target = base.clone();
        target.compute_gas_destroyed = 5_000;
        target.compute_gas_used += 5_000;

        // No frame pass at all: the remainder is the only thing on the table.
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Unexplained, "{verdict:?}");
        assert!(verdict.mechanisms.contains(&Mechanism::DestroyedComputeGas));

        // The frame pass ran and found no halted frame: the remainder is still unexplained, and
        // now it is a live contradiction — something destroyed an envelope that no frame lost.
        target.frames = halted_frames(0);
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Unexplained, "{verdict:?}");

        // With the independent witness, the same difference is licensed.
        target.frames = halted_frames(1);
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Explained, "{verdict:?}");
        assert!(verdict.mechanisms.contains(&Mechanism::ExceptionalHalt));
    }

    // A crossed resource limit changes which opcodes ran, so it licenses any quantity —
    // including the consensus-visible ones.
    #[test]
    fn test_resource_limit_evidence_explains_a_consensus_difference() {
        let base = quiet();
        let mut target = base.clone();
        target.status = "halt".to_string();
        target.halt_reason = Some("ComputeGasLimitExceeded { limit: 1, actual: 2 }".to_string());
        target.halt_kind = Some(HaltKind::ResourceLimit);
        target.state_root = B256::repeat_byte(9);
        target.gas_used += 5;
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Explained);
        assert!(verdict.mechanisms.contains(&Mechanism::ResourceLimitHalt));
    }

    // The evidence may sit on the *base* side: Rex7 relaxes enforcement on a failing precompile,
    // so the frozen spec is the one that halts and the unstable one that survives.
    #[test]
    fn test_evidence_on_the_base_side_explains_the_difference() {
        let target = quiet();
        let mut base = target.clone();
        base.status = "halt".to_string();
        base.halt_reason = Some("ComputeGasLimitExceeded { limit: 1, actual: 2 }".to_string());
        base.halt_kind = Some(HaltKind::ResourceLimit);
        base.state_root = B256::repeat_byte(9);
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Explained);
    }

    // Detention labels describe the setting, not a crossing: a cap nobody reached, or a mark that
    // moved without changing an outcome, must not license anything.
    #[test]
    fn test_detention_labels_alone_do_not_explain() {
        let base = quiet();
        let mut target = base.clone();
        target.detained_limit = Some(100_000);
        target.volatile_access = 0b100;
        target.compute_gas_used += 1;
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Unexplained);
        assert!(verdict.mechanisms.contains(&Mechanism::DetentionInForce));
        assert!(verdict.mechanisms.contains(&Mechanism::DetentionMarkDiff));
    }

    // Both revert-payload claims are reported and neither licenses. A frame that reverts with
    // MegaETH's selectors is indistinguishable from a contract that wrote the same four bytes, so
    // treating the bytes as evidence would let any fixture buy an exemption for any difference —
    // and `MegaLimitExceeded` in particular claims the hypothesis that licenses *every* quantity.
    #[test]
    fn test_revert_payload_claims_never_license() {
        let base = quiet();
        for (payload, expected) in [
            (
                FrameEvidence {
                    halted: 0,
                    limit_revert_payloads: 1,
                    volatile_disabled_payloads: 0,
                },
                Mechanism::LimitRevertPayload,
            ),
            (
                FrameEvidence {
                    halted: 0,
                    limit_revert_payloads: 0,
                    volatile_disabled_payloads: 1,
                },
                Mechanism::VolatileDisabledPayload,
            ),
        ] {
            let mut target = base.clone();
            target.frames = Some(payload);
            target.gas_used += 3;
            let verdict = judge(&compare(&target, &base), &target, &base);
            assert_eq!(verdict.class, DiffClass::Unexplained, "{verdict:?}");
            assert!(
                verdict.mechanisms.contains(&expected),
                "the claim is still reported for a human: {:?}",
                verdict.mechanisms
            );
        }
    }

    // An inner frame that halted is invisible in the transaction's own result and leaves no
    // destroyed remainder when the interpreter zeroed its counter. Frame evidence is the only
    // thing that sees it.
    #[test]
    fn test_frame_evidence_supplies_the_halt_the_result_hides() {
        let base = quiet();
        let mut target = base.clone();
        target.compute_gas_used += 700;
        assert_eq!(
            judge(&compare(&target, &base), &target, &base).class,
            DiffClass::Unexplained,
            "without frame evidence there is nothing to license the difference"
        );

        target.frames = halted_frames(1);
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Explained);
        assert!(verdict.mechanisms.contains(&Mechanism::ExceptionalHalt));
    }

    // Which halts count as a crossed resource limit is read off the typed reason, variant by
    // variant. Every `MegaHaltReason` gets a row: the four metering halts and the detention halt
    // are limits, the inherited EVM's halts and `SystemTxInvalidCallee` are not. The rule this
    // replaced — "not `Base(..)` means resource limit" — put `SystemTxInvalidCallee`, and every
    // variant a later spec adds, on the licensing side by default.
    #[test]
    fn test_halt_kind_covers_every_halt_reason() {
        use mega_evm::{
            revm::{
                context::result::{HaltReason as EthHaltReason, OutOfGasError},
                primitives::Address,
            },
            VolatileDataAccess,
        };
        for (reason, expected) in [
            (MegaHaltReason::DataLimitExceeded { limit: 1, actual: 2 }, HaltKind::ResourceLimit),
            (
                MegaHaltReason::KVUpdateLimitExceeded { limit: 1, actual: 2 },
                HaltKind::ResourceLimit,
            ),
            (
                MegaHaltReason::ComputeGasLimitExceeded { limit: 1, actual: 2 },
                HaltKind::ResourceLimit,
            ),
            (
                MegaHaltReason::StateGrowthLimitExceeded { limit: 1, actual: 2 },
                HaltKind::ResourceLimit,
            ),
            (
                MegaHaltReason::VolatileDataAccessOutOfGas {
                    access_type: VolatileDataAccess::empty(),
                    limit: 1,
                    actual: 2,
                },
                HaltKind::Detention,
            ),
            (MegaHaltReason::from(EthHaltReason::OutOfGas(OutOfGasError::Basic)), HaltKind::Other),
            (MegaHaltReason::SystemTxInvalidCallee { callee: Address::ZERO }, HaltKind::Other),
        ] {
            assert_eq!(halt_kind(&reason), expected, "{reason:?}");
        }
    }

    // A halted transaction is always an exceptional halt; whether it is *also* a crossed resource
    // limit is what the kind decides.
    #[test]
    fn test_halt_kind_drives_the_mechanism() {
        let mut outcome = quiet();
        outcome.status = "halt".to_string();

        outcome.halt_kind = Some(HaltKind::Other);
        let m = outcome.mechanisms();
        assert!(m.contains(&Mechanism::ExceptionalHalt));
        assert!(!m.contains(&Mechanism::ResourceLimitHalt));
        assert!(!m.contains(&Mechanism::DetentionHalt));

        outcome.halt_kind = Some(HaltKind::ResourceLimit);
        assert!(outcome.mechanisms().contains(&Mechanism::ResourceLimitHalt));

        outcome.halt_kind = Some(HaltKind::Detention);
        let m = outcome.mechanisms();
        assert!(m.contains(&Mechanism::DetentionHalt));
        assert!(!m.contains(&Mechanism::ResourceLimitHalt));
    }

    // Rescued gas is the tell for a limit exceed whose halt the outer accounting rewrote.
    #[test]
    fn test_rescued_gas_is_limit_evidence() {
        let mut outcome = quiet();
        outcome.rescued_gas = 1;
        assert!(outcome.mechanisms().contains(&Mechanism::GasRescued));
    }

    fn diff_of(class: DiffClass, fields: Vec<DiffField>, mechanisms: Vec<Mechanism>) -> UnitDiff {
        UnitDiff {
            name: "u".to_string(),
            path: "p".to_string(),
            class,
            fields,
            mechanisms,
            detail: None,
        }
    }

    // The tally counts every class, keeps only what a human must look at, and fails the gate on
    // exactly the two classes the sweep exists to catch.
    #[test]
    fn test_tally_accounting_and_gate() {
        let mut tally = DiffTally::default();
        tally.record(diff_of(DiffClass::Pass, vec![], vec![]));
        tally.record(diff_of(DiffClass::Skipped, vec![], vec![]));
        tally.record(diff_of(
            DiffClass::Explained,
            vec![DiffField::ComputeGasUsed],
            vec![Mechanism::ExceptionalHalt],
        ));
        assert_eq!(tally.total(), 3);
        assert_eq!(tally.count(DiffClass::Explained), 1);
        assert_eq!(tally.mechanisms.get("exceptional_halt"), Some(&1));
        assert_eq!(tally.explained_fields.get("compute_gas_used"), Some(&1));
        assert!(tally.flagged.is_empty(), "pass/skip/explained need no human");
        assert!(!tally.is_failure());

        let mut other = DiffTally::default();
        other.record(diff_of(DiffClass::Unexplained, vec![DiffField::StateRoot], vec![]));
        tally.merge(other);
        assert_eq!(tally.count(DiffClass::Unexplained), 1);
        assert_eq!(tally.flagged.len(), 1);
        assert!(tally.is_failure());
    }

    // A file the runner could not read at all is a hole in the sweep's coverage, not a pass.
    #[test]
    fn test_file_error_fails_the_gate() {
        let mut tally = DiffTally::default();
        tally.record(diff_of(DiffClass::Pass, vec![], vec![]));
        assert!(!tally.is_failure());
        tally.file_errors.push("unreadable".to_string());
        assert!(tally.is_failure());
    }

    // A sweep that judged nothing has nothing to say, and every count it prints is a truthful
    // zero. Reading that as a pass is how a corpus that never arrived becomes a green nightly.
    #[test]
    fn test_a_run_that_judged_nothing_fails_the_gate() {
        let mut tally = DiffTally::default();
        assert!(tally.is_failure(), "an empty tally is not a pass");

        // Skipped-by-filename files are not units judged, so a corpus of nothing but those is
        // still a run that judged nothing.
        tally.skipped_files = 12;
        assert!(tally.is_failure());

        tally.record(diff_of(DiffClass::Pass, vec![], vec![]));
        assert!(!tally.is_failure());
    }

    // The classifier reads one sentence — Rex7's precision invariant — and that sentence relates
    // exactly one pair of specs. Any other pair would be judged by a licence it was never given.
    //
    // That the constructor is the *only* way in is a property of the crate's boundary, which this
    // module is inside of; it is pinned by the `compile_fail` example on [`DiffSpecs`] and by
    // `tests/diff_mode.rs`, which are compiled as consumers.
    #[test]
    fn test_only_the_rex7_rex6_pair_can_be_constructed() {
        let (target, base) = DiffSpecs::SUPPORTED;
        let specs = DiffSpecs::new(target, base).expect("the supported pair");
        assert_eq!((specs.target(), specs.base()), (target, base));
        for (t, b) in [
            (SpecName::Rex7, SpecName::Equivalence),
            (SpecName::Rex6, SpecName::Rex5),
            (SpecName::Rex6, SpecName::Rex7),
            (SpecName::Rex7, SpecName::Rex7),
        ] {
            let err = DiffSpecs::new(t, b).expect_err("{t:?} vs {b:?} has no invariant");
            assert!(err.contains("Rex7") && err.contains("Rex6"), "name the supported pair: {err}");
        }
    }

    #[test]
    fn test_fill_status_class_mapping() {
        assert_eq!(fill_status_class(&UnitStatus::Ok), DiffClass::Pass);
        assert_eq!(fill_status_class(&UnitStatus::Error(String::new())), DiffClass::Skipped);
        assert_eq!(fill_status_class(&UnitStatus::Panic(String::new())), DiffClass::Panic);
    }
}
