//! What the measurement shim books, over every shape that can move a number it reports.
//!
//! `MegaETH` wraps every inspector it is handed. The EVM does not execute inside an inspector
//! callback, so anything that changes across one is the inspector's doing by construction — which
//! is what makes the callback boundary a sound place to measure from. Every fixture here is the
//! same comparison: one run with an inspector against one without, over the same fixture, with the
//! conservation law checked on both by the shared driver.
//!
//! The sections, in the order they appear:
//!
//! 1. **The shim itself** — gas written into an interpreter's counter or a frame's gas limit is
//!    measured, booked, and kept out of enforcement, with the clamp re-derived on the spot; an
//!    observation-only inspector is bit-identical to no inspector at all.
//! 2. **The two settlement windows** — a terminating opcode's `step_end`, whose counter edit
//!    reaches nobody, and a precompile's classification, whose split has to follow the callback
//!    rather than the recording site.
//! 3. **The blind spots** — the rewrite shapes an all-zero ledger used to admit: a frame's memory
//!    grown for free, an outcome's metadata rewritten around the result inside it, two edits to one
//!    signed lane that cancel, an instruction deleted by stepping the program counter past it, and
//!    a return buffer conjured in front of a frame that made no call.
//! 4. **The receipt's other two numbers** — the EIP-3529 refund, measured at the callback boundary
//!    because the EVM produces refunds too, and the EIP-8037 state-gas dimension, settled from the
//!    transaction's final figures because `MegaETH` produces none of it and revm propagates it by
//!    replacement.
//! 5. **Interception** — the gas an inspector puts into a synthetic outcome, over the four sizings
//!    it can choose relative to the envelope it was handed, and the halt direction where the choice
//!    reaches nothing.
//!
//! The rewrites the shim *refuses* are in `shim_refusals.rs`; the exhaustive callback × shape
//! sweep is in `inspector_cheat_matrix.rs`.

use crate::{
    common::{
        base_db, transact, transact_inspected, transact_inspected_refused, Outcome, Refusal,
        CALLEE, CONTRACT, DEFAULT_TX_GAS_LIMIT, EMPTY_TARGET,
    },
    inspector_common::{
        append_call, call_then_stop, countdown_loop_code, db_with_callee, deploy_then_stop, limits,
        limits_with_compute, plain_and_cheated, plain_run_code,
    },
};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    kzg_point_evaluation,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    ConservationTerms, EvmTxRuntimeLimits, InspectorLedger, Lane, MegaHaltReason, MegaSpecId,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLER, CREATE, GAS, INVALID, MLOAD, MSTORE, MSTORE8, POP, RETURN, RETURNDATASIZE,
        SSTORE, STOP,
    },
    context::{Cfg, ContextTr},
    handler::FrameResult,
    interpreter::{
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, Jumps, LoopControl, MemoryTr, ReturnData},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        Interpreter, InterpreterAction, InterpreterResult, InterpreterTypes,
    },
    Inspector,
};
use sha2::{Digest, Sha256};
use std::vec::Vec;

// === 1. the shim itself =======================================================================
//
// The measurement shim: what an inspector does to gas is measured, booked, and kept out of
// enforcement.
//
// `MegaETH` wraps every inspector it is handed. The EVM does not execute inside an inspector
// callback, so anything that changes across one is the inspector's doing by construction — which
// is what makes the callback boundary a sound place to measure from.
//
// Each test here is one shape a rewriting inspector can take, and each pins a different half of
// the mechanism:
//
// - injecting gas into a running interpreter must not buy compute headroom, and the gas clamp must
//   tighten again immediately rather than at the next checkpoint;
// - raising a child frame's gas limit conjures gas the transaction never funded, which the ledger
//   has to account for or the conservation law breaks;
// - an observation-only inspector changes nothing at all;
// - and removing gas is measured with the same machinery as adding it.

/// Edits the interpreter's gas counter once, at the `at`-th step, by `delta` gas.
///
/// One edit rather than a per-step trickle so that the amount conjured (or destroyed) is an exact
/// number a test can assert on, and so the edit lands well inside the plain segment rather than at
/// its boundary.
#[derive(Default)]
struct GasEditor {
    at: u64,
    delta: i64,
    steps: u64,
    applied: bool,
}

impl GasEditor {
    fn new(at: u64, delta: i64) -> Self {
        Self { at, delta, steps: 0, applied: false }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for GasEditor {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.steps += 1;
        if self.steps != self.at || self.applied {
            return;
        }
        self.applied = true;
        if self.delta >= 0 {
            interp.gas.erase_cost(self.delta.unsigned_abs());
        } else {
            assert!(
                interp.gas.record_regular_cost(self.delta.unsigned_abs()),
                "the fixture must leave enough gas for the removal to land",
            );
        }
    }
}

/// Raises the gas limit of every call to [`CALLEE`] by a fixed amount.
#[derive(Default)]
struct CallGasLimitRaiser {
    bonus: u64,
    raises: u64,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CallGasLimitRaiser {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address == CALLEE {
            inputs.gas_limit += self.bonus;
            self.raises += 1;
        }
        None
    }
}

/// Rewrites every successful contract creation into a revert — the shape the frame loop has to
/// carry through to the journal.
#[derive(Default)]
struct CreateKiller {
    killed: u64,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CreateKiller {
    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if outcome.result.result.is_ok() {
            outcome.result.result = InstructionResult::Revert;
            self.killed += 1;
        }
    }
}

/// Counts callbacks and changes nothing.
#[derive(Default)]
struct Observer {
    steps: u64,
    calls: u64,
    call_ends: u64,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Observer {
    fn step(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.steps += 1;
    }

    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.calls += 1;
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends += 1;
    }
}

/// (i) Gas injected into a running interpreter buys no compute headroom, is booked, and the clamp
/// tightens again on the spot.
///
/// The fixture is a checkpoint-free loop under a compute limit far below what the loop needs, so
/// the gas clamp is the only thing that can stop it: the visible counter is pinned to the compute
/// headroom and revm's own gas check rejects the crossing opcode. An inspector then writes four
/// times that headroom into the counter, mid-loop.
///
/// Three separate mechanisms are pinned:
///
/// - **Enforcement does not eat the injection.** The recorded compute total is identical to the
///   uninspected run's, to the gas. Without the baseline shift, the injection reads as negative
///   work and the loop is handed free headroom.
/// - **The clamp is re-derived immediately.** Usage still stops exactly at the limit. Without the
///   re-clamp the loop runs on the injected gas until the frame ends, and the frame-exit settlement
///   then records the whole overshoot — the halt still lands, but hundreds of thousands of gas
///   late.
/// - **The ledger records it.** Exactly what was injected, no more.
#[test]
fn test_injected_gas_is_booked_and_never_becomes_compute_headroom() {
    const INJECTED: u64 = 20_000;
    let code = countdown_loop_code(10_000);
    // Far below what the loop needs, so the clamp binds for the whole run.
    let intrinsic = transact(MegaSpecId::REX7, base_db(plain_run_code(0)), limits()).compute_gas;
    let limits = limits_with_compute(intrinsic + 5_000);

    let plain = transact(MegaSpecId::REX7, base_db(code.clone()), limits);
    let mut inspector = GasEditor::new(20, INJECTED as i64);
    let inspected = transact_inspected(MegaSpecId::REX7, base_db(code), limits, &mut inspector);

    assert!(inspector.applied, "the fixture must reach the injection point");
    assert!(
        matches!(plain.halt_reason("plain"), MegaHaltReason::ComputeGasLimitExceeded { .. }),
        "fixture check: the uninspected run must stop on the compute limit, got {:?}",
        plain.halt_reason("plain"),
    );
    assert_eq!(
        inspected.enforced(),
        plain.enforced(),
        "the injection must be neither counted as work nor deducted from it, and the re-derived \
         clamp must stop the loop at the same opcode the uninspected run stopped at; \
         inspected result {:?}",
        inspected.result,
    );
    assert!(
        matches!(
            inspected.halt_reason("inspected"),
            MegaHaltReason::ComputeGasLimitExceeded { .. }
        ),
        "injected gas must not turn a compute-limit halt into something else, got {:?}",
        inspected.halt_reason("inspected"),
    );
    assert_eq!(
        inspected.inspector_ledger.gas,
        Lane::once(i128::from(INJECTED)),
        "the ledger must hold exactly what was injected",
    );
    assert_eq!(inspected.inspector_ledger.env, Lane::default(), "no frame envelope was touched");
    assert_eq!(
        i128::from(inspected.total_gas_spent) + i128::from(INJECTED),
        i128::from(plain.total_gas_spent),
        "the injected gas is refunded with the rest of the rescued remainder, so the transaction \
         spends exactly that much less than the uninspected run",
    );
}

/// (v) The same machinery, in the other direction: gas removed from a running interpreter is
/// booked as a negative entry and is not charged as work.
///
/// Under an active clamp the removal comes out of the hidden remainder rather than the visible
/// counter — the frame has more EVM gas than compute headroom, and destroying EVM gas does not
/// shrink the headroom — so the transaction runs to the same successful end while spending exactly
/// the removed amount more.
#[test]
fn test_removed_gas_is_booked_as_a_negative_entry_and_is_not_charged_as_work() {
    const REMOVED: u64 = 1_000;
    let code = plain_run_code(200);

    let plain = transact(MegaSpecId::REX7, base_db(code.clone()), limits());
    let mut inspector = GasEditor::new(20, -(REMOVED as i64));
    let inspected = transact_inspected(MegaSpecId::REX7, base_db(code), limits(), &mut inspector);

    assert!(inspector.applied, "the fixture must reach the removal point");
    assert!(plain.result.is_success(), "fixture check: {:?}", plain.result);
    assert!(inspected.result.is_success(), "removing gas must not fail the transaction");
    assert_eq!(
        inspected.inspector_ledger.gas,
        Lane::once(-i128::from(REMOVED)),
        "the ledger must hold the removal as a negative entry",
    );
    assert_eq!(
        inspected.enforced(),
        plain.enforced(),
        "gas the inspector destroyed is not work the EVM performed",
    );
    assert_eq!(
        inspected.total_gas_spent,
        plain.total_gas_spent + REMOVED,
        "the removed gas never comes back, so the envelope is exactly that much larger",
    );
}

/// (ii) Raising a child frame's gas limit conjures gas the transaction never funded, and the
/// envelope only balances once the ledger accounts for it.
///
/// The caller's `CALL` opcode debited the gas it forwards before any inspector callback ran, so the
/// bonus the inspector adds is paid for by nobody. The child hands it straight back on return, and
/// the transaction ends up spending exactly that much less than the uninspected run.
///
/// Without the `env` lane the conservation law derives a destroyed total that is short by the
/// bonus, and the envelope assertion inside `execute_transaction` fails on the spot.
#[test]
fn test_a_raised_child_gas_limit_is_booked_as_conjured_gas() {
    const BONUS: u64 = 10_000;
    let callee = plain_run_code(20);
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(50_000u64) // gas
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    let build_db = || db_with_callee(code.clone(), callee.clone());

    let plain = transact(MegaSpecId::REX7, build_db(), limits());
    let mut inspector = CallGasLimitRaiser { bonus: BONUS, raises: 0 };
    let inspected = transact_inspected(MegaSpecId::REX7, build_db(), limits(), &mut inspector);

    assert_eq!(inspector.raises, 1, "the fixture must make exactly one inner call");
    assert!(plain.result.is_success(), "fixture check: {:?}", plain.result);
    assert!(inspected.result.is_success(), "the inner call must still succeed");
    assert_eq!(
        inspected.inspector_ledger.env,
        Lane::once(i128::from(BONUS)),
        "the ledger must hold exactly the gas the inspector added to the child's envelope",
    );
    assert_eq!(
        inspected.inspector_ledger.gas,
        Lane::default(),
        "no interpreter counter was touched"
    );
    assert_eq!(
        inspected.total_gas_spent + BONUS,
        plain.total_gas_spent,
        "the child returns the conjured gas to its caller, so the transaction spends that much less",
    );
    assert_eq!(
        inspected.enforced(),
        plain.enforced(),
        "a wider envelope is not more work: the child's compute budget comes from the compute \
         tracker, not from its gas limit",
    );
}

/// (ii, mirror) An edit to inputs the EVM never reads conjures nothing, so nothing is booked.
///
/// A callback that returns a synthetic outcome has intercepted the frame: no frame is built from
/// the inputs, so no edit of theirs can widen an envelope. The gas that outcome carries is the
/// inspector's own choice and has nothing to do with the edit — here it is deliberately the
/// original forwarded amount, so the transaction really does conjure nothing and the identity has
/// to close at zero.
///
/// Booking the edit anyway would claim gas was conjured for a frame that never existed, and the
/// conservation law would come out over by the bonus — the same failure as not booking a real one,
/// with the sign flipped.
///
/// The interception itself is booked, on the lane that carries rewrites rather than gas: answering
/// a frame the EVM was about to build changes what the transaction did, whatever it costs.
#[test]
fn test_an_intercepting_callback_books_no_envelope_adjustment() {
    /// Raises the child's gas limit and then intercepts the call, handing back an outcome built
    /// from the amount the caller actually forwarded.
    #[derive(Default)]
    struct Interceptor {
        intercepted: u64,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Interceptor {
        fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
            if inputs.target_address != CALLEE {
                return None;
            }
            let forwarded = inputs.gas_limit;
            inputs.gas_limit += 10_000;
            self.intercepted += 1;
            Some(CallOutcome::new(
                InterpreterResult::new(InstructionResult::Stop, Bytes::new(), Gas::new(forwarded)),
                inputs.return_memory_offset.clone(),
            ))
        }
    }

    let callee = plain_run_code(20);
    let code = call_then_stop(CALLEE, 50_000);
    let db = db_with_callee(code, callee);

    let mut inspector = Interceptor::default();
    let inspected = transact_inspected(MegaSpecId::REX7, db, limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert!(inspected.result.is_success(), "fixture check: {:?}", inspected.result);
    assert_eq!(
        inspected.inspector_ledger,
        InspectorLedger { interventions: 1, ..InspectorLedger::default() },
        "an edit to inputs that never reach a frame conjures nothing, but answering the frame is \
         itself a rewrite",
    );
    assert_eq!(inspected.inspector_ledger.conjured_gas(), 0, "no gas lane may move on this shape");
}

/// An intercepted frame that halts destroys the envelope it was handed, and that has to be booked.
///
/// A callback that returns a synthetic outcome skips the frame init entirely: no frame is built,
/// and the settlement that books what a refused frame init destroys never used to run on this
/// path. A halting outcome hands nothing back to the caller, so the transaction spends that
/// envelope with no compute total to show for it — which is exactly what the conservation law is
/// stated over, and what it goes red on.
#[test]
fn test_an_intercepted_frame_that_halts_books_the_envelope_it_destroys() {
    /// Intercepts the call to [`CALLEE`] with an exceptional halt, keeping the forwarded gas.
    #[derive(Default)]
    struct HaltingInterceptor {
        intercepted: u64,
        forwarded: u64,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for HaltingInterceptor {
        fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
            if inputs.target_address != CALLEE {
                return None;
            }
            self.intercepted += 1;
            self.forwarded = inputs.gas_limit;
            Some(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::OutOfGas,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            ))
        }
    }

    let code = call_then_stop(CALLEE, 50_000);
    let db = db_with_callee(code, plain_run_code(20));

    let mut inspector = HaltingInterceptor::default();
    let inspected = transact_inspected(MegaSpecId::REX7, db, limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert!(inspected.result.is_success(), "the caller absorbs the halt: {:?}", inspected.result);
    assert_eq!(
        inspected.destroyed, inspector.forwarded,
        "the whole intercepted envelope is destroyed — nothing hands it back",
    );
    assert_eq!(
        inspected.compute_gas,
        inspected.enforced() + inspected.destroyed,
        "and it is reported without being enforced",
    );
}

/// A `create_end` that turns a *successful* contract creation into a failure is honoured — and the
/// state has to follow it.
///
/// This is the rewrite direction there is something behind: the constructor ran, the deposit
/// predicates passed, and the inspector is telling the caller the frame failed. If the journal
/// decision were taken before the callback, the caller would be handed a failure over a deployed
/// contract, with the constructor's storage writes committed underneath it.
#[test]
fn test_killing_a_successful_creation_rolls_its_state_back() {
    // Init code that stores to slot 1 and returns a two-byte runtime code.
    let init_code: Vec<u8> = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(7))
        .push_number(0x6000u64)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(2u64) // size
        .push_number(30u64) // offset: the last two bytes of the word just stored
        .append(RETURN)
        .build()
        .to_vec();

    let code = deploy_then_stop(&init_code);

    let deployed = CONTRACT.create(0);

    // The uninspected run deploys, so the rewrite has something to undo.
    let mut observer = Observer::default();
    let plain =
        transact_inspected(MegaSpecId::REX7, base_db(code.clone()), limits(), &mut observer);
    assert!(plain.result.is_success(), "fixture check: {:?}", plain.result);
    let deployed_account = plain.state.get(&deployed).expect("the fixture must deploy a contract");
    assert!(
        !deployed_account.info.is_empty_code_hash(),
        "the fixture must deploy code for the rewrite to have something to undo",
    );

    let mut killer = CreateKiller::default();
    let killed = transact_inspected(MegaSpecId::REX7, base_db(code), limits(), &mut killer);

    assert_eq!(killer.killed, 1, "the fixture must rewrite exactly one creation");
    assert!(
        killed.state.get(&deployed).is_none_or(|account| account.info.is_empty_code_hash()),
        "a creation the inspector failed must leave no code at {deployed}",
    );
    assert_eq!(
        killed
            .state
            .get(&deployed)
            .and_then(|account| account.storage.get(&U256::from(1)))
            .map(|slot| slot.present_value())
            .unwrap_or_default(),
        U256::ZERO,
        "and none of the constructor's storage writes",
    );
}

/// (iv) An observation-only inspector leaves an empty ledger and a bit-identical transaction.
///
/// This is the property every tracer in production depends on. The comparison is against a run with
/// no inspector attached at all, across every number the transaction reports and the state it
/// produced — not just the ones the ledger touches.
#[test]
fn test_an_observing_inspector_changes_nothing() {
    let callee = plain_run_code(20);
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0x20), U256::from(0x99))
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(CALLEE)
        .push_number(50_000u64)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let build_db = || db_with_callee(code.clone(), callee.clone());

    let plain = transact(MegaSpecId::REX7, build_db(), limits());
    let mut inspector = Observer::default();
    let inspected = transact_inspected(MegaSpecId::REX7, build_db(), limits(), &mut inspector);

    assert!(inspector.steps > 0, "the fixture must actually run opcodes under the inspector");
    assert_eq!(inspector.calls, 2, "one top-level frame plus one inner call");
    assert_eq!(inspector.call_ends, 2, "every call must be paired");

    assert!(
        inspected.inspector_ledger.is_zero(),
        "an observation-only inspector must leave an empty ledger; got {:?}",
        inspected.inspector_ledger,
    );
    assert_eq!(format!("{:?}", inspected.result), format!("{:?}", plain.result));
    assert_eq!(inspected.compute_gas, plain.compute_gas);
    assert_eq!(inspected.enforced(), plain.enforced());
    assert_eq!(inspected.destroyed, plain.destroyed);
    assert_eq!(inspected.data_size, plain.data_size);
    assert_eq!(inspected.kv_updates, plain.kv_updates);
    assert_eq!(inspected.state_growth, plain.state_growth);
    assert_eq!(inspected.gas_used, plain.gas_used);
    assert_eq!(inspected.total_gas_spent, plain.total_gas_spent);
    assert_eq!(inspected.terms, plain.terms);
    assert_eq!(inspected.state, plain.state, "the produced state must be identical");
}

/// A transaction that ran with no inspector at all reports an empty ledger, and the law's `I` term
/// is zero — the shape every consumer of this API sees in practice.
///
/// The stronger property is what the field is *for*: an all-zero ledger is a consumer's guarantee
/// that the gas numbers next to it are the EVM's own, so it has to be exactly zero rather than
/// merely small. A fixture that makes an inner call and writes storage is used, so the assertion
/// covers a transaction with something for a lane to have picked up.
#[test]
fn test_an_uninspected_transaction_reports_an_empty_ledger() {
    let callee = plain_run_code(20);
    let code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(9))
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(CALLEE)
        .push_number(50_000u64)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let db = db_with_callee(code, callee);

    let plain = transact(MegaSpecId::REX7, db, limits());

    assert!(plain.result.is_success(), "fixture check: {:?}", plain.result);
    assert!(
        plain.terms.non_compute_gas > 0,
        "fixture check: the transaction must have moved a lane other than compute",
    );
    assert_eq!(
        plain.inspector_ledger,
        InspectorLedger::default(),
        "no inspector ran, so every lane must be untouched",
    );
    assert_eq!(plain.terms.inspector_conjured_gas, 0, "and the law's inspector term must be zero");
}

// === 2. the two settlement windows ============================================================
//
// The two windows in which a rewrite lands after the accounting that should have read it.
//
// Both halves of the measurement shim rest on the same claim: what the shim books is what the
// transaction's envelope actually moved by. There are two places where the number the shim reads
// and the number the envelope carries are not the same object, and each of them is a fixture
// here.
//
// - **A terminating opcode's `step_end`.** revm's inspected loop runs `step_end` *after* the
//   instruction that produced the frame's action, and that action carries its own copy of the gas
//   counter. An edit to `interp.gas` at that moment changes the counter `MegaETH`'s tail settlement
//   measures work against and nothing the caller will ever see, so it must move the settlement
//   baseline and must not move the ledger. The two neighbouring windows — a `step_end` in
//   mid-frame, and the one after a `CALL` has set a `NewFrame` action — are the boundary of that
//   rule: the frame resumes on the edited counter in both, so both are booked.
//
// - **A precompile's classification.** A precompile is answered inside the frame init and never
//   becomes a child frame, so its recording site is the only place that knows the forwarded
//   envelope and the work performed. The split is nonetheless settled at the frame's settlement
//   point, from what that site staged, exactly as an ordinary frame's is — because a callback runs
//   in between, and the classification is what decides whether the caller reclaims the remainder.
//   What that callback may do to the classification is bounded: the journal decision behind a
//   result frame init produced was taken before any callback ran and is not reachable from one, so
//   a rewrite that moves such a result across the success / revert / halt boundary is refused and
//   the settlement reads the classification the EVM produced. The cases below pin the uninspected
//   split each precompile arm produces, and the refusal that keeps it the one the settlement sees.
//
// Every case here is checked by the identity `common::finish` runs on every transaction: the
// tracker lanes must account for the whole receipt envelope, with the inspector's own term in it.

/// Gas the edit-once inspector writes into a live interpreter's counter.
const INJECT: u64 = 1_000;

/// Gas every probed CALL forwards. Well inside the 63/64 rule at the default transaction gas
/// limit and well inside the default compute budget, so the forwarded envelope is exactly this.
const PROBE_GAS: u64 = 1_000_000;

/// The transaction gas limit is not what binds any fixture here — pinned at compile time, so a
/// change to the shared limit cannot silently turn a destroyed-remainder case into an
/// out-of-gas one.
const _: () = assert!(DEFAULT_TX_GAS_LIMIT > 10 * PROBE_GAS);

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
    call_then_stop(IDENTITY, PROBE_GAS)
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

/// A `CALL` forwarding [`PROBE_GAS`] gas to `target` with `calldata` at `mem[0..]`, its success
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
        .push_number(PROBE_GAS)
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
        plain.destroyed, PROBE_GAS,
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
        PROBE_GAS - kzg_point_evaluation::GAS_COST,
        "verification ran, so the uninspected run destroys the envelope less the fixed fee",
    );
    assert_eq!(
        plain.compute_gas - plain.destroyed,
        plain.enforced(),
        "the fee is the work performed, and it is what enforces",
    );
}

// --- C: the pending action itself ---------------------------------------------------------------

/// Gas an action edit moves, and gas a cancelling pair moves through the result lane's two
/// windows.
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
    call_then_stop(CALLEE, PROBE_GAS)
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

// === 3. the blind spots =======================================================================
//
// The rewrite shapes an all-zero ledger used to admit.
//
// The measurement shim's contract is that a transaction an inspector rewrote never reaches a
// block: the canonical path refuses one whose `InspectorLedger` is non-zero, so every rewrite has
// to leave a mark on it. `measured_inspector.rs` and `inspector_cheat_matrix.rs` pin that per
// mechanism and per callback × shape pair. This module pins the shapes that slipped *between*
// those two questions — each one a rewrite the shim was handed, that changes what the transaction
// produces, and that every lane read as nothing:
//
// - a frame's memory grown for free, by moving the interpreter's memory and the memo of how far it
//   has been paid for in the same step, so that neither goes out of bounds and the next expanding
//   opcode charges nothing;
// - a `CallOutcome` / `CreateOutcome` metadata field — where the callee's return data lands, and
//   which address a creation reports — rewritten without touching the `InterpreterResult` inside
//   it, which is the only part the rewrite comparison used to read;
// - two edits to the *same* signed lane in opposite directions, which a net-only reading cancels to
//   zero;
// - the same cancellation spread across two frames, where only one of the two survives to the
//   receipt, so the net is zero and the effect is not;
// - an instruction deleted from a frame, by stepping the program counter past it, so the work is
//   never performed and there is nothing for any counter to meter;
// - a return buffer put in front of a frame that made no call, so `RETURNDATASIZE` reads a length
//   no call produced.
//
// Four of them are booked on `InspectorLedger::interventions`, from readings the shim did not use
// to take; the cancelling pair are what the per-lane gross activity counters exist for. Every test
// here asserts the ledger the shim books *and* the effect the rewrite had, because a shape that no
// longer changes anything is a shape that stopped testing the guard.//!
// The last two are also why the snapshot the first shape needed is now a *rule* rather than a
// list. A snapshot of four chosen readings caught the memory pair and let the program counter
// through, because `Interpreter::bytecode` was not among the things anyone had thought to name.
// What the shim takes now is every constant-time reading of the interpreter, and what pins that is
// the `Interpreter` row of `gas_surface.rs`'s closed table.

/// A second callee, whose frame reverts.
const REVERTER: Address = EMPTY_TARGET;

/// The address a rewritten `CreateOutcome` reports instead of the one the code was deployed at.
const FAKE_DEPLOYMENT: Address = address!("00000000000000000000000000000000000f00d0");

/// Slot the fixtures write their observable result to.
const RESULT_SLOT: u64 = 0x11;

/// Refund a cancelling pair of refund edits moves, in each direction.
///
/// Small enough to stay well under the EIP-3529 cap on every fixture here, so that what survives
/// to the receipt is the whole of the surviving half rather than whatever the cap left of it.
const REFUND: i64 = 2_000;

/// The mainnet memory expansion cost of a memory `words` words long.
const fn memory_cost(words: u64) -> u64 {
    3 * words + words * words / 512
}

// --- a frame's memory, grown for free ------------------------------------------------------------

/// How far the free-expansion inspector grows the frame's memory, in words.
///
/// The fixture's own `MSTORE` lands inside it, so the expansion the EVM would have charged for is
/// exactly the one the inspector already did for nothing.
const STOLEN_WORDS: u64 = 129;

/// Grows the frame's memory and tells the EVM it is already paid for.
///
/// Both halves are needed and neither is a rewrite on its own. Moving the memory alone leaves the
/// memo behind, and the next expanding opcode charges for an expansion that already happened;
/// moving the memo alone leaves the memory behind, and the EVM reads out of bounds. Moving both
/// keeps every invariant the interpreter has and skips the charge, which is why the pair was the
/// hole and neither half was.
#[derive(Default)]
struct FreeExpansion {
    fired: u32,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for FreeExpansion {
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        if self.fired > 0 || interp.bytecode.opcode() != MSTORE {
            return;
        }
        let words = STOLEN_WORDS as usize;
        assert!(interp.memory.resize(words * 32), "the fixture must allow the memory to be grown",);
        // Priced through revm's own table, so the memo is exactly what the EVM would have written
        // had the frame paid; the assertion below restates the formula independently, which is
        // what makes the two a check rather than one number written twice.
        let cost = context.cfg().gas_params().memory_cost(words);
        interp.gas.memory_mut().set_words_num(words, cost);
        self.fired += 1;
    }
}

/// ★ A frame whose memory was grown for free is not an all-zero ledger.
///
/// The rewrite reaches through no argument the shim used to compare: the interpreter's gas counter
/// is untouched, no action is pending, no frame input and no frame result exists yet. What it
/// moves is the interpreter's memory and the memo beside it, and the transaction then pays less
/// than it would have — which is the one thing the guard exists to keep out of a block.
#[test]
fn test_a_frame_whose_memory_was_grown_for_free_is_booked() {
    // MSTORE(offset = STOLEN_WORDS * 32 - 32, value = 0xAA), which expands memory to exactly the
    // size the inspector already grew it to.
    let offset = (STOLEN_WORDS - 1) * 32;
    let code = BytecodeBuilder::default()
        .push_number(0xAAu64)
        .push_number(offset)
        .append(MSTORE)
        .append(STOP)
        .build();

    let mut inspector = FreeExpansion::default();
    let (plain, cheated) = plain_and_cheated(|| base_db(code.clone()), &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach the expanding opcode exactly once");
    assert!(plain.is_success() && cheated.is_success(), "both runs must succeed");
    assert_eq!(
        plain.total_gas_spent - cheated.total_gas_spent,
        memory_cost(STOLEN_WORDS),
        "the expansion the inspector performed is the charge the EVM then skipped",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction that paid less because an inspector moved its memory must not read as \
         untouched: {:?}",
        cheated.inspector_ledger,
    );
}

// --- a call outcome's metadata -------------------------------------------------------------------

/// Where the fixture's `CALL` asks for its return data, and where the inspector moves it to.
const RETURN_AT: usize = 0;
const MOVED_TO: usize = 32;

/// Moves a finished call's return data somewhere else in the caller's memory.
///
/// The `InterpreterResult` inside the outcome — its classification, its output bytes, its gas —
/// comes back exactly as the EVM produced it. Only the range the caller will copy the output into
/// changes, which is not a field the result carries.
#[derive(Default)]
struct MoveReturnData {
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for MoveReturnData {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address != CALLEE || self.fired > 0 {
            return;
        }
        outcome.memory_offset = MOVED_TO..MOVED_TO + 32;
        self.fired += 1;
    }
}

/// ★ A call outcome whose return range was moved is not an all-zero ledger.
#[test]
fn test_a_moved_return_range_is_booked() {
    // Size the caller's memory to two words, call the callee for one word of output at offset 0,
    // then store what landed there.
    let code = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(32u64)
        .append(MSTORE)
        .push_number(32u64) // retSize
        .push_number(u64::try_from(RETURN_AT).unwrap()) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .push_number(u64::try_from(RETURN_AT).unwrap())
        .append(MLOAD)
        .push_number(RESULT_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build();
    // The callee returns one word of 0x11s.
    let callee = BytecodeBuilder::default()
        .push_u256(U256::from(0x11u64))
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    let db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let mut inspector = MoveReturnData::default();
    let (plain, cheated) = plain_and_cheated(db, &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach `call_end` for the callee once");
    assert_eq!(
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from(0x11u64),
        "without the rewrite the return data lands where the caller asked for it",
    );
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::ZERO,
        "with it, the caller reads a word the callee never wrote",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction whose state a rewritten return range changed must not read as untouched: \
         {:?}",
        cheated.inspector_ledger,
    );
}

// --- a frame's returned output ------------------------------------------------------------------

/// The word a rewritten output buffer feeds the caller instead of the one the callee returned.
const FORGED_OUTPUT: u64 = 0xdead;

/// Replaces the output buffer a finished call hands back, leaving its classification alone.
///
/// The classification and the remaining gas are what every other lane reads. The output is
/// neither, and it is what the caller copies into its own memory.
#[derive(Default)]
struct ForgeCallOutput {
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for ForgeCallOutput {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address != CALLEE || self.fired > 0 {
            return;
        }
        outcome.result.output = Bytes::from(U256::from(FORGED_OUTPUT).to_be_bytes::<32>().to_vec());
        self.fired += 1;
    }
}

/// ★ A call outcome whose returned output was replaced is not an all-zero ledger.
#[test]
fn test_a_forged_call_output_is_booked() {
    // Call the callee for one word of output, then store what landed there.
    let code = BytecodeBuilder::default()
        .push_number(32u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .push_number(0u64)
        .append(MLOAD)
        .push_number(RESULT_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build();
    // The callee returns one word of 0x11s.
    let callee = BytecodeBuilder::default()
        .push_u256(U256::from(0x11u64))
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    let db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let mut inspector = ForgeCallOutput::default();
    let (plain, cheated) = plain_and_cheated(db, &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach `call_end` for the callee once");
    assert_eq!(
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from(0x11u64),
        "without the rewrite the caller reads what the callee returned",
    );
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from(FORGED_OUTPUT),
        "with it, the caller reads a word no frame produced",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction whose state a replaced output buffer changed must not read as untouched: \
         {:?}",
        cheated.inspector_ledger,
    );
}

/// Reports a different address than the one the creation deployed to.
#[derive(Default)]
struct MoveDeploymentAddress {
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for MoveDeploymentAddress {
    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if self.fired > 0 || outcome.address.is_none() {
            return;
        }
        outcome.address = Some(FAKE_DEPLOYMENT);
        self.fired += 1;
    }
}

/// ★ A creation outcome whose reported address was rewritten is not an all-zero ledger.
///
/// The code is still deployed where the EVM put it; only the address the caller's stack receives
/// changes, so the caller goes on to talk to an account that holds nothing.
#[test]
fn test_a_rewritten_deployment_address_is_booked() {
    // Init code that returns two bytes of runtime code.
    let init: [u8; 11] = [0x60, 0x00, 0x60, 0x00, 0x52, 0x60, 0x02, 0x60, 0x1e, 0xf3, 0x00];
    let mut builder = BytecodeBuilder::default();
    for (offset, byte) in init.iter().enumerate() {
        builder = builder.push_number(u64::from(*byte)).push_number(offset as u64).append(MSTORE8);
    }
    let code = builder
        .push_number(init.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .push_number(RESULT_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build();

    let mut inspector = MoveDeploymentAddress::default();
    let (plain, cheated) = plain_and_cheated(|| base_db(code.clone()), &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach `create_end` once");
    let deployed = plain.storage_value(CONTRACT, U256::from(RESULT_SLOT));
    assert_ne!(deployed, U256::ZERO, "the fixture's CREATE must succeed");
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from_be_slice(FAKE_DEPLOYMENT.as_slice()),
        "the caller must have been handed the address the inspector wrote",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction told a contract lives somewhere it does not must not read as untouched: \
         {:?}",
        cheated.inspector_ledger,
    );
}

// --- a construction frame's pending action -------------------------------------------------------

/// Drains the gas a construction frame's pending `Return` action carries.
///
/// The contract this module's other cases rest on — that an action is the frame's result a moment
/// later, so an edit to it settles with that result — does not hold for a creation. Between the
/// two, `classify_frame_action` charges the code deposit out of the gas *this action* carries, and
/// a creation that cannot pay it becomes an `OutOfGas` that deploys nothing. So this edit changes
/// what the transaction produces, and it does it by a route that leaves the classification and the
/// output the boundary compares exactly where they were.
#[derive(Default)]
struct DrainConstructionAction {
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for DrainConstructionAction {
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        // A construction frame runs no deployed code, so it has no bytecode address.
        if self.fired > 0 || interp.input.bytecode_address().is_some() {
            return;
        }
        let Some(InterpreterAction::Return(result)) = interp.bytecode.action() else {
            return;
        };
        if !result.result.is_ok() {
            return;
        }
        let remaining = result.gas.remaining();
        assert!(
            result.gas.record_regular_cost(remaining),
            "the fixture must be able to drain the action it found",
        );
        self.fired += 1;
    }
}

/// ★ A construction frame whose pending action was drained is not an all-zero ledger.
///
/// Every lane the boundary reads stays put: the action's classification and output are untouched,
/// so nothing is an intervention; the gas edit is staged for the frame's settlement point, and
/// that point declines to book it because the result it finally sees is a swallowed one. The
/// deposit the drained action could no longer pay is what turned it into one.
#[test]
fn test_a_drained_construction_action_is_booked() {
    // Init code that returns two bytes of runtime code.
    let init: [u8; 11] = [0x60, 0x00, 0x60, 0x00, 0x52, 0x60, 0x02, 0x60, 0x1e, 0xf3, 0x00];
    let mut builder = BytecodeBuilder::default();
    for (offset, byte) in init.iter().enumerate() {
        builder = builder.push_number(u64::from(*byte)).push_number(offset as u64).append(MSTORE8);
    }
    let code = builder
        .push_number(init.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .push_number(RESULT_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build();

    let mut inspector = DrainConstructionAction::default();
    let (plain, cheated) = plain_and_cheated(|| base_db(code.clone()), &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach the construction frame's step_end once");
    assert_ne!(
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::ZERO,
        "without the edit the creation must succeed",
    );
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::ZERO,
        "with it the creation cannot pay its code deposit and deploys nothing",
    );
    assert_ne!(
        plain.gas_used, cheated.gas_used,
        "and the receipt the sender is billed on moves with it",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction whose contract an inspector deleted must not read as untouched: {:?}",
        cheated.inspector_ledger,
    );
}

/// Raises the gas an inner call frame's pending `Return` action carries, then takes the same
/// amount back out of the result that action became.
///
/// The two windows are one lane and one frame, and the pair nets to zero. They are still two
/// edits, made in two different callbacks, and the lane's traffic is what says so — the sum alone
/// reads as an inspector that did nothing.
#[derive(Default)]
struct CancellingActionAndResultEdits {
    raised: u32,
    lowered: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CancellingActionAndResultEdits {
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.raised > 0 || interp.input.bytecode_address() != Some(&CALLEE) {
            return;
        }
        let Some(InterpreterAction::Return(result)) = interp.bytecode.action() else {
            return;
        };
        if !result.result.is_ok() {
            return;
        }
        result.gas.erase_cost(ACTION_DELTA);
        self.raised += 1;
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if self.lowered > 0 || inputs.target_address != CALLEE {
            return;
        }
        assert!(
            outcome.result.gas.record_regular_cost(ACTION_DELTA),
            "the fixture must leave the result enough gas for the removal to land",
        );
        self.lowered += 1;
    }
}

/// ★ An edit staged at one callback and undone at the next is two edits, not none.
///
/// Nothing about this transaction changes: a call frame's remaining gas is read by nobody between
/// the two windows, so the pair really is invisible in what the transaction produces. That is the
/// point — the lane's traffic is the only thing that separates it from an inspector that never
/// ran, and on a *creation* frame the same pair is the shape that deletes a contract.
#[test]
fn test_cancelling_action_and_result_edits_are_booked() {
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let callee = BytecodeBuilder::default().append(STOP).build();
    let db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let mut inspector = CancellingActionAndResultEdits::default();
    let (plain, cheated) = plain_and_cheated(db, &mut inspector);

    assert_eq!((inspector.raised, inspector.lowered), (1, 1), "both windows must be reached");
    assert_eq!(
        cheated.gas_used, plain.gas_used,
        "the pair cancels, so the receipt really is the one the EVM would have produced",
    );
    assert_eq!(
        cheated.inspector_ledger.conjured_gas(),
        0,
        "and the conservation law must read the net, which is zero",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "but the guard must still see that the lane carried two edits: {:?}",
        cheated.inspector_ledger,
    );
    assert_eq!(
        cheated.inspector_ledger.result.gross(),
        2 * u128::from(ACTION_DELTA),
        "one edit in each window, counted where each was made",
    );
}

// --- two edits to one lane, in opposite directions -----------------------------------------------

/// Injects one gas before the frame reads its own remaining gas, and takes it back afterwards.
///
/// Both edits land on the interpreter counter, which is one signed lane. Their net is zero and
/// the transaction's envelope is unmoved — and in between them the frame read a number one higher
/// than the EVM would have given it, and wrote that number to storage.
#[derive(Default)]
struct CancellingCounterEdits {
    /// 0 before the injection, 1 between the two edits, 2 once both have landed.
    phase: u8,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CancellingCounterEdits {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        match self.phase {
            0 if interp.bytecode.opcode() == GAS => {
                interp.gas.erase_cost(1);
                self.phase = 1;
            }
            1 => {
                assert!(interp.gas.record_regular_cost(1), "the frame must afford the give-back");
                self.phase = 2;
            }
            _ => {}
        }
    }
}

/// ★ Two edits to the same lane that cancel are not an all-zero ledger.
///
/// The net of the gas lane really is zero — the transaction spent exactly what it would have — so
/// nothing the conservation law reads has moved. What moved is the number the frame read in
/// between, and a guard that asks the net cannot see it. The gross activity counter is what does.
#[test]
fn test_cancelling_counter_edits_are_booked() {
    let code = BytecodeBuilder::default()
        .append(GAS)
        .push_number(RESULT_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build();

    // No compute-gas limit, so the REX7 gas clamp hides nothing and the frame's own reading of
    // its remaining gas is the counter the injection moved.
    let limits = EvmTxRuntimeLimits::no_limits();
    let plain = transact(MegaSpecId::REX7, base_db(code.clone()), limits);
    let mut inspector = CancellingCounterEdits::default();
    let cheated = transact_inspected(MegaSpecId::REX7, base_db(code), limits, &mut inspector);

    assert_eq!(inspector.phase, 2, "both halves of the cancellation must have landed");
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)) + U256::from(1),
        "the frame must have read one gas more than the EVM would have given it",
    );
    assert_eq!(
        cheated.total_gas_spent, plain.total_gas_spent,
        "the two edits cancel, so the envelope the receipt reports is unmoved",
    );
    assert_eq!(
        cheated.inspector_conjured_gas(),
        0,
        "and so is the law's term: this is exactly the shape a net-only reading cannot see",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "but the transaction was rewritten, and the guard has to see that: {:?}",
        cheated.inspector_ledger,
    );
}

/// Adds a refund to one child frame's result and takes the same amount out of another's.
///
/// The frame that gets the addition returns, so its refund reaches the receipt. The frame that
/// gets the subtraction reverts, so revm discards its whole refund counter — the subtraction never
/// reaches anything. Net zero on the lane, one refund's worth of difference on the receipt.
#[derive(Default)]
struct CancellingRefundsAcrossFrames {
    added: u32,
    removed: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CancellingRefundsAcrossFrames {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == CALLEE && self.added == 0 {
            outcome.result.gas.record_refund(REFUND);
            self.added += 1;
        } else if inputs.target_address == REVERTER && self.removed == 0 {
            assert!(
                outcome.result.gas.refunded() >= REFUND,
                "the reverting callee must hold a refund of its own to take from, got {}",
                outcome.result.gas.refunded(),
            );
            outcome.result.gas.record_refund(-REFUND);
            self.removed += 1;
        }
    }
}

/// ★ A cancellation split across a surviving frame and a discarded one is not an all-zero ledger.
///
/// This is the previous shape with the asymmetry made explicit: the two halves are equal and
/// opposite where the ledger books them, and only one of them is still standing by the time the
/// receipt is built.
#[test]
fn test_cancelling_refunds_across_frames_are_booked() {
    let call_to = |builder: BytecodeBuilder, target: Address| {
        builder
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_address(target)
            .push_number(200_000u64)
            .append(CALL)
            .append(POP)
    };
    let code = call_to(call_to(BytecodeBuilder::default(), CALLEE), REVERTER).append(STOP).build();
    // Both callees set a slot and clear it again, so each ends holding a refund the EVM produced.
    let clearing = |builder: BytecodeBuilder| {
        builder
            .sstore(U256::from(RESULT_SLOT), U256::from(1u64))
            .sstore(U256::from(RESULT_SLOT), U256::ZERO)
    };
    let returning = clearing(BytecodeBuilder::default()).append(STOP).build();
    let reverting = clearing(BytecodeBuilder::default()).revert().build();
    let db = || {
        base_db(code.clone())
            .account_code(CALLEE, returning.clone())
            .account_code(REVERTER, reverting.clone())
    };

    let mut inspector = CancellingRefundsAcrossFrames::default();
    let (plain, cheated) = plain_and_cheated(db, &mut inspector);

    assert_eq!((inspector.added, inspector.removed), (1, 1), "both halves must have landed");
    assert!(
        plain.total_gas_spent >= 5 * u64::try_from(REFUND).unwrap(),
        "the fixture must burn enough that the EIP-3529 cap does not hide the difference",
    );
    assert_eq!(
        plain.gas_used - cheated.gas_used,
        u64::try_from(REFUND).unwrap(),
        "only the surviving frame's half reaches the receipt, so the sender pays that much less",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a receipt an inspector moved must not read as untouched: {:?}",
        cheated.inspector_ledger,
    );
}

// --- an opcode skipped, and a return buffer conjured
// ----------------------------------------------

/// What the fixture's `SSTORE` writes when it runs.
const STORED: u64 = 0x99;

/// The gas a cold `SSTORE` into a zero slot costs, which is what skipping it saves.
const COLD_SSTORE_SET: u64 = 22_100;

/// How many bytes of return data the forging inspector conjures.
///
/// Non-zero and a whole number of words, so that the `SSTORE` that stores it turns a zero slot
/// into a non-zero one — which is a different charge as well as a different value.
const CONJURED_RETURN_DATA: u64 = 96;

/// Advances the program counter past the frame's `SSTORE`, so the EVM never executes it.
///
/// revm's inspected loop runs this callback *before* the instruction, and the interpreter reads
/// the opcode it is about to execute from the very pointer this moves. Stepping the pointer on by
/// one byte therefore deletes one instruction from the frame: the two operands the `SSTORE` would
/// have consumed stay on the stack, the `STOP` after it runs instead, and the frame ends where it
/// was going to end.
///
/// Nothing about this reaches a gas counter. The work is not performed, so there is nothing for
/// the EVM to meter and nothing for a gas lane to see.
#[derive(Default)]
struct SkipTheStore {
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipTheStore {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.fired > 0 || interp.bytecode.opcode() != SSTORE {
            return;
        }
        interp.bytecode.relative_jump(1);
        self.fired += 1;
    }
}

/// ★ A frame with an opcode skipped out from under it is not an all-zero ledger.
///
/// The rewrite is the free-expansion shape's twin and is strictly worse: it does not merely make
/// the frame's next charge cheaper, it deletes an instruction from the frame. The transaction ends
/// with different storage *and* a smaller bill, and every gas lane reads zero because the gas that
/// went missing was never spent by anybody.
#[test]
fn test_a_skipped_opcode_is_booked() {
    let code = BytecodeBuilder::default()
        .sstore(U256::from(RESULT_SLOT), U256::from(STORED))
        .append(STOP)
        .build();

    let mut inspector = SkipTheStore::default();
    let (plain, cheated) = plain_and_cheated(|| base_db(code.clone()), &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach the store exactly once");
    assert!(plain.is_success() && cheated.is_success(), "both runs must succeed");
    assert_eq!(
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from(STORED),
        "without the rewrite the frame stores what its bytecode says",
    );
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::ZERO,
        "with it, the store never happens",
    );
    assert_eq!(
        plain.total_gas_spent - cheated.total_gas_spent,
        COLD_SSTORE_SET,
        "the deleted instruction is the charge the transaction then did not pay",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction an inspector deleted an instruction from must not read as untouched: {:?}",
        cheated.inspector_ledger,
    );
}

/// Puts return data in front of a frame that has made no call.
///
/// `RETURNDATASIZE` reads the buffer's length, so the frame goes on to store a number no call
/// produced. The buffer is the interpreter's own, reachable through `ReturnData` on any live
/// interpreter, and its length is a constant-time reading exactly like the memory's size.
#[derive(Default)]
struct ForgeReturnData {
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for ForgeReturnData {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.fired > 0 || interp.bytecode.opcode() != RETURNDATASIZE {
            return;
        }
        interp.return_data.set_buffer(Bytes::from(vec![0u8; CONJURED_RETURN_DATA as usize]));
        self.fired += 1;
    }
}

/// ★ A frame handed return data it never received is not an all-zero ledger.
///
/// The frame made no call, so the EVM's own buffer is empty and the store is a zero-to-zero
/// no-op. With the rewrite the same store turns a zero slot into a non-zero one, which changes the
/// post-state and costs the transaction more — in the opposite direction to every other shape
/// here, and just as invisible to a lane that only watches gas counters.
#[test]
fn test_a_forged_return_buffer_is_booked() {
    let code = BytecodeBuilder::default()
        .append(RETURNDATASIZE)
        .push_number(RESULT_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build();

    let mut inspector = ForgeReturnData::default();
    let (plain, cheated) = plain_and_cheated(|| base_db(code.clone()), &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach the read exactly once");
    assert!(plain.is_success() && cheated.is_success(), "both runs must succeed");
    assert_eq!(
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::ZERO,
        "a frame that made no call has no return data",
    );
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from(CONJURED_RETURN_DATA),
        "with the rewrite it reads the length of a buffer no call produced",
    );
    assert!(
        cheated.total_gas_spent > plain.total_gas_spent,
        "and pays for the non-zero store the rewrite turned it into: {} vs {}",
        cheated.total_gas_spent,
        plain.total_gas_spent,
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction whose state a forged buffer changed must not read as untouched: {:?}",
        cheated.inspector_ledger,
    );
}

// --- a frame invariant moved and moved back --------------------------------------------------

/// The caller the rewriting inspector shows the frame instead of the one that called it.
const IMPOSTOR: Address = address!("00000000000000000000000000000000000ca11e");

/// Moves the frame's caller for the length of one instruction, and puts it back.
///
/// `CALLER` reads `input.caller_address`, so the frame pushes an address nobody called it from and
/// goes on to store that. The rewrite is undone in the very next callback, which is what makes the
/// shape worth pinning: the frame's identity is the one the EVM gave it at every point a *frame*
/// could be inspected — at its start, at its end, and at every callback but the two this touches.
///
/// Nothing about it reaches a gas counter. Both runs execute the same instructions and pay the
/// same cold `SSTORE`; only the value written differs.
#[derive(Default)]
struct BorrowTheCaller {
    /// The caller the EVM gave the frame, kept so it can be handed back.
    original: Option<Address>,
    /// How many times each half of the rewrite ran.
    moved: u32,
    restored: u32,
}

impl<CTX> Inspector<CTX, EthInterpreter> for BorrowTheCaller {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        if self.moved > 0 || interp.bytecode.opcode() != CALLER {
            return;
        }
        self.original = Some(interp.input.caller_address);
        interp.input.caller_address = IMPOSTOR;
        self.moved += 1;
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        let Some(original) = self.original.filter(|_| self.restored == 0) else {
            return;
        };
        interp.input.caller_address = original;
        self.restored += 1;
    }
}

/// ★ A frame invariant moved in `step` and moved back in `step_end` is not an all-zero ledger.
///
/// The four addresses and the value a frame is identified by cannot change while it runs, which
/// makes them the readings a cheaper shim would be tempted to compare once per frame rather than
/// once per callback. This is the shape that answers that: an inspector borrows one of them for
/// exactly as long as it takes the frame to read it, and gives it back before anything outside the
/// two callbacks could look. A per-frame comparison sees the address it started with; a per-opcode
/// one sees it move twice.
#[test]
fn test_a_frame_invariant_moved_and_moved_back_is_booked() {
    let code = BytecodeBuilder::default()
        .append(CALLER)
        .push_number(RESULT_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build();

    let mut inspector = BorrowTheCaller::default();
    let (plain, cheated) = plain_and_cheated(|| base_db(code.clone()), &mut inspector);

    assert_eq!((inspector.moved, inspector.restored), (1, 1), "both halves must run once");
    assert_eq!(
        inspector.original,
        Some(crate::common::CALLER),
        "and the half that gives the address back must have the one the EVM gave the frame",
    );
    assert!(plain.is_success() && cheated.is_success(), "both runs must succeed");
    assert_eq!(
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from_be_slice(crate::common::CALLER.as_slice()),
        "without the rewrite the frame stores the address that called it",
    );
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from_be_slice(IMPOSTOR.as_slice()),
        "with it, the frame stores one nobody called it from",
    );
    assert_eq!(
        plain.total_gas_spent, cheated.total_gas_spent,
        "the two runs cost the same, so no gas lane can tell them apart",
    );
    assert!(
        cheated.inspector_ledger.interventions >= 2,
        "each half of the rewrite is a rewrite: {:?}",
        cheated.inspector_ledger,
    );
}

// === 4. the receipt's other two numbers =======================================================
//
// The two numbers on a receipt that the conservation law cannot see, and the lanes that do.
//
// The law is stated over `total_gas_spent`, which is `limit - remaining`. A transaction's receipt
// carries two more figures that arithmetic does not reach: the EIP-3529 refund, which decides what
// the sender actually pays, and the EIP-8037 state-gas dimension — a `Gas`'s `reservoir` and its
// `state_gas_spent` counter — which decides how much of the envelope the receipt counts as spent
// at all.
//
// Both are reachable from every callback that is handed a `Gas`, and both were unmeasured. The
// shapes here are what the two lanes now book, and each pins the *reason* its lane is measured
// where it is:
//
// - a **refund** is a quantity the EVM also produces, so only a difference across a callback
//   isolates the inspector's share — the lane is measured at the boundary, and is nominal in both
//   the senses that can make it differ from what reaches the receipt (the EIP-3529 cap, and the
//   chain of successful frame returns an edit has to survive);
// - a **reservoir** is a quantity `MegaETH` never produces at all, and one revm propagates by
//   replacement rather than by accumulation, so a boundary difference would book edits the EVM goes
//   on to erase. The lane is settled once, from the number the transaction ends with, which is
//   exactly the surviving part and is the inspector's in whole.

/// Gas the fixture's inner `CALL` forwards.
const INNER_CALL_GAS: u64 = 200_000;

/// A refund large enough that the cap keeps part of it out of the receipt.
const OVERSIZED_REFUND: i64 = 60_000;
/// The EIP-8037 pool an edit fills.
const RESERVOIR: u64 = 10_000;
/// The EIP-8037 spend an edit writes.
const STATE_GAS: i64 = 5_000;

/// Slot the top frame writes.
const TOP_SLOT: u64 = 0x10;
/// Slot the callee writes.
const CALLEE_SLOT: u64 = 0x20;
/// Slot the callee sets and clears, so the frame ends holding a refund of the EVM's own making.
const CLEARED_SLOT: u64 = 0x30;

// --- the fixture -------------------------------------------------------------------------------

/// How the fixture's callee ends, which is what decides whether its refund travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Callee {
    /// Writes storage, produces a refund by clearing a slot it just set, and returns.
    Returning,
    /// Writes storage and reverts, so the EVM discards everything the frame held.
    Reverting,
}

fn caller_code() -> Bytes {
    append_call(BytecodeBuilder::default(), CALLEE, INNER_CALL_GAS, 0)
        .append(POP)
        .sstore(U256::from(TOP_SLOT), U256::from(1u64))
        .append(STOP)
        .build()
}

fn callee_code(callee: Callee) -> Bytes {
    let builder = BytecodeBuilder::default()
        .sstore(U256::from(CALLEE_SLOT), U256::from(1u64))
        // Set and clear, so the frame ends holding a refund the EVM itself produced.
        .sstore(U256::from(CLEARED_SLOT), U256::from(1u64))
        .sstore(U256::from(CLEARED_SLOT), U256::ZERO);
    match callee {
        Callee::Returning => builder.append(STOP).build(),
        Callee::Reverting => builder.revert().build(),
    }
}

fn db_for(callee: Callee) -> MemoryDatabase {
    db_with_callee(caller_code(), callee_code(callee))
}

// --- the edit ----------------------------------------------------------------------------------

/// One edit, applied once, to one of the `Gas` objects a callback is handed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    /// Add to the running interpreter's refund counter.
    RefundAtStep(i64),
    /// Add to the finished inner call's refund counter.
    RefundAtCallEnd(i64),
    /// Fill the running interpreter's EIP-8037 pool.
    ReservoirAtStep,
    /// Fill it at the one moment the frame is holding a `NewFrame` action, whose child overwrites
    /// the pool on the way back.
    ReservoirAtSuspension,
    /// Fill the pool the inner call's inputs seed the child frame with.
    ReservoirOnInputs,
    /// Fill the finished inner call's pool.
    ReservoirAtCallEnd,
    /// Write the running interpreter's EIP-8037 spend counter.
    StateGasAtStep,
    /// Write the finished inner call's spend counter.
    StateGasAtCallEnd,
    /// Answer the inner call with a synthetic outcome that echoes the envelope and carries
    /// neither figure — the control the two below are read against.
    InterceptEcho,
    /// The same, carrying a refund the frame never earned.
    InterceptWithRefund,
    /// The same, carrying an EIP-8037 pool.
    InterceptWithReservoir,
}

impl Edit {
    /// Whether this edit answers the frame itself instead of letting the EVM build it.
    const fn intercepts(self) -> bool {
        matches!(
            self,
            Self::InterceptEcho | Self::InterceptWithRefund | Self::InterceptWithReservoir
        )
    }
}

/// Applies one [`Edit`], once, and records that it landed.
#[derive(Debug)]
struct Editor {
    edit: Edit,
    fired: u32,
    steps: u64,
}

impl Editor {
    const fn new(edit: Edit) -> Self {
        Self { edit, fired: 0, steps: 0 }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Editor {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.steps += 1;
        if self.fired > 0 || self.steps != 4 {
            return;
        }
        match self.edit {
            Edit::RefundAtStep(amount) => interp.gas.record_refund(amount),
            Edit::ReservoirAtStep => interp.gas.set_reservoir(RESERVOIR),
            Edit::StateGasAtStep => interp.gas.set_state_gas_spent(STATE_GAS),
            _ => return,
        }
        self.fired += 1;
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.fired > 0 || self.edit != Edit::ReservoirAtSuspension {
            return;
        }
        // The one window where the pool the frame holds is not the pool that travels: the child
        // this action builds was already sized from the pre-edit value, and its own pool
        // overwrites this one when it returns.
        if !matches!(interp.bytecode.action(), Some(InterpreterAction::NewFrame(_))) {
            return;
        }
        interp.gas.set_reservoir(RESERVOIR);
        self.fired += 1;
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if self.fired > 0 || inputs.target_address != CALLEE {
            return None;
        }
        if self.edit == Edit::ReservoirOnInputs {
            inputs.reservoir += RESERVOIR;
            self.fired += 1;
            return None;
        }
        if !self.edit.intercepts() {
            return None;
        }
        // The echo convention every tool that intercepts follows: hand back exactly what was
        // forwarded, so the gas lanes see nothing and only the figures under test move.
        let mut gas = Gas::new(inputs.gas_limit);
        match self.edit {
            Edit::InterceptWithRefund => gas.record_refund(REFUND),
            Edit::InterceptWithReservoir => gas.set_reservoir(RESERVOIR),
            _ => {}
        }
        self.fired += 1;
        Some(CallOutcome::new(
            InterpreterResult::new(InstructionResult::Stop, Bytes::new(), gas),
            inputs.return_memory_offset.clone(),
        ))
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if self.fired > 0 || inputs.target_address != CALLEE {
            return;
        }
        match self.edit {
            Edit::RefundAtCallEnd(amount) => outcome.result.gas.record_refund(amount),
            Edit::ReservoirAtCallEnd => outcome.result.gas.set_reservoir(RESERVOIR),
            Edit::StateGasAtCallEnd => outcome.result.gas.set_state_gas_spent(STATE_GAS),
            _ => return,
        }
        self.fired += 1;
    }
}

/// Runs the fixture with no inspector at all.
fn transact_plain(callee: Callee) -> Outcome {
    transact(MegaSpecId::REX7, db_for(callee), limits())
}

/// Runs it with one edit applied, asserting the edit landed exactly once.
fn transact_edited(callee: Callee, edit: Edit) -> Outcome {
    let mut editor = Editor::new(edit);
    let outcome = transact_inspected(MegaSpecId::REX7, db_for(callee), limits(), &mut editor);
    assert_eq!(
        editor.fired, 1,
        "{edit:?}: the fixture must reach the edit's callback exactly once",
    );
    outcome
}

// --- the fixture's own assumptions ---------------------------------------------------------------

/// The uninspected run is what the cells below assume it is: it succeeds, it produces a refund of
/// its own, and it reports no EIP-8037 dimension at all.
#[test]
fn test_the_fixture_refunds_on_its_own_and_holds_no_state_gas() {
    let plain = transact_plain(Callee::Returning);
    assert!(plain.result.is_success(), "{:?}", plain.result);
    assert!(
        plain.refunded() > 0,
        "the callee's cleared slot must leave a refund for the lowering cell to take from",
    );
    assert_eq!(
        plain.gas_used,
        plain.total_gas_spent - plain.refunded(),
        "the receipt's two gas numbers differ by exactly the refund",
    );
    assert_eq!(plain.state_gas_spent(), 0, "EIP-8037 is off on every MegaETH path");
    assert!(plain.inspector_ledger.is_zero(), "no inspector ran: {:?}", plain.inspector_ledger);
}

// --- the refund lane
// ------------------------------------------------------------------------------

/// A refund written into a running interpreter's counter is booked, and moves what the sender pays
/// without moving the envelope.
#[test]
fn test_a_refund_written_into_a_live_interpreter_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::RefundAtStep(REFUND));

    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger { refund: Lane::once(i128::from(REFUND)), ..InspectorLedger::default() },
        "the shim must book the refund and nothing else",
    );
    assert_eq!(
        edited.total_gas_spent, plain.total_gas_spent,
        "a refund does not move the envelope, which is why the law cannot see it",
    );
    assert_eq!(
        edited.refunded(),
        plain.refunded() + u64::try_from(REFUND).unwrap(),
        "but it does move the receipt's refund",
    );
    assert_eq!(
        edited.gas_used,
        plain.gas_used - u64::try_from(REFUND).unwrap(),
        "and through it what the sender pays",
    );
    assert_eq!(
        edited.terms.inspector_conjured_gas, 0,
        "the refund lane is deliberately not a term of the law",
    );
    assert!(!edited.inspector_ledger.is_zero(), "and the block guard has to see it");
}

/// The same edit made at the last callback that holds the finished frame's result.
#[test]
fn test_a_refund_written_into_a_finished_frame_result_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::RefundAtCallEnd(REFUND));

    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger { refund: Lane::once(i128::from(REFUND)), ..InspectorLedger::default() },
    );
    assert_eq!(edited.refunded(), plain.refunded() + u64::try_from(REFUND).unwrap());
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent);
}

/// A refund taken *out* is booked with the sign that says so — a lane that only saw one direction
/// would report an inspector that raised the sender's bill as having done nothing.
#[test]
fn test_a_refund_taken_out_of_a_frame_is_booked_with_the_sign_that_says_so() {
    let plain = transact_plain(Callee::Returning);
    assert!(
        plain.refunded() >= u64::try_from(REFUND).unwrap(),
        "fixture check: there must be a refund to take from, got {}",
        plain.refunded(),
    );

    let edited = transact_edited(Callee::Returning, Edit::RefundAtCallEnd(-REFUND));
    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger { refund: Lane::once(-i128::from(REFUND)), ..InspectorLedger::default() },
    );
    assert_eq!(edited.refunded(), plain.refunded() - u64::try_from(REFUND).unwrap());
    assert_eq!(
        edited.gas_used,
        plain.gas_used + u64::try_from(REFUND).unwrap(),
        "the sender pays more, by exactly what was taken",
    );
}

/// The lane reports what the inspector wrote, not what the EIP-3529 cap let through.
///
/// The cap applies to the transaction's whole refund at once, over a sum in which the EVM's own
/// refunds and an inspector's are indistinguishable, at a point past every callback. Splitting it
/// between them needs a priority rule the protocol does not have, so the lane states the edit and
/// the receipt states the effect — and the two are allowed to differ.
#[test]
fn test_the_refund_lane_reports_what_was_written_not_what_the_cap_let_through() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::RefundAtStep(OVERSIZED_REFUND));

    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            refund: Lane::once(i128::from(OVERSIZED_REFUND)),
            ..InspectorLedger::default()
        },
        "the lane carries the nominal edit",
    );
    assert_eq!(
        edited.refunded(),
        edited.total_gas_spent / 5,
        "while the receipt carries the EIP-3529 cap",
    );
    assert!(
        edited.refunded() < plain.refunded() + u64::try_from(OVERSIZED_REFUND).unwrap(),
        "fixture check: the cap must actually bind, or this cell asserts nothing",
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent, "the envelope is untouched");
}

/// A refund written into a frame the EVM then fails is booked too, even though it reaches nothing.
///
/// revm hands a frame's refund to its caller only on success, so this edit dies with the frame.
/// The lane books it anyway, because the alternative is a rule that has to track every frame
/// between the edit and the top — and because a lane that under-reports lets exactly the shape
/// this module exists to catch into a block, while over-reporting costs nothing: the law has no
/// term for it.
#[test]
fn test_a_refund_the_frame_chain_discards_is_still_booked() {
    let plain = transact_plain(Callee::Reverting);
    let edited = transact_edited(Callee::Reverting, Edit::RefundAtCallEnd(REFUND));

    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger { refund: Lane::once(i128::from(REFUND)), ..InspectorLedger::default() },
        "the lane books the edit",
    );
    assert_eq!(
        edited.refunded(),
        plain.refunded(),
        "the receipt is unmoved: a reverting frame hands its caller no refund",
    );
    assert_eq!(edited.gas_used, plain.gas_used);
}

// --- the EIP-8037 state-gas dimension ------------------------------------------------------------

/// A reservoir an inspector fills is gas the transaction never funded: the receipt reports that
/// much less spent, and the law needs it back.
#[test]
fn test_a_reservoir_written_into_a_live_interpreter_is_booked_and_the_law_closes() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::ReservoirAtStep);

    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            reservoir: Lane::once(i128::from(RESERVOIR)),
            ..InspectorLedger::default()
        },
    );
    assert_eq!(
        edited.total_gas_spent,
        plain.total_gas_spent - RESERVOIR,
        "the receipt counts the pool as unspent, so the envelope shrinks by exactly it",
    );
    assert_eq!(
        edited.terms.inspector_conjured_gas,
        i128::from(RESERVOIR),
        "which is why this lane, unlike the refund one, is a term of the law",
    );
}

/// The same, written into the pool a call's inputs seed the child frame with.
#[test]
fn test_a_reservoir_written_into_a_frame_input_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::ReservoirOnInputs);

    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            reservoir: Lane::once(i128::from(RESERVOIR)),
            // The inputs came back changed in a field the envelope lane does not cover, which the
            // rewrite comparison books on its own.
            interventions: 1,
            ..InspectorLedger::default()
        },
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent - RESERVOIR);
}

/// And into the finished frame's own pool, which its caller takes whatever the classification.
#[test]
fn test_a_reservoir_written_into_a_finished_frame_result_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::ReservoirAtCallEnd);

    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            reservoir: Lane::once(i128::from(RESERVOIR)),
            ..InspectorLedger::default()
        },
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent - RESERVOIR);
}

/// A reservoir edit the EVM overwrites books nothing — and there is nothing to book, because the
/// run it produces is the run the EVM would have produced alone.
///
/// This is the window that decides where the lane is measured. A difference taken across this
/// callback would say `RESERVOIR` was conjured; the transaction says otherwise, and the settlement
/// point is the only reading that agrees with it.
#[test]
fn test_a_reservoir_edit_the_evm_overwrites_books_nothing() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::ReservoirAtSuspension);

    assert!(
        edited.inspector_ledger.is_zero(),
        "an edit the child frame's own pool replaces moved nothing: {:?}",
        edited.inspector_ledger,
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent);
    assert_eq!(edited.gas_used, plain.gas_used);
    assert_eq!(edited.refunded(), plain.refunded());
}

/// The spend counter's own effect on the receipt: a successful transaction reports it, whether or
/// not EIP-8037 is enabled.
#[test]
fn test_state_gas_written_into_a_live_interpreter_reaches_the_receipt_and_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::StateGasAtStep);

    assert_eq!(plain.state_gas_spent(), 0, "fixture check");
    assert_eq!(
        edited.state_gas_spent(),
        u64::try_from(STATE_GAS).unwrap(),
        "the receipt reports what was written",
    );
    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            state_gas: Lane::once(i128::from(STATE_GAS)),
            ..InspectorLedger::default()
        },
    );
    assert_eq!(
        edited.total_gas_spent, plain.total_gas_spent,
        "the envelope is untouched, so this lane is not a term of the law either",
    );
    assert_eq!(edited.terms.inspector_conjured_gas, 0);
}

/// The counter's *other* effect, at a site no callback sees: a frame that fails folds its spend
/// counter back into its caller's pool, which turns a state-gas edit into an envelope-moving one.
///
/// The lane that catches it is the reservoir's, not the state-gas one, because the fold has
/// already happened by the time either is read. That is the second reason the two are settled from
/// the transaction's final figures rather than differenced across a callback.
#[test]
fn test_state_gas_on_a_failing_frame_becomes_its_callers_reservoir() {
    let plain = transact_plain(Callee::Reverting);
    let edited = transact_edited(Callee::Reverting, Edit::StateGasAtCallEnd);

    assert_eq!(
        edited.inspector_ledger,
        InspectorLedger {
            reservoir: Lane::once(i128::from(STATE_GAS)),
            ..InspectorLedger::default()
        },
        "the spend counter of a reverting frame arrives in its caller as a pool",
    );
    assert_eq!(
        edited.state_gas_spent(),
        0,
        "and not as a spend: a failing frame's counter is not accumulated",
    );
    assert_eq!(
        edited.total_gas_spent,
        plain.total_gas_spent - u64::try_from(STATE_GAS).unwrap(),
        "so the envelope moves, and the law's term has to move with it",
    );
}

// --- a frame the inspector answers itself
// ---------------------------------------------------------

/// A synthetic outcome carries figures of its own, and there is no EVM-produced number on the
/// other side of the callback to difference against — so the whole of what it carries is the
/// inspector's, measured against nothing rather than against a baseline.
///
/// The echo control is what makes the two cells below readings of the figures rather than of the
/// interception: it moves the gas lanes not at all, which is the convention every tool that
/// intercepts follows.
#[test]
fn test_a_synthetic_outcome_carries_its_own_figures() {
    let echo = transact_edited(Callee::Returning, Edit::InterceptEcho);
    assert_eq!(
        echo.inspector_ledger,
        InspectorLedger { interventions: 1, ..InspectorLedger::default() },
        "an echoing interception moves no figure at all",
    );

    let refunding = transact_edited(Callee::Returning, Edit::InterceptWithRefund);
    assert_eq!(
        refunding.inspector_ledger,
        InspectorLedger {
            refund: Lane::once(i128::from(REFUND)),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "the refund a frame that never ran hands back is the inspector's in whole",
    );
    assert_eq!(
        refunding.refunded(),
        echo.refunded() + u64::try_from(REFUND).unwrap(),
        "and it reaches the receipt: the outcome succeeded, so its caller records it",
    );
    assert_eq!(refunding.total_gas_spent, echo.total_gas_spent, "the envelope is unmoved");

    let pooled = transact_edited(Callee::Returning, Edit::InterceptWithReservoir);
    assert_eq!(
        pooled.inspector_ledger,
        InspectorLedger {
            reservoir: Lane::once(i128::from(RESERVOIR)),
            interventions: 1,
            ..InspectorLedger::default()
        },
    );
    assert_eq!(
        pooled.total_gas_spent,
        echo.total_gas_spent - RESERVOIR,
        "a pool does move the envelope, wherever it came from",
    );
}

// --- the frozen specs
// -----------------------------------------------------------------------------

/// On a frozen spec the two lanes report and settle nothing.
///
/// The shim is not spec-gated, and must not be: the block guard has to see a rewritten receipt
/// whichever spec produced it. What is gated is the accounting the lanes feed, so a frozen spec's
/// own numbers have to be exactly what they were — which is what this reads, by comparing an
/// edited run against an unedited one on the same spec.
#[test]
fn test_a_frozen_spec_reports_the_lanes_without_settling_anything() {
    const REX6: MegaSpecId = MegaSpecId::REX6;
    fn run(edit: Option<Edit>) -> Outcome {
        let db = db_for(Callee::Returning);
        let limits = EvmTxRuntimeLimits::from_spec(REX6);
        match edit {
            Some(edit) => {
                let mut editor = Editor::new(edit);
                let outcome = transact_inspected(REX6, db, limits, &mut editor);
                assert_eq!(editor.fired, 1, "{edit:?} must land");
                outcome
            }
            None => transact(REX6, db, limits),
        }
    }

    let plain = run(None);
    assert!(plain.inspector_ledger.is_zero());

    for (edit, expected) in [
        (
            Edit::RefundAtStep(REFUND),
            InspectorLedger {
                refund: Lane::once(i128::from(REFUND)),
                ..InspectorLedger::default()
            },
        ),
        (
            Edit::ReservoirAtStep,
            InspectorLedger {
                reservoir: Lane::once(i128::from(RESERVOIR)),
                ..InspectorLedger::default()
            },
        ),
        (
            Edit::StateGasAtStep,
            InspectorLedger {
                state_gas: Lane::once(i128::from(STATE_GAS)),
                ..InspectorLedger::default()
            },
        ),
    ] {
        let edited = run(Some(edit));
        assert_eq!(edited.inspector_ledger, expected, "{edit:?}: the lane reports on every spec");
        assert_eq!(
            edited.compute_gas, plain.compute_gas,
            "{edit:?}: a frozen spec's compute total must not move",
        );
        assert_eq!(edited.destroyed, plain.destroyed, "{edit:?}: nor its destroyed lane");
        // `inspector_conjured_gas` is a reading of the ledger rather than something the
        // transaction recorded, so it moves with the lane on every spec. Every other term is what
        // a frozen spec must leave alone.
        assert_eq!(
            ConservationTerms { inspector_conjured_gas: 0, ..edited.terms },
            plain.terms,
            "{edit:?}: nothing a frozen spec records may move",
        );
        assert_eq!(
            edited.terms.inspector_conjured_gas,
            edited.inspector_ledger.conjured_gas(),
            "{edit:?}: and the term is the ledger's net, exactly as it is under REX7",
        );
    }
}

// === 5. interception ==========================================================================
//
// The gas a synthetic outcome carries.
//
// A `frame_start` / `call` / `create` callback that returns `Some(outcome)` answers the frame
// itself: no frame is built, `frame_init` never runs, and the number the caller reclaims is
// whatever `Gas` the inspector put in that outcome. Nothing about it is derived from the
// execution — the inspector chooses it outright — so it is a gas figure the transaction's
// accounting has to be told about, exactly like an edit to a result the EVM did produce.
//
// The tests here are laid out over the sign of that choice, because the two directions settle
// differently and a lane that books one and drops the other is a real failure mode:
//
// - an outcome that hands back **less** than the envelope makes the caller spend gas no frame ever
//   performed work for;
// - an outcome that hands back **more** conjures gas the transaction never funded;
// - an outcome that hands back **exactly** the envelope — the echo convention every tracer that
//   intercepts follows — moves nothing, and must book nothing.
//
// The halt direction is the asymmetry: a halting outcome hands nothing back at all, so what the
// inspector wrote in the gas figure changes nothing the transaction spends, and the destroyed
// remainder is settled against the envelope instead.

/// Gas the fixture's `CALL` forwards, and the envelope every interception is measured against.
const FORWARDED: u64 = 50_000;

/// The entry contract: one `CALL` to [`CALLEE`] forwarding [`FORWARDED`], then `STOP`.
fn call_fixture() -> MemoryDatabase {
    db_with_callee(call_then_stop(CALLEE, FORWARDED), plain_run_code(20))
}

/// How an interception sizes the `Gas` it hands back, relative to the envelope it was given.
#[derive(Clone, Copy, Debug)]
enum Sizing {
    /// The echo convention: exactly the envelope.
    Echo,
    /// Half of it — the caller spends the other half for work no frame performed.
    Half,
    /// None of it.
    Zero,
    /// More than it — gas the transaction never funded.
    Excess(u64),
}

impl Sizing {
    fn gas(self, envelope: u64) -> u64 {
        match self {
            Self::Echo => envelope,
            Self::Half => envelope / 2,
            Self::Zero => 0,
            Self::Excess(extra) => envelope + extra,
        }
    }

    /// What the ledger must carry for this sizing, as a signed movement from the envelope.
    fn expected_delta(self, envelope: u64) -> i128 {
        i128::from(self.gas(envelope)) - i128::from(envelope)
    }
}

/// Intercepts the call to [`CALLEE`], sizing the outcome's gas by [`Sizing`].
struct CallInterceptor {
    sizing: Sizing,
    classification: InstructionResult,
    intercepted: u64,
    envelope: u64,
}

impl CallInterceptor {
    fn new(sizing: Sizing, classification: InstructionResult) -> Self {
        Self { sizing, classification, intercepted: 0, envelope: 0 }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CallInterceptor {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address != CALLEE {
            return None;
        }
        self.intercepted += 1;
        self.envelope = inputs.gas_limit;
        Some(CallOutcome::new(
            InterpreterResult::new(
                self.classification,
                Bytes::new(),
                Gas::new(self.sizing.gas(inputs.gas_limit)),
            ),
            inputs.return_memory_offset.clone(),
        ))
    }
}

/// An outcome that hands back less than the envelope makes the caller spend gas nothing performed.
#[test]
fn test_a_half_gas_interception_books_the_gas_it_took_from_the_caller() {
    let mut inspector = CallInterceptor::new(Sizing::Half, InstructionResult::Stop);
    let reading = transact_inspected(MegaSpecId::REX7, call_fixture(), limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(inspector.envelope, FORWARDED, "fixture check: the forwarded envelope");
    assert!(reading.result.is_success(), "fixture check: {:?}", reading.result);
    assert_eq!(
        reading.inspector_ledger,
        InspectorLedger {
            result: Lane::once(Sizing::Half.expected_delta(FORWARDED)),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "the half the outcome withheld is gas the inspector destroyed",
    );
}

/// The extreme of the same direction: the outcome hands back nothing at all.
#[test]
fn test_a_zero_gas_interception_books_the_whole_envelope() {
    let mut inspector = CallInterceptor::new(Sizing::Zero, InstructionResult::Stop);
    let reading = transact_inspected(MegaSpecId::REX7, call_fixture(), limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(
        reading.inspector_ledger,
        InspectorLedger {
            result: Lane::once(Sizing::Zero.expected_delta(FORWARDED)),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "an outcome that returns nothing consumed the whole envelope",
    );
}

/// The other direction: an outcome that hands back more than it was given conjures the difference.
#[test]
fn test_an_over_funded_interception_books_the_gas_it_conjured() {
    const EXTRA: u64 = 7_000;
    let mut inspector = CallInterceptor::new(Sizing::Excess(EXTRA), InstructionResult::Stop);
    let reading = transact_inspected(MegaSpecId::REX7, call_fixture(), limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(
        reading.inspector_ledger,
        InspectorLedger {
            result: Lane::once(Sizing::Excess(EXTRA).expected_delta(FORWARDED)),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "gas the transaction never funded is gas the inspector conjured",
    );
}

/// The echo convention moves nothing, and must book nothing.
///
/// This is the shape every tool that intercepts actually uses, and the reason the lane could go
/// missing for as long as it did: with the envelope echoed back the accounting closes whether or
/// not anything measures it. Pinning the zero is what says the lane is measuring rather than
/// coincidentally agreeing.
#[test]
fn test_an_echoing_interception_books_no_gas_at_all() {
    for classification in [InstructionResult::Stop, InstructionResult::Revert] {
        let mut inspector = CallInterceptor::new(Sizing::Echo, classification);
        let reading =
            transact_inspected(MegaSpecId::REX7, call_fixture(), limits(), &mut inspector);

        assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
        assert_eq!(
            reading.inspector_ledger,
            InspectorLedger { interventions: 1, ..InspectorLedger::default() },
            "{classification:?}: an echoed envelope moves no gas, so no gas lane may move",
        );
        assert_eq!(reading.inspector_ledger.conjured_gas(), 0, "{classification:?}");
    }
}

/// A halting outcome hands nothing back, so what the inspector wrote in its gas figure changes
/// nothing the transaction spends — and the envelope is destroyed whole.
///
/// What the outcome claimed is still traffic on the result lane: the sizings below differ from the
/// envelope by different amounts, and each one is an edit the inspector made whether or not the
/// classification let it reach anybody.
#[test]
fn test_a_halting_interception_destroys_the_envelope_whatever_gas_it_reports() {
    for sizing in [Sizing::Echo, Sizing::Half, Sizing::Zero, Sizing::Excess(7_000)] {
        let mut inspector = CallInterceptor::new(sizing, InstructionResult::OutOfGas);
        let reading =
            transact_inspected(MegaSpecId::REX7, call_fixture(), limits(), &mut inspector);

        assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
        assert!(reading.result.is_success(), "the caller absorbs the halt: {:?}", reading.result);
        assert_eq!(
            reading.inspector_ledger.conjured_gas(),
            0,
            "{sizing:?}: a halting frame hands nothing back, so no gas lane's net may move",
        );
        assert_eq!(
            reading.inspector_ledger,
            InspectorLedger {
                interventions: 1,
                result: Lane::of(0, sizing.expected_delta(FORWARDED).unsigned_abs()),
                ..InspectorLedger::default()
            },
            "{sizing:?}: and the traffic is what the outcome claimed, off the envelope",
        );
        assert_eq!(
            reading.destroyed, FORWARDED,
            "{sizing:?}: the whole envelope is destroyed, whatever the outcome claimed",
        );
    }
}

/// The generic callback intercepts too, and is measured by the same rule.
///
/// revm runs `frame_start` before the variant-specific `call` / `create`, and an outcome returned
/// there skips both. A lane wired only to the variant hooks would leave this one unmeasured.
#[test]
fn test_the_generic_frame_start_interception_is_measured_too() {
    /// Intercepts the call to [`CALLEE`] from the generic callback, handing back half.
    #[derive(Default)]
    struct GenericInterceptor {
        intercepted: u64,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for GenericInterceptor {
        fn frame_start(
            &mut self,
            _context: &mut CTX,
            frame_input: &mut FrameInput,
        ) -> Option<FrameResult> {
            let FrameInput::Call(inputs) = frame_input else { return None };
            if inputs.target_address != CALLEE {
                return None;
            }
            self.intercepted += 1;
            Some(FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit / 2),
                ),
                inputs.return_memory_offset.clone(),
            )))
        }
    }

    let mut inspector = GenericInterceptor::default();
    let reading = transact_inspected(MegaSpecId::REX7, call_fixture(), limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(
        reading.inspector_ledger,
        InspectorLedger {
            result: Lane::once(Sizing::Half.expected_delta(FORWARDED)),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "the generic callback's interception books on the same lane as the variant one's",
    );
}

/// Init code that writes one slot and returns two bytes of runtime code.
fn init_code() -> Vec<u8> {
    BytecodeBuilder::default()
        .sstore(U256::from(0x30), U256::from(1))
        .push_number(0x6000u64)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(2u64) // size
        .push_number(30u64) // offset
        .append(RETURN)
        .build()
        .to_vec()
}

/// The entry contract: one `CREATE`, then `STOP`.
fn create_fixture() -> MemoryDatabase {
    base_db(deploy_then_stop(&init_code()))
}

/// A creation answered by the inspector is measured against the envelope its `CREATE` forwarded.
///
/// The envelope is not a constant here — `CREATE` forwards all but a sixty-fourth of what the
/// caller holds — so the test reads it back from the callback rather than asserting a figure.
#[test]
fn test_an_intercepted_creation_is_measured_against_the_envelope_it_was_handed() {
    /// Intercepts the creation, handing back half of what it was given.
    #[derive(Default)]
    struct CreateInterceptor {
        intercepted: u64,
        envelope: u64,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CreateInterceptor {
        fn create(
            &mut self,
            _context: &mut CTX,
            inputs: &mut CreateInputs,
        ) -> Option<CreateOutcome> {
            self.intercepted += 1;
            self.envelope = inputs.gas_limit();
            Some(CreateOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit() / 2),
                ),
                None,
            ))
        }
    }

    let mut inspector = CreateInterceptor::default();
    let reading = transact_inspected(MegaSpecId::REX7, create_fixture(), limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one creation");
    assert!(inspector.envelope > 0, "fixture check: the creation must forward an envelope");
    assert_eq!(
        reading.inspector_ledger,
        InspectorLedger {
            result: Lane::once(Sizing::Half.expected_delta(inspector.envelope)),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "a creation's interception is measured against the envelope its CREATE forwarded",
    );
}

/// The envelope an interception is measured against is the one the callback *received*.
///
/// A callback is free to edit the inputs and then answer the frame itself. The edit reaches no
/// frame — nothing is built from those inputs — so the envelope the caller actually funded is the
/// one the callback was handed, and an outcome echoing the *edited* limit hands back more than
/// that. Measuring against the post-edit number instead would read this run as conjuring nothing.
#[test]
fn test_the_envelope_is_the_one_the_callback_received_not_the_one_it_left() {
    const BONUS: u64 = 9_000;

    /// Raises the child's gas limit and then intercepts, echoing the raised figure.
    #[derive(Default)]
    struct RaisingInterceptor {
        intercepted: u64,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for RaisingInterceptor {
        fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
            if inputs.target_address != CALLEE {
                return None;
            }
            self.intercepted += 1;
            inputs.gas_limit += BONUS;
            Some(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            ))
        }
    }

    let mut inspector = RaisingInterceptor::default();
    let reading = transact_inspected(MegaSpecId::REX7, call_fixture(), limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(
        reading.inspector_ledger,
        InspectorLedger {
            result: Lane::once(i128::from(BONUS)),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "the bonus reaches the caller through the outcome, so it is booked once, on the result \
         lane — the env lane stays empty because no frame was ever built from those inputs",
    );
}

/// The lane reports on a frozen spec too, and reporting it settles nothing there.
///
/// The measurement is not REX7-gated, and neither are the two lanes it joins: `InspectorLedger` is
/// what the canonical block path's guard reads, so a frame an inspector answered has to be visible
/// on it whatever spec is executing. What is REX7's alone is the settlement the lane feeds — the
/// envelope a refused frame init decides the fate of. REX6 derives nothing from the envelope and
/// books no destroyed remainder, so what it reports is what it always reported.
///
/// The transaction's own gas does follow the figure the inspector wrote, on both specs. That is
/// the EVM handing the caller back what the result carries, which is upstream's arithmetic rather
/// than `MegaETH`'s, and it is the movement the lane exists to account for rather than to prevent.
#[test]
fn test_a_frozen_spec_reports_the_lane_without_settling_anything() {
    let mut echoing = CallInterceptor::new(Sizing::Echo, InstructionResult::Stop);
    let echo = transact_inspected(
        MegaSpecId::REX6,
        call_fixture(),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX6),
        &mut echoing,
    );
    let mut halving = CallInterceptor::new(Sizing::Half, InstructionResult::Stop);
    let half = transact_inspected(
        MegaSpecId::REX6,
        call_fixture(),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX6),
        &mut halving,
    );

    assert_eq!(echoing.intercepted, 1, "fixture check");
    assert_eq!(halving.intercepted, 1, "fixture check");
    assert_eq!(
        echo.inspector_ledger,
        InspectorLedger { interventions: 1, ..InspectorLedger::default() },
        "REX6: an echoed envelope moves no gas here either",
    );
    assert_eq!(
        half.inspector_ledger,
        InspectorLedger {
            result: Lane::once(Sizing::Half.expected_delta(FORWARDED)),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "REX6: the lane reports, because the block guard has to see this frame on every spec",
    );
    assert_eq!(
        (echo.destroyed, half.destroyed),
        (0, 0),
        "REX6 has no destroyed remainder to book, on either sizing",
    );
    assert_eq!(
        echo.compute_gas, half.compute_gas,
        "and its compute total does not follow the figure the inspector wrote",
    );
    assert_eq!(
        half.total_gas_spent - echo.total_gas_spent,
        FORWARDED / 2,
        "the caller really did lose the half the outcome withheld — that is the EVM's arithmetic",
    );
}
