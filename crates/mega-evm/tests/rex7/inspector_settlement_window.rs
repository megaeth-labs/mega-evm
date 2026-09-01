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
//!   settlement baseline and must not move the ledger. The two neighbouring windows — a step_end in
//!   mid-frame, and the one after a `CALL` has set a `NewFrame` action — are the boundary of that
//!   rule: the frame resumes on the edited counter in both, so both are booked.
//!
//! - **A precompile's classification.** A precompile is answered inside the frame init and never
//!   becomes a child frame, so its recording site is the only place that knows the forwarded
//!   envelope and the work performed. It is not, however, the place that knows how the call ends:
//!   `call_end` runs afterwards and can rewrite the classification, and the classification is what
//!   decides whether the caller reclaims the remainder. So the split has to be settled at the
//!   frame's settlement point, from what the recording site staged, exactly as an ordinary frame's
//!   is.
//!
//! Every case here is checked by the identity `common::finish` runs on every transaction: the
//! tracker lanes must account for the whole receipt envelope, with the inspector's own term in it.

use crate::common::{
    transact, transact_inspected, Outcome, CALLER, CONTRACT, DEFAULT_TX_GAS_LIMIT, ONE_ETH,
};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    kzg_point_evaluation,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, InspectorLedger, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CALL, POP, STOP},
    context::ContextTr,
    interpreter::{
        interpreter_types::LoopControl, CallInputs, CallOutcome, InstructionResult, Interpreter,
        InterpreterAction, InterpreterTypes,
    },
    Inspector,
};
use sha2::{Digest, Sha256};

/// Gas the edit-once inspector writes into a live interpreter's counter.
const INJECT: u64 = 1_000;

/// Gas every probed CALL forwards. Well inside the 63/64 rule at the default transaction gas
/// limit and well inside the default compute budget, so the forwarded envelope is exactly this.
const FORWARDED: u64 = 1_000_000;

/// The identity precompile.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");
/// blake2f. Rejects any input whose length is not 213 bytes, before charging anything.
const BLAKE2F: Address = address!("0000000000000000000000000000000000000009");
/// KZG point evaluation.
const KZG: Address = address!("000000000000000000000000000000000000000a");

/// What the identity precompile charges for an empty input: its base cost, with no words to copy.
const IDENTITY_GAS: u64 = 15;

fn limits() -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)
}

fn db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

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
    BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(IDENTITY)
        .push_number(FORWARDED)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

fn run_counter_edit(code: Bytes, window: Window) -> (Outcome, Outcome, u32) {
    let plain = transact(MegaSpecId::REX7, db(code.clone()), limits());
    let mut inspector = CounterEditor::new(window);
    let edited = transact_inspected(MegaSpecId::REX7, db(code), limits(), &mut inspector);
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
        i128::from(INJECT),
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
        i128::from(INJECT),
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

fn run_reclassified(
    target: Address,
    calldata: &[u8],
    to: InstructionResult,
) -> (Outcome, Outcome, u32) {
    let code = call_precompile(target, calldata);
    let plain = transact(MegaSpecId::REX7, db(code.clone()), limits());
    let mut inspector = Reclassifier::new(target, to);
    let rewritten = transact_inspected(MegaSpecId::REX7, db(code), limits(), &mut inspector);
    (plain, rewritten, inspector.fired)
}

/// A successful precompile rewritten into a halt destroys the rest of its forwarded envelope, and
/// the transaction has to report it.
///
/// The caller reclaims nothing from a halted call, so everything the identity precompile did not
/// charge for is gone. Its recording site booked a destroyed remainder of zero, because at that
/// moment the call had succeeded.
#[test]
fn test_a_precompile_rewritten_into_a_halt_destroys_its_remainder() {
    let (plain, rewritten, fired) = run_reclassified(IDENTITY, &[], InstructionResult::OutOfGas);

    assert_eq!(fired, 1, "the fixture must reach the precompile's call_end exactly once");
    assert_eq!(plain.destroyed, 0, "the uninspected run destroys nothing");
    assert_eq!(
        rewritten.destroyed,
        FORWARDED - IDENTITY_GAS,
        "everything the precompile did not spend is destroyed once the call halts",
    );
    assert_eq!(
        rewritten.enforced(),
        plain.enforced(),
        "the same work ran either way, so the enforcing lane must not move",
    );
    assert_eq!(
        rewritten.compute_gas,
        plain.compute_gas + rewritten.destroyed,
        "the destroyed remainder is reported on top of the work performed",
    );
}

/// A halted precompile rewritten into a success destroys nothing, because the caller reclaims the
/// envelope its recording site had already written off.
#[test]
fn test_a_precompile_rewritten_into_a_success_destroys_nothing() {
    let (plain, rewritten, fired) = run_reclassified(BLAKE2F, &[], InstructionResult::Stop);

    assert_eq!(fired, 1, "the fixture must reach the precompile's call_end exactly once");
    assert_eq!(
        plain.destroyed, FORWARDED,
        "blake2f rejects the input before any work, so the uninspected run destroys all of it",
    );
    assert_eq!(rewritten.destroyed, 0, "a reclaimed envelope is not a destroyed one");
    assert_eq!(
        rewritten.compute_gas,
        rewritten.enforced(),
        "with nothing destroyed the reported total is the work performed",
    );
}

/// The corner where the two halves of the split move in opposite directions: a KZG failure that
/// `MegaETH` prices as work, rewritten into a success.
///
/// The fixed fee really was performed and stays on the enforcing lane. But the halt's gas object
/// carries the whole forwarded envelope as remaining — a halting precompile's gas is reset rather
/// than spent down — so a caller told the call succeeded reclaims all of it, including the fee.
/// That fee is then gas the execution priced and the envelope never paid: conjured gas, which the
/// ledger has to carry or the law reads the transaction as having spent less than it did.
#[test]
fn test_a_priced_precompile_failure_rewritten_into_a_success_conjures_its_fee() {
    let calldata = kzg_verification_failure();
    let (plain, rewritten, fired) = run_reclassified(KZG, &calldata, InstructionResult::Stop);

    assert_eq!(fired, 1, "the fixture must reach the precompile's call_end exactly once");
    assert_eq!(
        plain.destroyed,
        FORWARDED - kzg_point_evaluation::GAS_COST,
        "verification ran, so the uninspected run destroys the envelope less the fixed fee",
    );
    assert_eq!(rewritten.destroyed, 0, "a reclaimed envelope is not a destroyed one");
    assert_eq!(
        rewritten.enforced(),
        plain.enforced(),
        "the verification work is the same on both runs",
    );
    assert_eq!(
        rewritten.inspector_conjured_gas,
        i128::from(kzg_point_evaluation::GAS_COST),
        "the fee the caller reclaimed is gas the transaction was never charged for",
    );
}

/// The transaction gas limit is not what binds any of these fixtures — stated once, so a future
/// change to the shared limit cannot silently turn a destroyed-remainder case into an
/// out-of-gas one.
#[test]
fn test_the_fixtures_are_not_bound_by_the_transaction_gas_limit() {
    assert!(
        DEFAULT_TX_GAS_LIMIT > 10 * FORWARDED,
        "the forwarded envelope must be a small part of the transaction's own",
    );
}
