//! A deterministic rewriting inspector, and the corpus sweep that runs it.
//!
//! # What this is for
//!
//! `MegaETH` supports rewriting inspectors in full: the measurement shim books what one does to a
//! transaction's gas, and the conservation law accounts for it. `tests/rex7/shim_measurement.rs`
//! and `tests/rex7/inspector_cheat_matrix.rs` pin that mechanism shape by shape, on fixtures built
//! to reach each shape. What neither can do is put a rewriting inspector on top of *arbitrary*
//! execution — the corner of the state space where a rewrite meets a detained frame, a latched
//! resource exceed, a precompile, a `SELFDESTRUCT`, an EIP-7702 delegation, a nested revert.
//!
//! The EEST corpus is that state space, already written down. This module drives it: every vector
//! is executed three times — with no inspector, with a read-only one, and with a rewriting one —
//! and asks two questions.
//!
//! - **Does anything break?** Every gas-accounting cross-check `MegaETH` has is a `debug_assert`,
//!   so a build with debug assertions live turns a broken conservation law into a panic, which
//!   [`panic_capture`](crate::panic_capture) turns into that vector's verdict rather than a lost
//!   worker thread. Zero panics over the corpus is the gate.
//! - **Is observation still free?** The read-only run must be bit-identical to the run with no
//!   inspector at all, on every quantity the differential classifier compares. That is the property
//!   every tracer in production depends on, and it is checked here against 44,000 transactions
//!   rather than against a handful of fixtures.
//! - **Can the guard still see it?** A rewriting run that applied a mutation the shim is contracted
//!   to book unconditionally must not end with an all-zero ledger. The canonical block path admits
//!   a transaction whose ledger is zero, so a rewrite that leaves it zero is a rewrite that reaches
//!   a block — see [`ChaosClass::LedgerBlind`] and [`ChaosShape::is_always_booked`] for why the
//!   gate is stated over a subset of the pool rather than over all of it.
//!
//! # Why the randomness is not random
//!
//! A sweep whose failures cannot be reproduced is a sweep whose failures cannot be fixed. Every
//! decision the chaos inspector makes comes from a hash of two things: a global seed the caller
//! chooses, and the vector's own identity (its fixture path, its unit name, its transaction
//! indexes). No clock, no address, no iteration order, no thread id. The same seed and the same
//! corpus produce the same mutations on any machine, in any thread count, in any order — so a
//! flagged vector comes with everything needed to re-run exactly it.
//!
//! # What the pool leaves out, and the one refusal it draws on purpose
//!
//! One rewrite shape is missing: turning a *failed contract creation* into a successful one. The
//! shim refuses that shape and asserts on it, deliberately — by the time `create_end` runs, the
//! journal has been reverted and no code was deposited, so a success there reports a deployment
//! that did not happen, and a corpus that produces it should stop rather than quietly take the
//! rejection path. Including it here would make the detector's own firing the sweep's dominant
//! result. The refusal is pinned end-to-end by the two tests named in
//! `tests/rex7/inspector_cheat_matrix.rs`'s `inapplicable` table instead.
//!
//! The other refused shape *is* in the pool: [`ChaosShape::MoveInitResultClass`], which moves the
//! classification of a result frame init produced. It is drawn rather than withheld because the
//! shim answers it by declining the transaction rather than by asserting, and a decline is
//! something a sweep can count — [`ChaosClass::Refused`] is that count. So the corpus exercises
//! the refusal at scale instead of leaving it to the fixtures that reach it on purpose, and the
//! number says how much of the corpus its draws actually land on. The two general shapes
//! `FailFrame` and `ReviveCall` reach the same refusal whenever they happen to land on an
//! init-produced result, and are counted the same way.
//!
//! The four interception shapes do not, and that is a boundary rather than an omission: a result
//! an inspector answered a frame with is the inspector's in whole, with no checkpoint opened and
//! no state written behind it, so rewriting its classification contradicts nothing and is
//! supported. A draw that intercepts a frame and then reclassifies its own answer therefore
//! executes, which is what keeps the two halves of the pool composable.

use crate::{
    diff::{compare, execute_unit_in_mode, execute_unit_reporting_chaos, RunMode},
    panic_capture,
    runner::{is_skipped_fixture, skip_test, vector_label, FixtureScan, TestError, TestErrorKind},
    types::{SpecName, TestSuite, TestUnit, TxPartIndices},
};
use indicatif::{ProgressBar, ProgressDrawTarget};
use mega_evm::{
    revm::{
        context::{Cfg, ContextTr, JournalTr},
        handler::FrameResult,
        inspector::Inspector,
        interpreter::{
            interpreter_types::{Jumps, LoopControl, MemoryTr, ReturnData, StackTr},
            CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas,
            InstructionResult, Interpreter, InterpreterAction, InterpreterResult, InterpreterTypes,
        },
        primitives::{Address, Bytes, Log, U256},
    },
    FrameResultOriginTr, FORBIDDEN_CREATE_REVIVAL, FORBIDDEN_FRAME_INIT_REWRITE,
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

/// How many mutations one transaction may receive.
///
/// Bounded for two reasons. A budget keeps the sweep's running time bounded — every gas injection
/// buys the transaction more opcodes to execute, and an unbounded trickle into a loop is an
/// unbounded sweep. And a budget keeps a flagged vector legible: a dozen mutations can be listed
/// in a report, a hundred thousand cannot.
const MUTATION_BUDGET: u32 = 12;

/// One in this many callbacks carries a mutation, until the budget runs out.
const FIRE_IN: u64 = 8;

/// The largest gas amount a single mutation moves.
///
/// Small on purpose: the whole budget can move at most `MUTATION_BUDGET * GAS_DELTA_MAX` gas,
/// which is far less than a fixture's gas limit. The shapes are being tested, not the magnitudes —
/// a lane that drops an adjustment drops it whatever its size.
const GAS_DELTA_MAX: u64 = 512;

/// Transient-storage slot the journal-write shape writes to.
const CHAOS_SLOT: u64 = 0xC4A05;

/// Account the journal-write shape writes that slot on.
///
/// An address no fixture uses, so the write cannot collide with one the transaction makes and be
/// mistaken for it. Transient storage is discarded at the end of the transaction either way, so
/// the write reaches no post-state — the point is that it goes through the journal, which is the
/// surface an inspector can reach without any `MegaETH` lane metering it.
const CHAOS_ADDRESS: Address =
    mega_evm::revm::primitives::address!("00000000000000000000000000000000c4a05c4a");

// --- the deterministic stream -----------------------------------------------------------------

/// `splitmix64`: a full-period, well-distributed mixing function with no state but its input.
///
/// Written out rather than taken from a crate so that the stream is fixed by this file: a
/// dependency bump that changed a generator's algorithm would silently change what every seed
/// means, and a seed that no longer reproduces its own failure is worse than no seed at all.
const fn mix(seed: u64) -> u64 {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// FNV-1a over bytes — the hash a vector's identity is folded through.
///
/// Also written out rather than taken from the standard library: `DefaultHasher`'s output is
/// explicitly not guaranteed stable across Rust releases, and a seed whose meaning depends on the
/// toolchain does not reproduce anything.
fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// The seed one vector's chaos run uses, derived from the global seed and the vector's identity.
///
/// The identity is everything that distinguishes this transaction from every other in the corpus:
/// which file it came from, which unit of that file, and which of that unit's transaction vectors.
/// Two runs of the same corpus with the same global seed therefore mutate the same vectors the
/// same way, whatever order the files are swept in and however many threads sweep them.
pub fn vector_seed(global: u64, path: &str, name: &str, indexes: TxPartIndices) -> u64 {
    let mut hash = fnv1a(path.as_bytes(), 0xCBF2_9CE4_8422_2325);
    hash = fnv1a(&[0], hash);
    hash = fnv1a(name.as_bytes(), hash);
    hash = fnv1a(&[0], hash);
    hash = fnv1a(&(indexes.data as u64).to_le_bytes(), hash);
    hash = fnv1a(&(indexes.gas as u64).to_le_bytes(), hash);
    hash = fnv1a(&(indexes.value as u64).to_le_bytes(), hash);
    mix(hash ^ mix(global))
}

// --- the shapes ---------------------------------------------------------------------------------

/// A rewrite shape the chaos pool draws from — one legal column of the cheat-shape matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChaosShape {
    /// Gas written into a live interpreter's counter.
    InjectGas,
    /// Gas taken out of one.
    DrainGas,
    /// The interpreter's own working state — a memory word, or the operand an `SSTORE` is about
    /// to consume.
    EditFrameState,
    /// A transient-storage write made behind the EVM's back.
    JournalWrite,
    /// A raised `gas_limit` on a frame about to be built.
    RaiseEnvelope,
    /// A lowered one.
    LowerEnvelope,
    /// A call turned static, so what the frame is allowed to do changes rather than what it costs.
    MakeStatic,
    /// A synthetic outcome, so no frame is built at all. Its gas echoes the envelope the callback
    /// was handed, which is what every tool that intercepts does.
    Intercept,
    /// The same, sized above the envelope, so the outcome hands the caller back gas the
    /// transaction never funded.
    InterceptOverGas,
    /// Sized below it, so the caller spends the difference on a frame that never ran.
    InterceptUnderGas,
    /// Sized at nothing, the extreme of the same direction: the whole envelope is consumed.
    InterceptNoGas,
    /// A raised remaining-gas figure on a finished frame's result.
    RaiseResultGas,
    /// A lowered one.
    LowerResultGas,
    /// A successful frame result rewritten into a revert or an exceptional halt.
    FailFrame,
    /// A failed *call* frame rewritten into a success. The creation form of this shape is refused
    /// by the shim and is deliberately not in the pool — see the module docs.
    ReviveCall,
    /// Gas written into the action the interpreter is already holding — the object a terminating
    /// or suspending instruction left behind, which carries its own copy of what the frame is
    /// handing on.
    RaiseActionGas,
    /// Gas taken out of one.
    LowerActionGas,
    /// A refund added to a `Gas`'s refund counter — what the sender is billed, which the envelope
    /// the conservation law is stated over does not reach.
    RaiseRefund,
    /// A refund taken out of one. Skipped when the `Gas` has none, rather than driving the counter
    /// negative — a state revm documents as invalid at the end of a transaction.
    LowerRefund,
    /// An EIP-8037 state-gas pool written into a `Gas` or a call's inputs. `MegaETH` runs with the
    /// EIP off and fills no pool, so anything found in one is gas the transaction never funded.
    WriteReservoir,
    /// An EIP-8037 spend counter written into a `Gas`. Structurally zero for the same reason, and
    /// reachable through two different receipt figures depending on how the frame ends.
    WriteStateGas,
    /// The frame's memory grown, together with the memo of how far it has been paid for, so that
    /// the interpreter stays consistent and the next expanding opcode is charged nothing.
    GrowMemoryFree,
    /// A finished outcome's metadata rewritten around the `InterpreterResult` inside it: the range
    /// a call's return data lands in, shrunk to nothing, or the address a creation reports.
    MoveOutcomeMetadata,
    /// Gas injected into an interpreter counter and taken straight back out at the next
    /// live-interpreter callback, so the lane's net is zero and the frame saw a number in between
    /// that the EVM would never have produced.
    CancelGasEdit,
    /// A refund added to one finished frame's result and taken out of the next one's, so the
    /// lane's net is zero and — whenever the two frames end differently — the receipt's is
    /// not.
    CancelRefundEdit,
    /// The program counter stepped past the instruction the frame was about to execute, deleting
    /// it from the frame. The work is never performed, so no counter falls and no lane moves.
    SkipOpcode,
    /// A return buffer put in front of the frame, so its `RETURNDATASIZE` and `RETURNDATACOPY`
    /// read data no call produced.
    RewriteReturnData,
    /// The classification of a result *frame init* produced, moved across the success / revert /
    /// halt boundary. The shim refuses this one, so the run it lands in is declined rather than
    /// executed — which is the verdict [`ChaosClass::Refused`] names.
    MoveInitResultClass,
}

impl ChaosShape {
    /// Every shape, in the order the labels are listed by `--chaos-shapes`.
    pub const ALL: [Self; 28] = [
        Self::InjectGas,
        Self::DrainGas,
        Self::EditFrameState,
        Self::JournalWrite,
        Self::RaiseEnvelope,
        Self::LowerEnvelope,
        Self::MakeStatic,
        Self::Intercept,
        Self::InterceptOverGas,
        Self::InterceptUnderGas,
        Self::InterceptNoGas,
        Self::RaiseResultGas,
        Self::LowerResultGas,
        Self::FailFrame,
        Self::ReviveCall,
        Self::RaiseActionGas,
        Self::LowerActionGas,
        Self::RaiseRefund,
        Self::LowerRefund,
        Self::WriteReservoir,
        Self::WriteStateGas,
        Self::GrowMemoryFree,
        Self::MoveOutcomeMetadata,
        Self::CancelGasEdit,
        Self::CancelRefundEdit,
        Self::SkipOpcode,
        Self::RewriteReturnData,
        Self::MoveInitResultClass,
    ];

    /// The shape a label names.
    ///
    /// # Errors
    ///
    /// Returns a message listing every label when `label` is not one.
    pub fn parse(label: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|shape| shape.label() == label).ok_or_else(|| {
            format!(
                "unknown chaos shape {label:?}; known shapes are {}",
                Self::ALL.map(Self::label).join(", ")
            )
        })
    }

    /// Stable label, for reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::InjectGas => "inject_gas",
            Self::DrainGas => "drain_gas",
            Self::EditFrameState => "edit_frame_state",
            Self::JournalWrite => "journal_write",
            Self::RaiseEnvelope => "raise_envelope",
            Self::LowerEnvelope => "lower_envelope",
            Self::MakeStatic => "make_static",
            Self::Intercept => "intercept",
            Self::InterceptOverGas => "intercept_over_gas",
            Self::InterceptUnderGas => "intercept_under_gas",
            Self::InterceptNoGas => "intercept_no_gas",
            Self::RaiseResultGas => "raise_result_gas",
            Self::LowerResultGas => "lower_result_gas",
            Self::FailFrame => "fail_frame",
            Self::ReviveCall => "revive_call",
            Self::RaiseActionGas => "raise_action_gas",
            Self::LowerActionGas => "lower_action_gas",
            Self::RaiseRefund => "raise_refund",
            Self::LowerRefund => "lower_refund",
            Self::WriteReservoir => "write_reservoir",
            Self::WriteStateGas => "write_state_gas",
            Self::GrowMemoryFree => "grow_memory_free",
            Self::MoveOutcomeMetadata => "move_outcome_metadata",
            Self::CancelGasEdit => "cancel_gas_edit",
            Self::CancelRefundEdit => "cancel_refund_edit",
            Self::SkipOpcode => "skip_opcode",
            Self::RewriteReturnData => "rewrite_return_data",
            Self::MoveInitResultClass => "move_init_result_class",
        }
    }

    /// Whether the shim is contracted to book a mutation of this shape unconditionally.
    ///
    /// Most shapes are booked *when the thing they moved still reaches something*: gas written into
    /// a counter the interpreter is about to stop reading moves nothing, a result's remaining gas
    /// edited on a halting frame is never handed back, an envelope edited by the same callback that
    /// then answers the frame reaches no frame at all. Those are all correct non-bookings, and a
    /// gate stated over them would be wrong.
    ///
    /// The shapes below have no such escape. Each of them either changes an argument the shim holds
    /// — which the rewrite comparison reads on the spot — or moves a lane at a boundary that books
    /// unconditionally. So a run that applied one of them and ended with an all-zero ledger is a
    /// rewrite the canonical block path would admit, which is what
    /// [`ChaosClass::LedgerBlind`] names.
    pub const fn is_always_booked(self) -> bool {
        matches!(
            self,
            // The rewrite comparison sees the argument come back changed.
            Self::MakeStatic |
                Self::Intercept |
                Self::InterceptOverGas |
                Self::InterceptUnderGas |
                Self::InterceptNoGas |
                Self::FailFrame |
                Self::ReviveCall |
                Self::GrowMemoryFree |
                Self::MoveOutcomeMetadata |
                // Refunds are booked at the callback boundary, whatever becomes of the frame —
                // which is why the interpreter-facing arm of `hit_interpreter` withholds one that
                // would land in a counter a terminating action has already displaced. That is the
                // single window in which a refund edit reaches nothing, and the pool skips it
                // rather than leaving these three shapes out of the gate.
                Self::RaiseRefund |
                Self::LowerRefund |
                Self::CancelRefundEdit |
                // Both move a constant-time reading the shim takes off every live interpreter,
                // and both are drawn only in the window where they move it: a skip is withheld
                // unless the frame is still running, and a rewritten return buffer always gets a
                // length the current one does not have.
                Self::SkipOpcode |
                Self::RewriteReturnData |
                // Gas written into a pending action is staged for the point that can say whether
                // it moved the envelope, and the lane's traffic is booked at this boundary rather
                // than at that point — so a staged edit shows up whatever the frame's
                // classification turns out to be. The draw is withheld when the action cannot
                // take the edit, so every applied one moves a number.
                Self::RaiseActionGas |
                Self::LowerActionGas |
                // The rewrite comparison books it before the shim refuses it, and the refusal is
                // counted beside that — so the ledger carries two reasons to be non-zero.
                Self::MoveInitResultClass
        )
    }
}

/// Shapes reachable from a callback that holds a live interpreter.
///
/// `RaiseActionGas` and `LowerActionGas` only land at the one callback that runs with an action
/// already pending — `step_end`, which revm's inspected loop runs after the instruction that set
/// it. A draw for them anywhere else leaves the interpreter alone and spends no budget.
/// `SkipOpcode` is withheld in the opposite window, at a callback whose frame is already
/// terminating, and at an instruction whose deletion would move the frame off an instruction
/// boundary.
const INTERPRETER_SHAPES: [ChaosShape; 14] = [
    ChaosShape::InjectGas,
    ChaosShape::DrainGas,
    ChaosShape::EditFrameState,
    ChaosShape::JournalWrite,
    ChaosShape::RaiseActionGas,
    ChaosShape::LowerActionGas,
    ChaosShape::RaiseRefund,
    ChaosShape::LowerRefund,
    ChaosShape::WriteReservoir,
    ChaosShape::WriteStateGas,
    ChaosShape::GrowMemoryFree,
    ChaosShape::CancelGasEdit,
    ChaosShape::SkipOpcode,
    ChaosShape::RewriteReturnData,
];

/// Shapes reachable from a callback that holds a frame's inputs, before the frame is built.
///
/// The four interception shapes differ only in how the synthetic outcome's `Gas` is sized against
/// the envelope. That is the whole of what separates them, and it is the separation that matters:
/// the echo is the shape every real tool uses, and it is also the one shape whose accounting
/// closes without anything measuring the figure.
const INPUT_SHAPES: [ChaosShape; 9] = [
    ChaosShape::RaiseEnvelope,
    ChaosShape::LowerEnvelope,
    ChaosShape::MakeStatic,
    ChaosShape::Intercept,
    ChaosShape::InterceptOverGas,
    ChaosShape::InterceptUnderGas,
    ChaosShape::InterceptNoGas,
    ChaosShape::JournalWrite,
    ChaosShape::WriteReservoir,
];

/// Shapes reachable from a callback that holds a finished frame's result.
const RESULT_SHAPES: [ChaosShape; 12] = [
    ChaosShape::RaiseResultGas,
    ChaosShape::LowerResultGas,
    ChaosShape::FailFrame,
    ChaosShape::ReviveCall,
    ChaosShape::JournalWrite,
    ChaosShape::RaiseRefund,
    ChaosShape::LowerRefund,
    ChaosShape::WriteReservoir,
    ChaosShape::WriteStateGas,
    ChaosShape::MoveOutcomeMetadata,
    ChaosShape::CancelRefundEdit,
    ChaosShape::MoveInitResultClass,
];

/// Which mutations a chaos run is allowed to make.
///
/// The knob exists for triage rather than for the sweep's normal operation: a flagged vector is
/// re-run with the filter narrowed until the smallest set of shapes that still reproduces it is
/// found, which is the difference between "chaos broke something" and a defect report. See
/// [`ChaosInspector::new`] for what narrowing does and does not preserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeFilter {
    /// Bitmask over [`ChaosShape::ALL`], by index.
    allowed: u32,
}

impl Default for ShapeFilter {
    fn default() -> Self {
        Self { allowed: u32::MAX }
    }
}

impl ShapeFilter {
    /// A filter allowing exactly the listed shapes.
    pub fn only(shapes: &[ChaosShape]) -> Self {
        let mut allowed = 0;
        for shape in shapes {
            allowed |= 1 << Self::index(*shape);
        }
        Self { allowed }
    }

    /// Whether `shape` may be drawn.
    pub const fn allows(&self, shape: ChaosShape) -> bool {
        self.allowed & (1 << Self::index(shape)) != 0
    }

    /// Whether this filter allows every shape.
    pub fn is_complete(&self) -> bool {
        ChaosShape::ALL.into_iter().all(|s| self.allows(s))
    }

    const fn index(shape: ChaosShape) -> u32 {
        shape as u32
    }
}

/// How many mutations of each shape one run applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChaosTally {
    /// Mutations applied, per shape label.
    pub applied: BTreeMap<&'static str, u32>,
    /// Callbacks the inspector was handed.
    pub callbacks: u64,
}

impl ChaosTally {
    /// Total mutations applied.
    pub fn total(&self) -> u32 {
        self.applied.values().sum()
    }

    /// Folds another run's tally into this one.
    pub fn merge(&mut self, other: &Self) {
        for (shape, count) in &other.applied {
            *self.applied.entry(shape).or_insert(0) += count;
        }
        self.callbacks += other.callbacks;
    }
}

// --- the inspector ------------------------------------------------------------------------------

/// A read-only inspector that counts every callback it is handed and changes nothing.
///
/// The control the chaos run is judged against. It implements every callback, on purpose: an
/// inspector that implemented only one would exercise only one of the shim's wrappers, and the
/// claim under test is that *observation* costs nothing, not that one callback does.
#[derive(Debug, Default)]
pub struct CallbackCounter {
    callbacks: u64,
}

impl CallbackCounter {
    /// How many callbacks this inspector was handed.
    pub const fn callbacks(&self) -> u64 {
        self.callbacks
    }
}

/// The control counts callbacks and writes nothing back, which is exactly what the declaration
/// promises — so it is also what [`RunMode::ObserveTrusted`](crate::diff::RunMode::ObserveTrusted)
/// drives the shim's fast path with.
impl mega_evm::TrustedObserver for CallbackCounter {}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CallbackCounter {
    fn initialize_interp(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.callbacks += 1;
    }

    fn step(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.callbacks += 1;
    }

    fn step_end(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.callbacks += 1;
    }

    fn log(&mut self, _context: &mut CTX, _log: Log) {
        self.callbacks += 1;
    }

    fn frame_start(
        &mut self,
        _context: &mut CTX,
        _frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        self.callbacks += 1;
        None
    }

    fn frame_end(
        &mut self,
        _context: &mut CTX,
        _frame_input: &FrameInput,
        _frame_result: &mut FrameResult,
    ) {
        self.callbacks += 1;
    }

    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.callbacks += 1;
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.callbacks += 1;
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.callbacks += 1;
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.callbacks += 1;
    }

    fn selfdestruct(&mut self, _contract: Address, _target: Address, _value: U256) {
        self.callbacks += 1;
    }
}

/// Rewrites what it is handed, deterministically, from a seed.
///
/// At every callback it draws one value from the stream; that value decides whether this callback
/// carries a mutation and, if so, which shape and how large. The stream advances on every callback
/// whether or not a mutation lands, so the decision sequence is a function of the seed and the
/// execution — never of what the inspector chose earlier.
#[derive(Debug)]
pub struct ChaosInspector {
    seed: u64,
    filter: ShapeFilter,
    /// Position in the stream: how many callbacks have been seen.
    tick: u64,
    /// Mutations left in this transaction's budget.
    budget: u32,
    /// Gas a [`ChaosShape::CancelGasEdit`] injected and has not taken back out yet.
    ///
    /// The give-back is taken at the next live-interpreter callback rather than on a second draw,
    /// so the pair closes tightly and usually inside the same frame. Both halves land on the
    /// interpreter lane, and their net is zero — which is the whole point of the shape.
    pending_gas: u64,
    /// A refund a [`ChaosShape::CancelRefundEdit`] added and has not taken back out yet, settled
    /// at the next result-facing callback for the same reason.
    pending_refund: i64,
    tally: ChaosTally,
}

impl ChaosInspector {
    /// A chaos inspector driven by `seed`, restricted to what `filter` allows.
    ///
    /// The decision stream does not depend on the filter: every callback draws the same value
    /// whatever is allowed, so narrowing a filter keeps each remaining mutation exactly where the
    /// full run put it. What narrowing does change is how far the budget reaches — a rejected draw
    /// spends none of it — so a narrowed run can carry mutations further into a transaction than
    /// the full run did. Narrowing therefore reproduces a flagged mutation; it does not reproduce
    /// a flagged run.
    pub fn new(seed: u64, filter: ShapeFilter) -> Self {
        Self {
            seed,
            filter,
            tick: 0,
            budget: MUTATION_BUDGET,
            pending_gas: 0,
            pending_refund: 0,
            tally: ChaosTally::default(),
        }
    }

    /// What this run mutated.
    pub fn tally(&self) -> ChaosTally {
        self.tally.clone()
    }

    /// Draws the next value from the stream, advancing it by one callback.
    fn draw(&mut self) -> u64 {
        self.tick += 1;
        self.tally.callbacks += 1;
        mix(self.seed ^ mix(self.tick))
    }

    /// Picks a shape from `pool` for this callback, or `None` when this callback carries no
    /// mutation or the budget is spent.
    fn pick(&mut self, pool: &[ChaosShape]) -> Option<(ChaosShape, u64)> {
        let draw = self.draw();
        if self.budget == 0 || !draw.is_multiple_of(FIRE_IN) {
            return None;
        }
        let shape = pool[(draw / FIRE_IN) as usize % pool.len()];
        if !self.filter.allows(shape) {
            return None;
        }
        Some((shape, mix(draw)))
    }

    /// Books one applied mutation against the budget.
    fn applied(&mut self, shape: ChaosShape) {
        self.budget = self.budget.saturating_sub(1);
        *self.tally.applied.entry(shape.label()).or_insert(0) += 1;
    }

    /// A gas amount in `1..=GAS_DELTA_MAX`, drawn from `entropy`.
    fn amount(entropy: u64) -> u64 {
        entropy % GAS_DELTA_MAX + 1
    }

    /// Applies an interpreter-facing shape.
    fn hit_interpreter<CTX: ContextTr, INTR: InterpreterTypes>(
        &mut self,
        interp: &mut Interpreter<INTR>,
        context: &mut CTX,
        shape: ChaosShape,
        entropy: u64,
    ) {
        match shape {
            ChaosShape::InjectGas => interp.gas.erase_cost(Self::amount(entropy)),
            ChaosShape::DrainGas => {
                if !interp.gas.record_regular_cost(Self::amount(entropy)) {
                    // The frame cannot afford the removal; leave the counter alone rather than
                    // manufacture an out-of-gas the EVM did not reach.
                    return;
                }
            }
            ChaosShape::EditFrameState => {
                if !edit_frame_state(interp, entropy) {
                    return;
                }
            }
            ChaosShape::JournalWrite => write_journal(context, entropy),
            ChaosShape::RaiseActionGas | ChaosShape::LowerActionGas => {
                let raise = shape == ChaosShape::RaiseActionGas;
                if !edit_pending_action_gas(interp, raise, Self::amount(entropy)) {
                    return;
                }
            }
            ChaosShape::RaiseRefund | ChaosShape::LowerRefund => {
                // A refund written into the interpreter's own counter while a terminating action
                // is pending lands in an object nobody reads again: the action carries its own
                // `Gas`, and that is what becomes the frame's result. The edit would be applied,
                // correctly booked nowhere, and would then look to the ledger gate like a rewrite
                // the shim missed. Leave the counter alone and spend no budget; the same edit
                // reaches the live object at the next callback, and the dead window itself is
                // pinned by `tests/rex7/shim_measurement.rs`.
                if matches!(interp.bytecode.action(), Some(InterpreterAction::Return(_))) {
                    return;
                }
                if !edit_receipt_figure(&mut interp.gas, shape, entropy) {
                    return;
                }
            }
            ChaosShape::WriteReservoir | ChaosShape::WriteStateGas => {
                if !edit_receipt_figure(&mut interp.gas, shape, entropy) {
                    return;
                }
            }
            ChaosShape::GrowMemoryFree => {
                if !grow_memory_free(interp, context) {
                    return;
                }
            }
            ChaosShape::SkipOpcode => {
                if !skip_opcode(interp) {
                    return;
                }
            }
            ChaosShape::RewriteReturnData => rewrite_return_data(interp, entropy),
            ChaosShape::CancelGasEdit => {
                // The give-back is taken at the next live-interpreter callback, by
                // `settle_pending_gas`. A second draw while one is outstanding leaves the
                // interpreter alone and spends no budget.
                if self.pending_gas != 0 {
                    return;
                }
                let amount = Self::amount(entropy);
                interp.gas.erase_cost(amount);
                self.pending_gas = amount;
            }
            _ => return,
        }
        self.applied(shape);
    }

    /// Takes back the gas a [`ChaosShape::CancelGasEdit`] injected, closing the pair.
    ///
    /// Skipped when the frame cannot afford it, rather than manufacturing an out-of-gas the EVM
    /// did not reach — the pair then stays open, its net stays non-zero, and the ledger is
    /// non-zero either way.
    fn settle_pending_gas<INTR: InterpreterTypes>(&mut self, interp: &mut Interpreter<INTR>) {
        if self.pending_gas != 0 && interp.gas.record_regular_cost(self.pending_gas) {
            self.pending_gas = 0;
        }
    }

    /// The refund half of the same mechanism, settled against a finished frame's result.
    fn settle_pending_refund(&mut self, gas: &mut Gas) {
        if self.pending_refund != 0 && gas.refunded() >= self.pending_refund {
            gas.record_refund(-self.pending_refund);
            self.pending_refund = 0;
        }
    }

    /// Applies an input-facing shape to a call's inputs, or intercepts the frame.
    fn hit_call_inputs<CTX: ContextTr>(
        &mut self,
        context: &mut CTX,
        inputs: &mut CallInputs,
        shape: ChaosShape,
        entropy: u64,
    ) -> Option<CallOutcome> {
        let mut outcome = None;
        match shape {
            ChaosShape::RaiseEnvelope => {
                inputs.gas_limit = inputs.gas_limit.saturating_add(Self::amount(entropy));
            }
            ChaosShape::LowerEnvelope => {
                inputs.gas_limit = inputs.gas_limit.saturating_sub(Self::amount(entropy));
            }
            ChaosShape::MakeStatic => {
                // A call already made static by its caller would come back unchanged, which is a
                // mutation nothing has to book; leave it alone and spend no budget.
                if inputs.is_static {
                    return None;
                }
                inputs.is_static = true;
            }
            ChaosShape::WriteReservoir => {
                inputs.reservoir = inputs.reservoir.saturating_add(Self::amount(entropy));
            }
            ChaosShape::Intercept |
            ChaosShape::InterceptOverGas |
            ChaosShape::InterceptUnderGas |
            ChaosShape::InterceptNoGas => {
                outcome = Some(CallOutcome::new(
                    InterpreterResult::new(
                        synthetic_result(entropy),
                        Bytes::new(),
                        Gas::new(interception_gas(shape, inputs.gas_limit, entropy)),
                    ),
                    inputs.return_memory_offset.clone(),
                ));
            }
            ChaosShape::JournalWrite => write_journal(context, entropy),
            _ => return None,
        }
        self.applied(shape);
        outcome
    }

    /// Applies an input-facing shape to a creation's inputs, or intercepts the frame.
    fn hit_create_inputs<CTX: ContextTr>(
        &mut self,
        context: &mut CTX,
        inputs: &mut CreateInputs,
        shape: ChaosShape,
        entropy: u64,
    ) -> Option<CreateOutcome> {
        let mut outcome = None;
        match shape {
            ChaosShape::RaiseEnvelope => {
                inputs.set_gas_limit(inputs.gas_limit().saturating_add(Self::amount(entropy)));
            }
            ChaosShape::LowerEnvelope => {
                inputs.set_gas_limit(inputs.gas_limit().saturating_sub(Self::amount(entropy)));
            }
            ChaosShape::Intercept |
            ChaosShape::InterceptOverGas |
            ChaosShape::InterceptUnderGas |
            ChaosShape::InterceptNoGas => {
                outcome = Some(CreateOutcome::new(
                    InterpreterResult::new(
                        synthetic_result(entropy),
                        Bytes::new(),
                        Gas::new(interception_gas(shape, inputs.gas_limit(), entropy)),
                    ),
                    None,
                ));
            }
            ChaosShape::JournalWrite => write_journal(context, entropy),
            // `MakeStatic` has no counterpart here — a creation carries no static flag — and
            // `WriteReservoir` has none either, because `CreateInputs` keeps its pool private and
            // offers no setter. The rest are not input-facing at all. All of them leave the inputs
            // alone and spend no budget.
            _ => return None,
        }
        self.applied(shape);
        outcome
    }

    /// Applies a result-facing shape to a finished frame's result.
    ///
    /// `is_creation` withholds the one shape the shim refuses with an assertion: a failed contract
    /// creation rewritten into a success. The pool never offers it, so a creation drawing
    /// `ReviveCall` leaves the result alone and spends no budget.
    ///
    /// `is_frame_init` is the other way round — it is what *arms*
    /// [`ChaosShape::MoveInitResultClass`], which is only that shape when the result it lands on
    /// came out of frame init. A draw for it anywhere else leaves the result alone, so the shape's
    /// tally counts refusals reached rather than draws made.
    fn hit_result(
        &mut self,
        result: &mut InterpreterResult,
        is_creation: bool,
        is_frame_init: bool,
        shape: ChaosShape,
        entropy: u64,
    ) {
        match shape {
            ChaosShape::RaiseResultGas => result.gas.erase_cost(Self::amount(entropy)),
            ChaosShape::LowerResultGas => {
                if !result.gas.record_regular_cost(Self::amount(entropy)) {
                    return;
                }
            }
            ChaosShape::FailFrame => {
                if !result.result.is_ok() {
                    return;
                }
                result.result = if entropy.is_multiple_of(2) {
                    InstructionResult::Revert
                } else {
                    InstructionResult::OutOfGas
                };
            }
            ChaosShape::ReviveCall => {
                if is_creation || result.result.is_ok() {
                    return;
                }
                result.result = InstructionResult::Stop;
            }
            ChaosShape::RaiseRefund |
            ChaosShape::LowerRefund |
            ChaosShape::WriteReservoir |
            ChaosShape::WriteStateGas => {
                if !edit_receipt_figure(&mut result.gas, shape, entropy) {
                    return;
                }
            }
            ChaosShape::CancelRefundEdit => {
                if self.pending_refund != 0 {
                    return;
                }
                let amount = Self::amount(entropy) as i64;
                result.gas.record_refund(amount);
                self.pending_refund = amount;
            }
            ChaosShape::MoveInitResultClass => {
                if !is_frame_init {
                    return;
                }
                result.result = across_the_class_boundary(result.result);
            }
            _ => return,
        }
        self.applied(shape);
    }
}

/// The class a result is moved *to*, which is any class but its own.
///
/// Stated over all three so the shape reaches every arm of frame init, not only the ones that
/// return successfully: an empty-code call and a precompile come back `Stop`, a refusal comes back
/// a halt, and `MegaETH`'s own frame-local exceed comes back a revert.
const fn across_the_class_boundary(from: InstructionResult) -> InstructionResult {
    if from.is_ok() {
        InstructionResult::Revert
    } else if from.is_revert() {
        InstructionResult::OutOfGas
    } else {
        InstructionResult::Revert
    }
}

/// Grows the frame's memory and moves the memo of how far it has been paid for with it, returning
/// whether anything moved.
///
/// The pair is what makes this a rewrite rather than a corruption: moving the memo alone leaves the
/// EVM reading out of bounds, moving the memory alone leaves the growth charged for twice, and
/// moving both leaves the interpreter in a state it could have reached by paying, having paid
/// nothing. The next expanding opcode inside the new bound is then charged nothing at all.
///
/// The memo is priced through revm's own table rather than a restatement of the formula, because
/// `MemoryGas::set_words_num` hands revm's caller a `checked_sub` it unwraps unchecked: a memo
/// higher than the schedule would have written is undefined behaviour at the next expansion, not a
/// wrong number.
fn grow_memory_free<CTX: ContextTr, INTR: InterpreterTypes>(
    interp: &mut Interpreter<INTR>,
    context: &CTX,
) -> bool {
    let words = interp.memory.size() / 32 + 1;
    if !interp.memory.resize(words * 32) {
        return false;
    }
    let cost = context.cfg().gas_params().memory_cost(words);
    interp.gas.memory_mut().set_words_num(words, cost);
    true
}

/// Steps the program counter past the instruction the frame was about to execute, returning
/// whether anything moved.
///
/// This is the shape with the sharpest teeth in the pool — it does not make an instruction cheaper,
/// it deletes one — and it is also the only one that can move the frame off an instruction
/// boundary, so its guard is what keeps the sweep's executions well-formed rather than merely
/// different.
///
/// Three conditions, each load-bearing:
///
/// - **The frame is still running.** With a terminating action already pending, revm's inspected
///   loop breaks without reading the counter again, so a skip there changes nothing about the
///   execution while still costing budget.
/// - **The instruction is not a `PUSH`.** A `PUSH` is the one instruction whose bytes are not all
///   opcodes, so skipping its opcode byte leaves its immediate data to be executed as code and the
///   whole rest of the frame decodes at the wrong offsets. Skipping any other single-byte
///   instruction lands exactly on the next instruction boundary: the frame executes its own
///   bytecode with one instruction removed, which is a well-formed program and a reproducible one.
/// - **The instruction does not end the frame.** Skipping a `STOP` or a `RETURN` would carry
///   execution past the point the frame was going to stop, into whatever follows it. That is
///   bounded — the analysed bytecode is padded with `STOP`, and gas bounds it in any case — but it
///   makes a frame's cost a function of what happens to sit after its terminator, which is the
///   opposite of what a corpus sweep wants.
///
/// Termination is not at risk under those conditions. Deleting an instruction cannot create a
/// backward jump the bytecode did not already contain, every path still ends at a terminator or
/// runs out of gas, and the budget bounds how many instructions one transaction can lose.
fn skip_opcode<INTR: InterpreterTypes>(interp: &mut Interpreter<INTR>) -> bool {
    /// `PUSH1` through `PUSH32` — every opcode that carries immediate bytes.
    const PUSH_RANGE: core::ops::RangeInclusive<u8> = 0x60..=0x7F;
    /// `STOP`, `RETURN`, `REVERT`, `INVALID`, `SELFDESTRUCT`.
    const TERMINATORS: [u8; 5] = [0x00, 0xF3, 0xFD, 0xFE, 0xFF];

    if !interp.bytecode.is_not_end() {
        return false;
    }
    let opcode = interp.bytecode.opcode();
    if PUSH_RANGE.contains(&opcode) || TERMINATORS.contains(&opcode) {
        return false;
    }
    interp.bytecode.relative_jump(1);
    true
}

/// Puts a return buffer in front of the frame that no call of its own produced.
///
/// The length always differs from the one the buffer has, which is what makes the shape
/// unconditionally visible: the shim reads the buffer's identity, and a replacement of a different
/// length moves it whatever the allocator does with the old one. It is also what the frame reads —
/// `RETURNDATASIZE` returns exactly this number.
///
/// Growing rather than shrinking, and by at most a word or so at a time, so that a
/// `RETURNDATACOPY` the fixture makes can only succeed where it would have reverted, and the
/// buffer stays small enough that the copy it pays for is bounded by the mutation budget.
fn rewrite_return_data<INTR: InterpreterTypes>(interp: &mut Interpreter<INTR>, entropy: u64) {
    let len = interp.return_data.buffer().len() + 1 + (entropy % 32) as usize;
    interp.return_data.set_buffer(Bytes::from(vec![(entropy % 256) as u8; len]));
}

/// Writes one of the receipt figures that is not the envelope, returning whether anything moved.
///
/// The three are grouped because they are one surface — every `Gas` an inspector is handed carries
/// all of them — and separated from the gas lanes because the conservation law reaches only one of
/// the three. A refund reaches what the sender is billed; the EIP-8037 pool reaches the envelope
/// the receipt reports as spent; the EIP-8037 spend counter reaches the receipt's state-gas figure,
/// or its caller's pool when the frame fails.
///
/// Lowering a refund the `Gas` does not have is skipped rather than driving the counter negative:
/// revm documents a negative refund at the end of a transaction as invalid, so producing one would
/// be testing a state the EVM cannot reach on its own.
fn edit_receipt_figure(gas: &mut Gas, shape: ChaosShape, entropy: u64) -> bool {
    let amount = entropy % GAS_DELTA_MAX + 1;
    match shape {
        ChaosShape::RaiseRefund => gas.record_refund(amount as i64),
        ChaosShape::LowerRefund => {
            if gas.refunded() < amount as i64 {
                return false;
            }
            gas.record_refund(-(amount as i64));
        }
        ChaosShape::WriteReservoir => gas.set_reservoir(amount),
        ChaosShape::WriteStateGas => gas.set_state_gas_spent(amount as i64),
        _ => return false,
    }
    true
}

/// The gas a synthetic outcome hands back, given the envelope the callback was handed.
///
/// The four interception shapes are exactly this function's four cases. `Intercept` echoes the
/// envelope, which is the convention every tool that intercepts follows and the one sizing whose
/// accounting closes even if nothing measures it; the other three move it, in both directions and
/// down to nothing, so a lane that books one direction and drops the other is caught.
const fn interception_gas(shape: ChaosShape, envelope: u64, entropy: u64) -> u64 {
    match shape {
        ChaosShape::InterceptOverGas => envelope.saturating_add(entropy % GAS_DELTA_MAX + 1),
        ChaosShape::InterceptUnderGas => envelope.saturating_sub(entropy % GAS_DELTA_MAX + 1),
        ChaosShape::InterceptNoGas => 0,
        _ => envelope,
    }
}

/// The classification a synthetic outcome carries — one of the three a real frame can end in.
fn synthetic_result(entropy: u64) -> InstructionResult {
    match entropy % 3 {
        0 => InstructionResult::Stop,
        1 => InstructionResult::Revert,
        _ => InstructionResult::OutOfGas,
    }
}

/// Edits the interpreter's working state, returning whether anything was edited.
///
/// Two edits, chosen by what the frame is doing rather than at random: the operand an `SSTORE` is
/// about to consume, when that is what the interpreter is on, and a memory word otherwise. Neither
/// can fail the frame by itself — a pushed word would be read as the next opcode's operand, which
/// changes the fixture rather than cheating inside it.
fn edit_frame_state<INTR: InterpreterTypes>(interp: &mut Interpreter<INTR>, entropy: u64) -> bool {
    const SSTORE: u8 = 0x55;
    if interp.bytecode.opcode() == SSTORE {
        if let Some([key, value]) = interp.stack.popn::<2>() {
            let pushed = interp.stack.push(value.wrapping_add(U256::from(entropy % 8 + 1))) &&
                interp.stack.push(key);
            return pushed;
        }
        return false;
    }
    if interp.memory.size() >= 32 {
        interp.memory.set(0, &[(entropy % 256) as u8; 32]);
        return true;
    }
    false
}

/// Moves gas in or out of the action the interpreter is holding, returning whether anything moved.
///
/// The pending action is the one gas-carrying object a live-interpreter callback can reach that is
/// not the interpreter's own counter, and the two are different numbers at exactly one moment: a
/// terminating instruction has copied the counter into a `Return` action, or a `CALL` / `CREATE`
/// has put the child's envelope into a `NewFrame` one. With no action pending there is nothing to
/// edit and no budget is spent.
fn edit_pending_action_gas<INTR: InterpreterTypes>(
    interp: &mut Interpreter<INTR>,
    raise: bool,
    amount: u64,
) -> bool {
    match interp.bytecode.action() {
        Some(InterpreterAction::Return(result)) => {
            if raise {
                result.gas.erase_cost(amount);
                true
            } else {
                // The action cannot afford the removal; leave it alone rather than manufacture an
                // out-of-gas the EVM did not reach.
                result.gas.record_regular_cost(amount)
            }
        }
        // Both saturate, so an envelope already at either end does not move. Report that as "not
        // applied" rather than spending the budget on it: the ledger gate below is stated over
        // shapes that always book, and a mutation that moved nothing has nothing to book.
        Some(InterpreterAction::NewFrame(FrameInput::Call(inputs))) => {
            let moved = move_envelope(inputs.gas_limit, raise, amount);
            let applied = moved != inputs.gas_limit;
            inputs.gas_limit = moved;
            applied
        }
        Some(InterpreterAction::NewFrame(FrameInput::Create(inputs))) => {
            let moved = move_envelope(inputs.gas_limit(), raise, amount);
            let applied = moved != inputs.gas_limit();
            inputs.set_gas_limit(moved);
            applied
        }
        _ => false,
    }
}

/// A child envelope moved by `amount`, saturating at both ends.
const fn move_envelope(limit: u64, raise: bool, amount: u64) -> u64 {
    if raise {
        limit.saturating_add(amount)
    } else {
        limit.saturating_sub(amount)
    }
}

/// Writes one transient-storage slot on the frame's own account, behind the EVM's back.
///
/// Transient storage is journalled, so the write follows the frame's checkpoint like any other
/// state change — which is the point: this is the unmetered surface an inspector reaches through,
/// and it must leave the accounting lanes alone without leaving the journal inconsistent.
fn write_journal<CTX: ContextTr>(context: &mut CTX, entropy: u64) {
    context.journal_mut().tstore(CHAOS_ADDRESS, U256::from(CHAOS_SLOT), U256::from(entropy));
}

impl<CTX: ContextTr + FrameResultOriginTr, INTR: InterpreterTypes> Inspector<CTX, INTR>
    for ChaosInspector
{
    fn initialize_interp(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.settle_pending_gas(interp);
        if let Some((shape, entropy)) = self.pick(&INTERPRETER_SHAPES) {
            self.hit_interpreter(interp, context, shape, entropy);
        }
    }

    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.settle_pending_gas(interp);
        if let Some((shape, entropy)) = self.pick(&INTERPRETER_SHAPES) {
            self.hit_interpreter(interp, context, shape, entropy);
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.settle_pending_gas(interp);
        if let Some((shape, entropy)) = self.pick(&INTERPRETER_SHAPES) {
            self.hit_interpreter(interp, context, shape, entropy);
        }
    }

    fn log_full(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX, _log: Log) {
        self.settle_pending_gas(interp);
        if let Some((shape, entropy)) = self.pick(&INTERPRETER_SHAPES) {
            self.hit_interpreter(interp, context, shape, entropy);
        }
    }

    fn frame_start(
        &mut self,
        context: &mut CTX,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        let (shape, entropy) = self.pick(&INPUT_SHAPES)?;
        match frame_input {
            FrameInput::Call(inputs) => {
                self.hit_call_inputs(context, inputs, shape, entropy).map(FrameResult::Call)
            }
            FrameInput::Create(inputs) => {
                self.hit_create_inputs(context, inputs, shape, entropy).map(FrameResult::Create)
            }
            FrameInput::Empty => None,
        }
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let (shape, entropy) = self.pick(&INPUT_SHAPES)?;
        self.hit_call_inputs(context, inputs, shape, entropy)
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let (shape, entropy) = self.pick(&INPUT_SHAPES)?;
        self.hit_create_inputs(context, inputs, shape, entropy)
    }

    fn call_end(&mut self, context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.settle_pending_refund(&mut outcome.result.gas);
        let is_frame_init = context.is_frame_init_result();
        let Some((shape, entropy)) = self.pick(&RESULT_SHAPES) else { return };
        if shape == ChaosShape::MoveOutcomeMetadata {
            if shrink_return_range(outcome) {
                self.applied(shape);
            }
            return;
        }
        self.hit_result(&mut outcome.result, false, is_frame_init, shape, entropy);
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.settle_pending_refund(&mut outcome.result.gas);
        let is_frame_init = context.is_frame_init_result();
        let Some((shape, entropy)) = self.pick(&RESULT_SHAPES) else { return };
        if shape == ChaosShape::MoveOutcomeMetadata {
            if relabel_deployment(outcome) {
                self.applied(shape);
            }
            return;
        }
        self.hit_result(&mut outcome.result, true, is_frame_init, shape, entropy);
    }

    fn frame_end(
        &mut self,
        context: &mut CTX,
        _frame_input: &FrameInput,
        frame_result: &mut FrameResult,
    ) {
        self.settle_pending_refund(frame_result.gas_mut());
        let is_frame_init = context.is_frame_init_result();
        let Some((shape, entropy)) = self.pick(&RESULT_SHAPES) else { return };
        if shape == ChaosShape::MoveOutcomeMetadata {
            let moved = match frame_result {
                FrameResult::Call(outcome) => shrink_return_range(outcome),
                FrameResult::Create(outcome) => relabel_deployment(outcome),
            };
            if moved {
                self.applied(shape);
            }
            return;
        }
        let is_creation = matches!(frame_result, FrameResult::Create(_));
        self.hit_result(
            frame_result.interpreter_result_mut(),
            is_creation,
            is_frame_init,
            shape,
            entropy,
        );
    }
}

/// Shrinks a finished call's return range to nothing, returning whether anything moved.
///
/// Shrunk rather than moved: revm copies the callee's output into the caller's memory at this
/// range and panics if the range is outside what the caller has allocated, so the one edit that is
/// safe on an arbitrary corpus is the one that copies less. The caller then reads whatever was in
/// its memory before the call, which is the same semantic change the moved form makes.
fn shrink_return_range(outcome: &mut CallOutcome) -> bool {
    if outcome.memory_offset.is_empty() {
        return false;
    }
    outcome.memory_offset = outcome.memory_offset.start..outcome.memory_offset.start;
    true
}

/// Reports a successful creation at an address it did not deploy to, returning whether anything
/// moved.
fn relabel_deployment(outcome: &mut CreateOutcome) -> bool {
    if outcome.address.is_none() || outcome.address == Some(CHAOS_ADDRESS) {
        return false;
    }
    outcome.address = Some(CHAOS_ADDRESS);
    true
}

// --- the sweep ------------------------------------------------------------------------------

/// How one vector's three runs came out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChaosClass {
    /// The read-only run was identical to the run with no inspector, and the rewriting run
    /// completed without tripping anything.
    #[default]
    Pass,
    /// The read-only run differed from the run with no inspector. Observation is not free, which
    /// breaks every tracer in production.
    ControlDrift,
    /// The rewriting run and the reference disagreed about whether the transaction executes at
    /// all: one produced a receipt and the other an `EVMError`. No inspector callback runs before
    /// validation, so the two cannot legitimately differ here — with the one exception the verdict
    /// below names.
    ChaosRejected,
    /// The rewriting run was declined because the measurement shim *refused* one of its rewrites.
    ///
    /// The one legitimate way an inspector can stop a transaction that would otherwise execute,
    /// and the designed outcome of two rewrite shapes rather than a defect: moving the
    /// classification of a result frame init produced, and reviving a failed contract creation.
    /// Both leave the caller an answer the state behind it contradicts, so the shim restores the
    /// classification and fails the transaction rather than let a receipt be built on it.
    ///
    /// Counted rather than passed, because the number is worth seeing: it says how much of the
    /// corpus the pool's [`ChaosShape::MoveInitResultClass`] draws actually reach.
    Refused,
    /// The rewriting run applied a mutation the shim is contracted to book unconditionally — see
    /// [`ChaosShape::is_always_booked`] — and still ended with an all-zero ledger.
    ///
    /// That is the one thing the ledger exists to prevent: the canonical block path admits a
    /// transaction whose ledger is zero, so a rewrite that leaves it zero is a rewrite that
    /// reaches a block. Unlike every other verdict here it is not about the transaction's numbers
    /// being wrong — they may be exactly what a rewriting inspector should produce — but about the
    /// guard being unable to tell that anything happened.
    LedgerBlind,
    /// Neither run executed the transaction, and the runner declined it identically.
    Skipped,
    /// A run panicked — which, in a build with debug assertions live, is how a broken conservation
    /// law surfaces.
    Panic,
}

impl ChaosClass {
    /// Stable upper-case label, for tallies and reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::ControlDrift => "CONTROL_DRIFT",
            Self::ChaosRejected => "CHAOS_REJECTED",
            Self::Refused => "REFUSED",
            Self::LedgerBlind => "LEDGER_BLIND",
            Self::Skipped => "SKIPPED",
            Self::Panic => "PANIC",
        }
    }

    /// Whether this verdict fails the gate.
    pub const fn is_failure(self) -> bool {
        matches!(self, Self::ControlDrift | Self::ChaosRejected | Self::LedgerBlind | Self::Panic)
    }
}

/// The verdict on one vector.
#[derive(Debug, Clone)]
pub struct UnitChaos {
    /// The unit's key in the fixture's test-suite map, with the vector's indexes when the unit
    /// declares more than one.
    pub name: String,
    /// The fixture file the unit came from.
    pub path: String,
    /// The seed this vector's rewriting run was driven by — everything needed to re-run exactly
    /// it.
    pub seed: u64,
    /// How the runs came out.
    pub class: ChaosClass,
    /// Mutations the rewriting run applied.
    pub mutations: u32,
    /// What went wrong, for a verdict that needs a human.
    pub detail: Option<String>,
}

/// Runs one vector three times — no inspector, a read-only one, a rewriting one — and judges.
///
/// The reference and the control settle the "observation is free" half. The rewriting run is
/// judged by what it does *not* do: it must not panic (every gas-accounting cross-check is a debug
/// assertion, so a broken law is a panic) and it must not change whether the transaction executes
/// at all.
///
/// Nothing compares the rewriting run's *numbers* to the reference's. A rewriting inspector is
/// supposed to change them — that is what "supported" means — and the property that they still add
/// up is stated by the conservation law, which the execution checks itself.
pub fn chaos_unit(
    unit: &TestUnit,
    indexes: TxPartIndices,
    spec: &SpecName,
    seed: u64,
    filter: ShapeFilter,
) -> ChaosVerdict {
    let reference = execute_unit_in_mode(unit, indexes, spec, RunMode::Plain);
    let control = execute_unit_in_mode(unit, indexes, spec, RunMode::Observe);

    match (&reference, &control) {
        (Ok(reference), Ok(control)) => {
            let fields = compare(&control.outcome, &reference.outcome);
            if !fields.is_empty() {
                return ChaosVerdict::failed(
                    ChaosClass::ControlDrift,
                    format!(
                        "an observation-only inspector moved: {}",
                        fields.iter().map(|f| f.label()).collect::<Vec<_>>().join(", ")
                    ),
                );
            }
            if !control.ledger.is_zero() {
                return ChaosVerdict::failed(
                    ChaosClass::ControlDrift,
                    format!(
                        "an observation-only inspector booked a ledger entry: {:?}",
                        control.ledger
                    ),
                );
            }
        }
        (Err(reference), Err(control)) => {
            let (reference, control) = (reference.to_string(), control.to_string());
            if reference != control {
                return ChaosVerdict::failed(
                    ChaosClass::ControlDrift,
                    format!("the runs were declined differently: {reference} != {control}"),
                );
            }
        }
        (Ok(_), Err(e)) | (Err(e), Ok(_)) => {
            return ChaosVerdict::failed(
                ChaosClass::ControlDrift,
                format!("only one of the two read-only runs executed: {e}"),
            )
        }
    }

    let mut applied = ChaosTally::default();
    let chaos = execute_unit_reporting_chaos(
        unit,
        indexes,
        spec,
        RunMode::Chaos { seed, filter },
        &mut applied,
    );

    let (class, detail) = match (reference.is_ok(), &chaos) {
        (true, Ok(run)) => match blind_shapes(&applied, run.ledger.is_zero()) {
            None => (ChaosClass::Pass, None),
            Some(shapes) => (
                ChaosClass::LedgerBlind,
                Some(format!(
                    "the run applied {shapes} and the ledger is still all-zero, so the canonical \
                     block path would admit it"
                )),
            ),
        },
        // The runner declined this vector before execution — an intrinsic-gas overrun, an
        // unsupported transaction shape — and declined it the same way with the inspector
        // attached. Nothing executed, so nothing was tested; counted rather than passed.
        (false, Err(_)) => (ChaosClass::Skipped, None),
        // A decline the shim itself produced is the designed outcome of a refused rewrite, not a
        // disagreement about whether the transaction executes. It is told apart by the reason the
        // error carries, which is the shim's own message.
        (true, Err(e)) if is_refusal(e) => (ChaosClass::Refused, Some(e.to_string())),
        (true, Err(e)) => (
            ChaosClass::ChaosRejected,
            Some(format!("the rewriting run was declined where the reference executed: {e}")),
        ),
        (false, Ok(_)) => (
            ChaosClass::ChaosRejected,
            Some("the rewriting run executed where the reference was declined".to_string()),
        ),
    };
    ChaosVerdict { class, applied, detail }
}

/// Whether a decline is one the measurement shim produced by refusing a rewrite.
///
/// Read off the reason the error carries, against the shim's own message constants, so a message
/// that changes changes here too rather than silently reclassifying a whole corpus.
fn is_refusal(error: &TestErrorKind) -> bool {
    let rendered = error.to_string();
    rendered.contains(FORBIDDEN_FRAME_INIT_REWRITE) || rendered.contains(FORBIDDEN_CREATE_REVIVAL)
}

/// The shapes a run applied that the shim must have booked, when its ledger says it booked
/// nothing at all.
///
/// `None` when the ledger is non-zero, or when every shape the run applied is one whose booking is
/// conditional on something the run may not have reached — gas written into a counter the
/// interpreter is about to stop reading, a result's gas edited on a frame that hands nothing back,
/// a pool a later frame overwrote. Those non-bookings are correct, and a gate stated over them
/// would fail on a working shim.
fn blind_shapes(applied: &ChaosTally, ledger_is_zero: bool) -> Option<String> {
    if !ledger_is_zero {
        return None;
    }
    let blind: Vec<&str> = applied
        .applied
        .keys()
        .copied()
        .filter(|label| ChaosShape::parse(label).is_ok_and(ChaosShape::is_always_booked))
        .collect();
    (!blind.is_empty()).then(|| blind.join(", "))
}

/// What one vector's three runs produced.
#[derive(Debug, Clone, Default)]
pub struct ChaosVerdict {
    /// How the runs came out.
    pub class: ChaosClass,
    /// What the rewriting run mutated.
    pub applied: ChaosTally,
    /// What went wrong, for a verdict that needs a human.
    pub detail: Option<String>,
}

impl ChaosVerdict {
    /// A verdict that failed before the rewriting run was reached, so it mutated nothing.
    fn failed(class: ChaosClass, detail: String) -> Self {
        Self { class, applied: ChaosTally::default(), detail: Some(detail) }
    }
}

/// The per-shape aggregate a whole sweep applied, plus the verdict counts.
#[derive(Debug, Clone, Default)]
pub struct ChaosSweepTally {
    /// Vectors per [`ChaosClass`], keyed by [`ChaosClass::label`].
    pub classes: BTreeMap<&'static str, usize>,
    /// Mutations applied over the whole sweep, per shape.
    pub shapes: ChaosTally,
    /// Every vector that needs a human.
    pub flagged: Vec<UnitChaos>,
    /// Files the runner could not read or parse at all, as rendered errors.
    pub file_errors: Vec<String>,
    /// Files validation skips by filename, and which the sweep therefore judged no vector of.
    pub skipped_files: usize,
}

impl ChaosSweepTally {
    /// Number of vectors in a class.
    pub fn count(&self, class: ChaosClass) -> usize {
        self.classes.get(class.label()).copied().unwrap_or(0)
    }

    /// Total number of vectors judged.
    pub fn total(&self) -> usize {
        self.classes.values().sum()
    }

    /// Whether the run should fail its gate.
    ///
    /// The two content conditions are a failing verdict and a file the sweep could not read. The
    /// other two are what make those mean something: a sweep that judged no vector reaches the
    /// gate with every count truthfully zero, and so does one whose inspector never mutated
    /// anything — a corpus that never arrived and a chaos run that was not chaotic both look
    /// exactly like a clean sweep from the counts alone.
    pub fn is_failure(&self) -> bool {
        self.total() == 0 ||
            self.shapes.total() == 0 ||
            !self.flagged.is_empty() ||
            !self.file_errors.is_empty()
    }

    /// Records one vector's verdict.
    pub fn record(&mut self, verdict: UnitChaos) {
        *self.classes.entry(verdict.class.label()).or_insert(0) += 1;
        if verdict.class.is_failure() {
            self.flagged.push(verdict);
        }
    }

    /// Merges another tally into this one.
    pub fn merge(&mut self, other: Self) {
        for (label, count) in other.classes {
            *self.classes.entry(label).or_insert(0) += count;
        }
        self.shapes.merge(&other.shapes);
        self.flagged.extend(other.flagged);
        self.file_errors.extend(other.file_errors);
        self.skipped_files += other.skipped_files;
    }
}

/// Runs the chaos comparison over every transaction vector of every unit of one fixture file.
pub fn chaos_test_suite(
    path: &Path,
    spec: &SpecName,
    global_seed: u64,
    filter: ShapeFilter,
) -> Result<(Vec<UnitChaos>, ChaosTally), TestError> {
    let path_str = path.to_string_lossy().into_owned();
    if skip_test(path) {
        return Ok((vec![], ChaosTally::default()));
    }

    let fixture_err = |msg: String| TestError {
        name: "chaos".to_string(),
        path: path_str.clone(),
        kind: TestErrorKind::FixtureError(msg),
    };
    let source = std::fs::read_to_string(path).map_err(|e| fixture_err(format!("read: {e}")))?;
    let suite: TestSuite = serde_json::from_str(&source).map_err(|e| TestError {
        name: "Unknown".to_string(),
        path: path_str.clone(),
        kind: e.into(),
    })?;

    let mut verdicts = Vec::with_capacity(suite.0.len());
    let mut shapes = ChaosTally::default();
    for (name, unit) in suite.0 {
        let vectors = unit.vectors();
        let multi = vectors.len() > 1;
        for indexes in vectors {
            let label = if multi { vector_label(&name, indexes) } else { name.clone() };
            let seed = vector_seed(global_seed, &path_str, &label, indexes);
            let verdict =
                match panic_capture::catch(|| chaos_unit(&unit, indexes, spec, seed, filter)) {
                    Ok(verdict) => {
                        shapes.merge(&verdict.applied);
                        UnitChaos {
                            name: label,
                            path: path_str.clone(),
                            seed,
                            class: verdict.class,
                            mutations: verdict.applied.total(),
                            detail: verdict.detail,
                        }
                    }
                    // A vector that panicked has no tally to report and still has to be counted.
                    Err(report) => UnitChaos {
                        name: label,
                        path: path_str.clone(),
                        seed,
                        class: ChaosClass::Panic,
                        mutations: 0,
                        detail: Some(report),
                    },
                };
            verdicts.push(verdict);
        }
    }
    Ok((verdicts, shapes))
}

/// How a corpus-wide chaos run behaves.
#[derive(Debug, Clone, Copy)]
pub struct ChaosRunConfig {
    /// The spec every run executes under.
    pub spec: SpecName,
    /// The global seed every vector's own seed is derived from.
    pub seed: u64,
    /// Which mutations the rewriting run is allowed to make.
    pub filter: ShapeFilter,
    /// Run every file on one thread.
    pub single_thread: bool,
    /// Draw a progress bar.
    pub progress: bool,
}

/// Runs the chaos comparison over every fixture file, in parallel.
///
/// Installs the panic capture hook, for the same reason the differential sweep does: a
/// `debug_assert!` one vector trips becomes that vector's verdict instead of taking down a worker
/// thread, which is what makes a single-process full-corpus sweep possible at all.
pub fn run_chaos(scan: FixtureScan, config: ChaosRunConfig) -> ChaosSweepTally {
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
                .name(format!("chaos-{i}"))
                .spawn(move || {
                    let mut tally = ChaosSweepTally::default();
                    loop {
                        let index = next.fetch_add(1, Ordering::SeqCst);
                        let Some(path) = queue.lock().unwrap().get(index).cloned() else {
                            return tally;
                        };
                        if is_skipped_fixture(&path) {
                            tally.skipped_files += 1;
                            bar.inc(1);
                            continue;
                        }
                        match chaos_test_suite(&path, &config.spec, config.seed, config.filter) {
                            Ok((verdicts, shapes)) => {
                                tally.shapes.merge(&shapes);
                                for verdict in verdicts {
                                    tally.record(verdict);
                                }
                            }
                            Err(e) => tally.file_errors.push(e.to_string()),
                        }
                        bar.inc(1);
                    }
                })
                .expect("spawn chaos worker"),
        );
    }

    let mut tally = ChaosSweepTally { file_errors: errors, ..ChaosSweepTally::default() };
    for handle in handles {
        match handle.join() {
            Ok(worker) => tally.merge(worker),
            Err(_) => tally
                .file_errors
                .push("a chaos worker thread panicked; its files were not judged".to_string()),
        }
    }
    bar.finish_and_clear();
    tally
}
