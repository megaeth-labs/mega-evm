//! The lanes the measurement shim books a rewrite on, and what each one is measured against.
//!
//! `MegaETH` wraps every inspector it is handed. The EVM does not execute inside an inspector
//! callback, so anything that changes across one is the inspector's doing by construction — which
//! is what makes the callback boundary a sound place to measure from. Every fixture here is the
//! same comparison: one run with an inspector against one without, over the same fixture, with the
//! conservation law checked on both by the shared driver.
//!
//! The two groups, in the order they appear:
//!
//! 1. **The shim itself** — gas written into an interpreter's counter or a frame's gas limit is
//!    measured, booked, and kept out of enforcement, with the clamp re-derived on the spot; an
//!    observation-only inspector is bit-identical to no inspector at all.
//! 2. **The receipt's other two numbers** — the EIP-3529 refund, measured at the callback boundary
//!    because the EVM produces refunds too, and the EIP-8037 state-gas dimension, settled from the
//!    transaction's final figures because `MegaETH` produces none of it and revm propagates it by
//!    replacement.
//!
//! The two windows in which a rewrite lands after the accounting that should have read it are in
//! `shim_settlement.rs`, and the shapes an all-zero ledger used to admit are in
//! `shim_blind_spots.rs`. The rewrites the shim *refuses* are in `shim_refusals.rs`; the exhaustive
//! callback × shape sweep is in `inspector_cheat_matrix.rs`.

use crate::{
    common::{base_db, transact, transact_inspected, Outcome, CALLEE, CONTRACT},
    inspector_common::{
        append_call, call_then_stop, countdown_loop_code, db_with_callee, deploy_then_stop, limits,
        limits_with_compute, plain_run_code, REFUND,
    },
};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    ConservationTerms, EvmTxRuntimeLimits, InspectorLedger, Lane, MegaHaltReason, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CALL, MSTORE, POP, RETURN, STOP},
    interpreter::{
        interpreter_types::LoopControl, CallInputs, CallOutcome, CreateInputs, CreateOutcome, Gas,
        InstructionResult, Interpreter, InterpreterAction, InterpreterResult, InterpreterTypes,
    },
    Inspector,
};
use std::vec::Vec;

// === the shim itself =========================================================================
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

// === the receipt's other two numbers =========================================================
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
