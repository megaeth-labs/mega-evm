//! The two windows in which a rewrite lands after the accounting that should have read it.
//!
//! Both halves of the measurement shim rest on the same claim: what the shim books is what the
//! transaction's envelope actually moved by. There are two places where the number the shim reads
//! and the number the envelope carries are not the same object, and each of them is a fixture
//! here.
//!
//! - **A terminating opcode's `step_end`.** revm's inspected loop runs `step_end` *after* the
//!   instruction that produced the frame's action, and that action carries its own copy of the gas
//!   counter. An edit to `interp.gas` at that moment changes the counter `MegaETH`'s tail
//!   settlement measures work against and nothing the caller will ever see, so it must move the
//!   settlement baseline and must not move the ledger. The two neighbouring windows — a `step_end`
//!   in mid-frame, and the one after a `CALL` has set a `NewFrame` action — are the boundary of
//!   that rule: the frame resumes on the edited counter in both, so both are booked.
//!
//! - **A precompile's classification.** A precompile is answered inside the frame init and never
//!   becomes a child frame, so its recording site is the only place that knows the forwarded
//!   envelope and the work performed. The split is nonetheless settled at the frame's settlement
//!   point, from what that site staged, exactly as an ordinary frame's is — because a callback runs
//!   in between, and the classification is what decides whether the caller reclaims the remainder.
//!   What that callback may do to the classification is bounded: the journal decision behind a
//!   result frame init produced was taken before any callback ran and is not reachable from one, so
//!   a rewrite that moves such a result across the success / revert / halt boundary is refused and
//!   the settlement reads the classification the EVM produced. The cases below pin the uninspected
//!   split each precompile arm produces, and the refusal that keeps it the one the settlement sees.
//!
//! Every case here is checked by the identity `common::finish` runs on every transaction: the
//! tracker lanes must account for the whole receipt envelope, with the inspector's own term in it.

use crate::{
    common::{
        base_db, transact, transact_inspected, transact_inspected_refused, Outcome, Refusal,
        CALLEE, DEFAULT_TX_GAS_LIMIT,
    },
    inspector_common::{call_then_stop, db_with_callee, limits},
};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    kzg_point_evaluation, test_utils::BytecodeBuilder, InspectorLedger, Lane, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CALL, INVALID, POP, STOP},
    context::ContextTr,
    interpreter::{
        interpreter_types::LoopControl, CallInputs, CallOutcome, FrameInput, InstructionResult,
        Interpreter, InterpreterAction, InterpreterTypes,
    },
    Inspector,
};
use sha2::{Digest, Sha256};

/// Gas the edit-once inspector writes into a live interpreter's counter.
const INJECT: u64 = 1_000;

/// Gas every probed CALL forwards. Well inside the 63/64 rule at the default transaction gas
/// limit and well inside the default compute budget, so the forwarded envelope is exactly this.
const FORWARDED: u64 = 1_000_000;

/// The transaction gas limit is not what binds any fixture here — pinned at compile time, so a
/// change to the shared limit cannot silently turn a destroyed-remainder case into an
/// out-of-gas one.
const _: () = assert!(DEFAULT_TX_GAS_LIMIT > 10 * FORWARDED);

/// The identity precompile.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");
/// blake2f. Rejects any input whose length is not 213 bytes, before charging anything.
const BLAKE2F: Address = address!("0000000000000000000000000000000000000009");
/// KZG point evaluation.
const KZG: Address = address!("000000000000000000000000000000000000000a");

// --- A: the window a terminating opcode's `step_end` sits in ---------------------------------

/// Which of the three `step_end` windows an edit is aimed at, told apart by the action the
/// instruction that just ran left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Window {
    /// No action yet: the frame carries on, and the edited counter is what it carries on with.
    MidFrame,
    /// A `NewFrame` action: the frame suspends into a child and then resumes on this counter.
    Suspending,
    /// A `Return` action: the frame is over, and the gas it hands back was copied into the action
    /// before this callback ran.
    Terminating,
}

impl Window {
    fn of<INTR: InterpreterTypes>(interp: &mut Interpreter<INTR>) -> Self {
        match interp.bytecode.action() {
            None => Self::MidFrame,
            Some(InterpreterAction::NewFrame(_)) => Self::Suspending,
            Some(InterpreterAction::Return(_)) => Self::Terminating,
        }
    }
}

/// Writes [`INJECT`] into the interpreter's counter once, at the first `step_end` that sits in
/// `window`.
#[derive(Debug)]
struct CounterEditor {
    window: Window,
    fired: u32,
}

impl CounterEditor {
    fn new(window: Window) -> Self {
        Self { window, fired: 0 }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CounterEditor {
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.fired > 0 || Window::of(interp) != self.window {
            return;
        }
        self.fired += 1;
        interp.gas.erase_cost(INJECT);
    }
}

/// `PUSH1 1; POP; STOP` — three opcodes, so a mid-frame `step_end` and a terminating one are both
/// reached, and nothing else happens in between.
fn straight_line_code() -> Bytes {
    BytecodeBuilder::default().push_number(1u64).append(POP).append(STOP).build()
}

/// A `CALL` into the identity precompile, its success flag popped, then `STOP` — so the frame
/// suspends once and the `step_end` after the `CALL` opcode sits in [`Window::Suspending`].
fn suspending_code() -> Bytes {
    call_then_stop(IDENTITY, FORWARDED)
}

fn run_counter_edit(code: Bytes, window: Window) -> (Outcome, Outcome, u32) {
    let plain = transact(MegaSpecId::REX7, base_db(code.clone()), limits());
    let mut inspector = CounterEditor::new(window);
    let edited = transact_inspected(MegaSpecId::REX7, base_db(code), limits(), &mut inspector);
    (plain, edited, inspector.fired)
}

/// An edit made in the terminating window reaches nobody, so nothing is booked for it — and the
/// transaction is the one the EVM would have produced alone.
///
/// The action the terminating instruction set already holds its own copy of the counter, so the
/// caller is handed a number this edit never touched. Booking it would tell the conservation law
/// that the transaction spent [`INJECT`] less than it did.
///
/// `compute_gas` being unmoved is the other half of the rule, and the one that would break if the
/// fix were written as "leave the counter alone" rather than "book nothing for it": the tail
/// settlement measures work as a drop in this very counter, so without the baseline shift the
/// injection would read as [`INJECT`] gas of work the frame never performed.
#[test]
fn test_an_edit_in_the_terminating_window_is_not_booked() {
    let (plain, edited, fired) = run_counter_edit(straight_line_code(), Window::Terminating);

    assert_eq!(fired, 1, "the fixture must reach a terminating step_end exactly once");
    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger::default(),
        "an edit that cannot reach the envelope must leave the ledger untouched",
    );
    assert_eq!(
        edited.compute_gas, plain.compute_gas,
        "the settlement baseline must absorb the edit, so it counts as no work at all",
    );
    assert_eq!(
        edited.total_gas_spent, plain.total_gas_spent,
        "the envelope must be the one the uninspected run produced",
    );
}

/// The near boundary: a mid-frame edit is booked, because the frame carries on spending the
/// counter the callback left behind.
#[test]
fn test_an_edit_in_mid_frame_is_still_booked() {
    let (_, edited, fired) = run_counter_edit(straight_line_code(), Window::MidFrame);

    assert_eq!(fired, 1, "the fixture must reach a mid-frame step_end exactly once");
    assert_eq!(
        edited.inspector_ledger.gas,
        Lane::once(i128::from(INJECT)),
        "gas written into a counter the frame will keep spending is conjured gas",
    );
}

/// The far boundary, and the one a coarser rule would get wrong: a `CALL` has set an action too,
/// but it is a `NewFrame` action — the frame suspends, the child runs, and then the frame resumes
/// on exactly this counter. So the edit reaches the envelope and must be booked, even though the
/// interpreter is "at the end of its loop" in precisely the same sense as the terminating case.
#[test]
fn test_an_edit_in_the_suspending_window_is_still_booked() {
    let (_, edited, fired) = run_counter_edit(suspending_code(), Window::Suspending);

    assert_eq!(fired, 1, "the fixture must suspend into a child frame exactly once");
    assert_eq!(
        edited.inspector_ledger.gas,
        Lane::once(i128::from(INJECT)),
        "a suspended frame resumes on the edited counter, so the edit reaches the envelope",
    );
}

// --- B: a precompile's classification, rewritten after its recording site ---------------------

/// Rewrites the result of the call to `target` into `to`, once.
#[derive(Debug)]
struct Reclassifier {
    target: Address,
    to: InstructionResult,
    fired: u32,
}

impl Reclassifier {
    fn new(target: Address, to: InstructionResult) -> Self {
        Self { target, to, fired: 0 }
    }
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for Reclassifier {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if self.fired > 0 || inputs.target_address != self.target {
            return;
        }
        self.fired += 1;
        outcome.result.result = self.to;
    }
}

/// A `CALL` forwarding [`FORWARDED`] gas to `target` with `calldata` at `mem[0..]`, its success
/// flag popped so the caller survives either classification.
fn call_precompile(target: Address, calldata: &[u8]) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, calldata)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(calldata.len() as u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(target)
        .push_number(FORWARDED)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// The EIP-4844 point-evaluation test vector with the last byte of the proof flipped: 192 bytes
/// with a matching versioned hash, so KZG clears the length doorway and fails inside verification
/// — the one halt shape `MegaETH` prices as work performed.
fn kzg_verification_failure() -> Vec<u8> {
    let commitment = hex::decode(
        "8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca2\
         5f26936857bc3a7c2539ea8ec3a952b7",
    )
    .unwrap();
    let mut versioned_hash = Sha256::digest(&commitment).to_vec();
    versioned_hash[0] = 0x01; // VERSIONED_HASH_VERSION_KZG
    let z =
        hex::decode("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000").unwrap();
    let y =
        hex::decode("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9").unwrap();
    let proof = hex::decode(
        "a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc216074\
         4faf0070725e00b60ad9a026a15b1a8c",
    )
    .unwrap();

    let mut input = Vec::new();
    input.extend_from_slice(&versioned_hash);
    input.extend_from_slice(&z);
    input.extend_from_slice(&y);
    input.extend_from_slice(&commitment);
    input.extend_from_slice(&proof);
    assert_eq!(input.len(), 192, "the priced probe must clear the 192-byte doorway");
    let last = input.len() - 1;
    input[last] ^= 0x01;
    input
}

/// Runs the fixture twice: once uninspected, and once with the classification rewritten across
/// the boundary the shim refuses.
///
/// The refusal is asserted here rather than in each case, so every case below is left stating the
/// one thing that differs between them — which arm of the precompile it reaches, and what the
/// uninspected run's split therefore is.
fn run_reclassified(target: Address, calldata: &[u8], to: InstructionResult) -> (Outcome, Refusal) {
    let code = call_precompile(target, calldata);
    let plain = transact(MegaSpecId::REX7, base_db(code.clone()), limits());
    let mut inspector = Reclassifier::new(target, to);
    let refusal =
        transact_inspected_refused(MegaSpecId::REX7, base_db(code), limits(), &mut inspector);
    assert_eq!(inspector.fired, 1, "the fixture must reach the precompile's call_end exactly once");
    assert_eq!(refusal.rejected_rewrites, 1, "the shim must count the refusal");
    assert!(
        refusal.error.contains("classification of a result frame init produced"),
        "the transaction must fail with the refusal's own reason, got {}",
        refusal.error,
    );
    (plain, refusal)
}

/// A successful precompile rewritten into a halt is refused, and the uninspected run destroys
/// nothing.
///
/// The rewrite is the direction with state behind it: `make_call_frame` commits the checkpoint
/// before it returns a successful precompile's result, so a caller told the call halted would be
/// told so with the transfer that funded it standing.
#[test]
fn test_rewriting_a_successful_precompile_into_a_halt_is_refused() {
    let (plain, _) = run_reclassified(IDENTITY, &[], InstructionResult::OutOfGas);

    assert_eq!(plain.destroyed, 0, "the uninspected run destroys nothing");
    assert_eq!(
        plain.compute_gas,
        plain.enforced(),
        "with nothing destroyed the reported total is the work performed",
    );
}

/// A rejected precompile rewritten into a success is refused, and the uninspected run destroys the
/// whole envelope.
///
/// The other direction, and the other half of the split: `blake2f` rejects the input before any
/// work, so `make_call_frame` reverted the checkpoint and nothing was performed.
#[test]
fn test_reviving_a_rejected_precompile_is_refused() {
    let (plain, _) = run_reclassified(BLAKE2F, &[], InstructionResult::Stop);

    assert_eq!(
        plain.destroyed, FORWARDED,
        "blake2f rejects the input before any work, so the uninspected run destroys all of it",
    );
    assert_eq!(
        plain.enforced(),
        plain.compute_gas - plain.destroyed,
        "nothing was performed, so nothing enforces",
    );
}

/// The third arm, and the only one whose failure `MegaETH` prices as work: a KZG verification that
/// ran and rejected.
///
/// The refusal matters most here. A halting precompile's gas object carries the whole forwarded
/// envelope as remaining — it is reset rather than spent down — so a caller told such a call
/// succeeded would reclaim all of it, the fixed fee included. That fee is gas the execution priced
/// and the envelope never paid, which is exactly the shape the refusal keeps out.
#[test]
fn test_reviving_a_priced_precompile_failure_is_refused() {
    let calldata = kzg_verification_failure();
    let (plain, _) = run_reclassified(KZG, &calldata, InstructionResult::Stop);

    assert_eq!(
        plain.destroyed,
        FORWARDED - kzg_point_evaluation::GAS_COST,
        "verification ran, so the uninspected run destroys the envelope less the fixed fee",
    );
    assert_eq!(
        plain.compute_gas - plain.destroyed,
        plain.enforced(),
        "the fee is the work performed, and it is what enforces",
    );
}

// --- C: the pending action itself ---------------------------------------------------------------

/// Gas an action edit moves.
const ACTION_DELTA: u64 = 700;

/// Reaches past the interpreter's gas counter and into the action the interpreter is holding, once.
///
/// The counter and the action are two different objects at exactly one moment — after a
/// terminating or suspending instruction has run and before the loop hands the action on — and
/// this is the inspector that edits the second one.
#[derive(Debug)]
struct ActionEditor {
    window: Window,
    /// Positive raises the gas the action carries, negative lowers it.
    delta: i64,
    /// Fire only on an action whose classification is (or is not) an exceptional halt.
    halting: bool,
    fired: u32,
}

impl ActionEditor {
    fn raise(window: Window) -> Self {
        Self { window, delta: ACTION_DELTA as i64, halting: false, fired: 0 }
    }

    fn lower(window: Window) -> Self {
        Self { window, delta: -(ACTION_DELTA as i64), halting: false, fired: 0 }
    }

    fn on_halt() -> Self {
        Self { window: Window::Terminating, delta: ACTION_DELTA as i64, halting: true, fired: 0 }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for ActionEditor {
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.fired > 0 || Window::of(interp) != self.window {
            return;
        }
        match interp.bytecode.action() {
            Some(InterpreterAction::Return(result)) => {
                if result.result.is_ok_or_revert() == self.halting {
                    return;
                }
                if self.delta >= 0 {
                    result.gas.erase_cost(self.delta.unsigned_abs());
                } else {
                    assert!(
                        result.gas.record_regular_cost(self.delta.unsigned_abs()),
                        "the fixture must leave the action enough gas for the removal to land",
                    );
                }
            }
            Some(InterpreterAction::NewFrame(FrameInput::Call(inputs))) => {
                inputs.gas_limit = inputs.gas_limit.saturating_add(self.delta.unsigned_abs());
            }
            _ => return,
        }
        self.fired += 1;
    }
}

/// A `CALL` into [`CALLEE`], its result flag popped, then `STOP` — so the first terminating
/// `step_end` of the transaction belongs to an *inner* frame, and what that frame's action carries
/// is decided by the callee the fixture installs.
fn call_callee_code() -> Bytes {
    call_then_stop(CALLEE, FORWARDED)
}

/// Gas written into a returning frame's pending action is gas the caller really reclaims, so it
/// has to be booked — the frame's classification is what says so, and the classification is only
/// known at the frame's settlement point.
#[test]
fn test_raising_a_returning_frames_pending_action_is_booked() {
    let plain = transact(MegaSpecId::REX7, base_db(straight_line_code()), limits());
    let mut inspector = ActionEditor::raise(Window::Terminating);
    let edited = transact_inspected(
        MegaSpecId::REX7,
        base_db(straight_line_code()),
        limits(),
        &mut inspector,
    );

    assert_eq!(inspector.fired, 1, "the fixture must reach a terminating step_end exactly once");
    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            result: Lane::once(i128::from(ACTION_DELTA)),
            ..InspectorLedger::default()
        },
        "an edit to the action a returning frame hands back is an edit to the envelope",
    );
    assert_eq!(
        edited.total_gas_spent,
        plain.total_gas_spent - ACTION_DELTA,
        "the transaction really did spend less, which is why the ledger has to carry it",
    );
    assert_eq!(
        edited.compute_gas, plain.compute_gas,
        "the edit is not work: the frame performed exactly what it performed uninspected",
    );
}

/// The same edit in the other direction.
#[test]
fn test_lowering_a_returning_frames_pending_action_is_booked() {
    let plain = transact(MegaSpecId::REX7, base_db(straight_line_code()), limits());
    let mut inspector = ActionEditor::lower(Window::Terminating);
    let edited = transact_inspected(
        MegaSpecId::REX7,
        base_db(straight_line_code()),
        limits(),
        &mut inspector,
    );

    assert_eq!(inspector.fired, 1, "the fixture must reach a terminating step_end exactly once");
    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            result: Lane::once(-i128::from(ACTION_DELTA)),
            ..InspectorLedger::default()
        },
        "gas taken out of the action is gas the caller never gets back",
    );
    assert_eq!(
        edited.total_gas_spent,
        plain.total_gas_spent + ACTION_DELTA,
        "the transaction really did spend more",
    );
}

/// The classification branch: a halting frame hands nothing back, so an edit to the gas its action
/// carries moves nothing and must not reach the lane's *net* — and the remainder it destroys is
/// the EVM's own number, not the edited one.
///
/// The lane's gross carries the edit all the same. Whether it moved the envelope is what the
/// classification decides; whether the inspector made it is not, and the block guard asks the
/// second question.
#[test]
fn test_editing_a_halting_frames_pending_action_moves_nothing() {
    let callee = BytecodeBuilder::default().append(INVALID).build();
    let plain =
        transact(MegaSpecId::REX7, db_with_callee(call_callee_code(), callee.clone()), limits());
    let mut inspector = ActionEditor::on_halt();
    let edited = transact_inspected(
        MegaSpecId::REX7,
        db_with_callee(call_callee_code(), callee),
        limits(),
        &mut inspector,
    );

    assert_eq!(inspector.fired, 1, "the fixture must halt an inner frame exactly once");
    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            result: Lane::of(0, u128::from(ACTION_DELTA)),
            ..InspectorLedger::default()
        },
        "a halting frame hands its remainder to nobody, so the edit moves the envelope by nothing \
         — and the lane still has to show it was made",
    );
    assert_eq!(
        edited.inspector_ledger.conjured_gas(),
        0,
        "the conservation law reads the net, which is what stays zero",
    );
    assert!(
        !edited.inspector_ledger.is_zero(),
        "and the block guard reads the gross, which is what does not",
    );
    assert_eq!(
        edited.destroyed, plain.destroyed,
        "the destroyed remainder is the EVM's own, not the one the inspector wrote",
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent, "and the envelope is unmoved");
}

/// The other action variant: gas written into a pending `NewFrame` action is the envelope a child
/// frame is about to be built with, which the caller was never debited for.
#[test]
fn test_raising_a_pending_new_frame_action_is_booked_as_an_envelope() {
    let plain = transact(MegaSpecId::REX7, base_db(suspending_code()), limits());
    let mut inspector = ActionEditor::raise(Window::Suspending);
    let edited =
        transact_inspected(MegaSpecId::REX7, base_db(suspending_code()), limits(), &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must suspend into a child frame exactly once");
    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger { env: Lane::once(i128::from(ACTION_DELTA)), ..InspectorLedger::default() },
        "the child's budget grew by gas the caller's CALL never forwarded",
    );
    assert_eq!(
        edited.total_gas_spent,
        plain.total_gas_spent - ACTION_DELTA,
        "the child hands the extra budget straight back, so the transaction spends less",
    );
}

/// Rewrites the classification inside a pending `Return` action, once, at the terminating
/// `step_end` of the frame that set it.
#[derive(Debug)]
struct ActionReclassifier {
    to: InstructionResult,
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for ActionReclassifier {
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.fired > 0 {
            return;
        }
        let Some(InterpreterAction::Return(result)) = interp.bytecode.action() else { return };
        result.result = self.to;
        self.fired += 1;
    }
}

/// An edit to a pending action that is not to its gas moves nothing and is booked as an
/// intervention — but it still decides what the frame did, so the frame's state follows it.
///
/// The action is what `classify_frame_action` builds the frame's result from, so a classification
/// written here is the one the caller sees and the one the journal decision is taken on. Nothing
/// on any gas lane can see that, which is what the intervention counter is for.
#[test]
fn test_rewriting_a_pending_actions_classification_is_an_intervention() {
    let callee =
        BytecodeBuilder::default().sstore(U256::from(1u64), U256::from(1u64)).append(STOP).build();
    let plain =
        transact(MegaSpecId::REX7, db_with_callee(call_callee_code(), callee.clone()), limits());
    let mut inspector = ActionReclassifier { to: InstructionResult::Revert, fired: 0 };
    let edited = transact_inspected(
        MegaSpecId::REX7,
        db_with_callee(call_callee_code(), callee),
        limits(),
        &mut inspector,
    );

    assert_eq!(inspector.fired, 1, "the fixture must reach a terminating step_end exactly once");
    assert_eq!(
        plain.storage_value(CALLEE, U256::from(1u64)),
        U256::from(1u64),
        "uninspected, the callee's write is committed",
    );
    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger { interventions: 1, ..InspectorLedger::default() },
        "no gas moved, and the only thing left to say is that the transaction was not left alone",
    );
    assert_eq!(
        edited.storage_value(CALLEE, U256::from(1u64)),
        U256::ZERO,
        "a frame the caller was told reverted must leave no write behind",
    );
}
