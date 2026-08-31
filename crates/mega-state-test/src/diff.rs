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

use crate::{
    panic_capture,
    runner::{
        configure_max_blobs, execution_status, external_envs_for, find_all_json_tests, halt_reason,
        inject_block_hashes, prune_base_fee_vault_changes, resolve_chain_id,
        set_cfg_spec_and_mainnet_gas_params, skip_test, TestError, TestErrorKind, UnitStatus,
    },
    types::{tx_env_at, SpecName, TestSuite, TestUnit, TxPartIndices},
    utils::{log_rlp_hash, state_merkle_trie_root},
};
use indicatif::{ProgressBar, ProgressDrawTarget};
use mega_evm::{
    alloy_sol_types::SolError,
    revm::{
        context::cfg::CfgEnv,
        database,
        database_interface::DatabaseCommit,
        handler::FrameResult,
        inspector::Inspector,
        interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInput},
        primitives::{Bytes, B256},
    },
    MegaContext, MegaEvm, MegaLimitExceeded, MegaTransaction, MegaTransactionNew as _,
    VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
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
    /// A frame reverted with the frame-local `MegaLimitExceeded` payload.
    FrameLocalLimitRevert,
    /// A frame ended in an exceptional halt.
    ///
    /// Counted at the frame the EVM finished, so it covers an inner frame the caller absorbed, a
    /// precompile that failed, and a call or creation refused before a frame opened — none of
    /// which the transaction's own result shows.
    ExceptionalHalt,
    /// Rex7 booked a destroyed compute-gas remainder: an envelope was lost without being
    /// executed.
    DestroyedComputeGas,
    /// A `disableVolatileDataAccess` guard rejected an opcode.
    VolatileAccessDisabled,
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
    pub const fn falsifies(self) -> Option<Hypothesis> {
        match self {
            Self::ResourceLimitHalt |
            Self::DetentionHalt |
            Self::GasRescued |
            Self::FrameLocalLimitRevert => Some(Hypothesis::WithinLimits),
            Self::ExceptionalHalt | Self::DestroyedComputeGas => {
                Some(Hypothesis::NoExceptionalHalt)
            }
            Self::VolatileAccessDisabled => Some(Hypothesis::NoDisabledVolatileReject),
            Self::DetentionMarkDiff | Self::DetentionInForce => None,
        }
    }

    /// Stable lower-case label, for tallies and reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ResourceLimitHalt => "resource_limit_halt",
            Self::DetentionHalt => "detention_halt",
            Self::GasRescued => "gas_rescued",
            Self::FrameLocalLimitRevert => "frame_local_limit_revert",
            Self::ExceptionalHalt => "exceptional_halt",
            Self::DestroyedComputeGas => "destroyed_compute_gas",
            Self::VolatileAccessDisabled => "volatile_access_disabled",
            Self::DetentionMarkDiff => "detention_mark_diff",
            Self::DetentionInForce => "detention_in_force",
        }
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
    pub halt_reason: Option<String>,
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
        match self.halt_reason.as_deref() {
            Some(r) if r.starts_with("VolatileDataAccessOutOfGas") => {
                found.push(Mechanism::DetentionHalt);
            }
            // Every MegaETH-specific halt is a resource limit; `Base(..)` wraps the inherited
            // EVM's own halts, which are not.
            Some(r) if !r.starts_with("Base") => found.push(Mechanism::ResourceLimitHalt),
            _ => {}
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
            if frames.limit_exceeded_reverts > 0 {
                found.push(Mechanism::FrameLocalLimitRevert);
            }
            if frames.volatile_disabled_reverts > 0 {
                found.push(Mechanism::VolatileAccessDisabled);
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameEvidence {
    /// Frames whose final result was an exceptional halt.
    pub halted: u32,
    /// Frames that reverted with the frame-local `MegaLimitExceeded` payload.
    pub limit_exceeded_reverts: u32,
    /// Frames that reverted with the `VolatileDataAccessDisabled` payload.
    pub volatile_disabled_reverts: u32,
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
        match result.output.get(..4) {
            Some(s) if s == MegaLimitExceeded::SELECTOR => {
                self.evidence.limit_exceeded_reverts += 1;
            }
            Some(s) if s == VOLATILE_DATA_ACCESS_DISABLED_SELECTOR => {
                self.evidence.volatile_disabled_reverts += 1;
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
#[derive(Debug, Clone, Copy)]
pub struct DiffSpecs {
    /// The spec under test, normally the unstable one.
    pub target: SpecName,
    /// The frozen spec the target inherits from.
    pub base: SpecName,
}

/// Runs one unit under both specs and classifies the result.
///
/// `collect_evidence` decides whether the second, inspected pass runs when the cheap evidence does
/// not settle the case; see [`diff_unit`] for why it is staged.
pub fn diff_unit(unit: &TestUnit, specs: DiffSpecs, collect_evidence: bool) -> UnitDiffOutcome {
    let target = execute_unit_outcome(unit, &specs.target, false);
    let base = execute_unit_outcome(unit, &specs.base, false);

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
                    label(specs.target),
                    label(specs.base)
                ),
            )
        }
        (Err(e), Ok(_)) => {
            return UnitDiffOutcome::unexplained(
                vec![],
                vec![],
                format!(
                    "{} rejected the transaction but {} executed it: {e}",
                    label(specs.target),
                    label(specs.base)
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
    // only for the handful of units that reach here, instead of on every unit in the corpus.
    let (Ok(target), Ok(base)) = (
        execute_unit_outcome(unit, &specs.target, true),
        execute_unit_outcome(unit, &specs.base, true),
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
fn compare(target: &SpecOutcome, base: &SpecOutcome) -> Vec<DiffField> {
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
fn judge(fields: &[DiffField], target: &SpecOutcome, base: &SpecOutcome) -> UnitDiffOutcome {
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

/// Executes one unit at transaction index 0 under `spec` and collects its outcome and evidence.
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
    let tx = tx_env_at(unit, TxPartIndices { data: 0, gas: 0, value: 0 })?;

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

/// Runs the differential comparison over every unit of one fixture file.
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
        let outcome = match panic_capture::catch(|| diff_unit(&unit, specs, collect_evidence)) {
            Ok(outcome) => outcome,
            Err(report) => UnitDiffOutcome {
                class: DiffClass::Panic,
                fields: vec![],
                mechanisms: vec![],
                detail: Some(report),
            },
        };
        diffs.push(outcome.named(name, path_str.clone()));
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

    /// Whether the run should fail its gate: a panic or an unexplained difference.
    pub fn is_failure(&self) -> bool {
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
pub fn run_diff(files: Vec<PathBuf>, config: DiffRunConfig) -> DiffTally {
    panic_capture::install_capture_hook();

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

    let mut tally = DiffTally::default();
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
pub fn collect_fixture_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, TestError> {
    let mut files = Vec::new();
    for path in paths {
        if !path.exists() {
            return Err(TestError {
                name: "Path validation".to_string(),
                path: path.display().to_string(),
                kind: TestErrorKind::InvalidPath,
            });
        }
        files.extend(find_all_json_tests(path));
    }
    if files.is_empty() {
        return Err(TestError {
            name: "Path validation".to_string(),
            path: paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
            kind: TestErrorKind::NoJsonFiles,
        });
    }
    Ok(files)
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

    // Every mechanism maps to exactly the hypothesis it observes, and the two detention labels
    // stay informational. A mechanism silently gaining a hypothesis would let it explain a
    // difference it is not evidence for.
    #[test]
    fn test_mechanism_hypothesis_table() {
        use Hypothesis::{NoDisabledVolatileReject, NoExceptionalHalt, WithinLimits};
        for (mechanism, expected) in [
            (Mechanism::ResourceLimitHalt, Some(WithinLimits)),
            (Mechanism::DetentionHalt, Some(WithinLimits)),
            (Mechanism::GasRescued, Some(WithinLimits)),
            (Mechanism::FrameLocalLimitRevert, Some(WithinLimits)),
            (Mechanism::ExceptionalHalt, Some(NoExceptionalHalt)),
            (Mechanism::DestroyedComputeGas, Some(NoExceptionalHalt)),
            (Mechanism::VolatileAccessDisabled, Some(NoDisabledVolatileReject)),
            (Mechanism::DetentionMarkDiff, None),
            (Mechanism::DetentionInForce, None),
        ] {
            assert_eq!(mechanism.falsifies(), expected, "{}", mechanism.label());
        }
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

    // The exceptional-halt carve-out raises the reported compute total and is explicitly
    // forbidden from moving the receipt or the state, so it licenses one field and not the other.
    #[test]
    fn test_exceptional_halt_explains_only_the_reported_compute_total() {
        let base = quiet();
        let mut target = base.clone();
        target.compute_gas_destroyed = 5_000;
        target.compute_gas_used += 5_000;
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Explained);
        assert!(verdict.mechanisms.contains(&Mechanism::DestroyedComputeGas));

        // Same evidence, a state-root difference: not licensed.
        let mut target = base.clone();
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

    // A crossed resource limit changes which opcodes ran, so it licenses any quantity —
    // including the consensus-visible ones.
    #[test]
    fn test_resource_limit_evidence_explains_a_consensus_difference() {
        let base = quiet();
        let mut target = base.clone();
        target.status = "halt".to_string();
        target.halt_reason = Some("ComputeGasLimitExceeded { limit: 1, actual: 2 }".to_string());
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

    // A `disableVolatileDataAccess` rejection charges the opcode's static fee under Rex7 and
    // nothing under Rex6, which can change anything downstream.
    #[test]
    fn test_guard_rejection_explains_a_consensus_difference() {
        let base = quiet();
        let mut target = base.clone();
        target.frames = Some(FrameEvidence {
            halted: 0,
            limit_exceeded_reverts: 0,
            volatile_disabled_reverts: 1,
        });
        target.gas_used += 3;
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Explained);
        assert!(verdict.mechanisms.contains(&Mechanism::VolatileAccessDisabled));
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

        target.frames = Some(FrameEvidence {
            halted: 1,
            limit_exceeded_reverts: 0,
            volatile_disabled_reverts: 0,
        });
        let verdict = judge(&compare(&target, &base), &target, &base);
        assert_eq!(verdict.class, DiffClass::Explained);
        assert!(verdict.mechanisms.contains(&Mechanism::ExceptionalHalt));
    }

    // The halt-reason string decides which mechanism a halt is: the inherited EVM's own halts
    // arrive wrapped in `Base(..)` and are not resource limits.
    #[test]
    fn test_halt_reason_classification() {
        let mut outcome = quiet();
        outcome.status = "halt".to_string();

        outcome.halt_reason = Some("Base(OutOfGas(Basic))".to_string());
        let m = outcome.mechanisms();
        assert!(m.contains(&Mechanism::ExceptionalHalt));
        assert!(!m.contains(&Mechanism::ResourceLimitHalt));

        outcome.halt_reason = Some("ComputeGasLimitExceeded { limit: 1, actual: 2 }".to_string());
        assert!(outcome.mechanisms().contains(&Mechanism::ResourceLimitHalt));

        outcome.halt_reason = Some("VolatileDataAccessOutOfGas { .. }".to_string());
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

    #[test]
    fn test_fill_status_class_mapping() {
        assert_eq!(fill_status_class(&UnitStatus::Ok), DiffClass::Pass);
        assert_eq!(fill_status_class(&UnitStatus::Error(String::new())), DiffClass::Skipped);
        assert_eq!(fill_status_class(&UnitStatus::Panic(String::new())), DiffClass::Panic);
    }
}
