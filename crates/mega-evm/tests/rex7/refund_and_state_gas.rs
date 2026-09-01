//! The two numbers on a receipt that the conservation law cannot see, and the lanes that do.
//!
//! The law is stated over `total_gas_spent`, which is `limit - remaining`. A transaction's receipt
//! carries two more figures that arithmetic does not reach: the EIP-3529 refund, which decides what
//! the sender actually pays, and the EIP-8037 state-gas dimension — a `Gas`'s `reservoir` and its
//! `state_gas_spent` counter — which decides how much of the envelope the receipt counts as spent
//! at all.
//!
//! Both are reachable from every callback that is handed a `Gas`, and both were unmeasured. The
//! shapes here are what the two lanes now book, and each pins the *reason* its lane is measured
//! where it is:
//!
//! - a **refund** is a quantity the EVM also produces, so only a difference across a callback
//!   isolates the inspector's share — the lane is measured at the boundary, and is nominal in both
//!   the senses that can make it differ from what reaches the receipt (the EIP-3529 cap, and the
//!   chain of successful frame returns an edit has to survive);
//! - a **reservoir** is a quantity `MegaETH` never produces at all, and one revm propagates by
//!   replacement rather than by accumulation, so a boundary difference would book edits the EVM
//!   goes on to erase. The lane is settled once, from the number the transaction ends with, which
//!   is exactly the surviving part and is the inspector's in whole.

use crate::common::{CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    AdditionalLimit, ConservationTerms, EmptyExternalEnv, EvmTxRuntimeLimits, InspectorLedger,
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
    MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CALL, POP, STOP},
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    handler::EvmTr,
    interpreter::{
        interpreter_types::LoopControl, CallInputs, CallOutcome, Interpreter, InterpreterAction,
        InterpreterTypes,
    },
    Inspector,
};

/// High enough that EVM gas is never what binds.
const TX_GAS_LIMIT: u64 = 100_000_000;
/// Gas the fixture's inner `CALL` forwards.
const INNER_CALL_GAS: u64 = 200_000;

/// Refund an edit writes, small enough that the EIP-3529 cap does not clip it.
const REFUND: i64 = 2_000;
/// Refund the cap test writes, chosen to exceed a fifth of anything the fixture can burn.
const OVERSIZED_REFUND: i64 = 60_000;
/// The EIP-8037 pool a reservoir edit fills.
const RESERVOIR: u64 = 10_000;
/// The EIP-8037 spend counter a state-gas edit writes.
const STATE_GAS: i64 = 5_000;

/// Slot the caller writes.
const TOP_SLOT: u64 = 0x10;
/// Slot the callee writes and keeps.
const CALLEE_SLOT: u64 = 0x20;
/// Slot the callee sets and clears, so its frame carries a refund the EVM produced.
const CLEARED_SLOT: u64 = 0x30;

// --- what one run reports ----------------------------------------------------------------------

struct Reading {
    result: ExecutionResult<MegaHaltReason>,
    /// Receipt `gas_used`: the envelope less the refund, floored by EIP-7623.
    gas_used: u64,
    /// Receipt envelope, which is what the conservation law is stated over.
    total_gas_spent: u64,
    /// Receipt refund, after the EIP-3529 cap.
    refunded: u64,
    /// Receipt EIP-8037 state gas.
    state_gas_spent: u64,
    destroyed: u64,
    terms: ConservationTerms,
    ledger: InspectorLedger,
}

/// The conservation identity, over the envelope the receipt reports.
fn assert_identity(label: &str, r: &Reading) {
    assert_eq!(
        r.terms.inspector_conjured_gas,
        r.ledger.conjured_gas(),
        "{label}: the law's `I` term is the ledger's net, and nothing else",
    );
    assert_eq!(
        r.terms.envelope_for(r.destroyed),
        i128::from(r.total_gas_spent),
        "{label}: the law must close against the envelope the receipt reports ({})",
        r.terms,
    );
}

fn tx() -> MegaTransaction {
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(TX_GAS_LIMIT).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn context(db: &mut MemoryDatabase) -> MegaContext<&mut MemoryDatabase, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, MegaSpecId::REX7)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context
}

fn read(limit: &AdditionalLimit, outcome: MegaTransactionOutcome) -> Reading {
    assert_eq!(
        outcome.inspector_ledger,
        limit.inspector_ledger(),
        "the outcome must report the ledger the shim booked, unchanged",
    );
    let gas = *outcome.result_and_state.result.gas();
    Reading {
        result: outcome.result_and_state.result,
        gas_used: gas.tx_gas_used(),
        total_gas_spent: gas.total_gas_spent(),
        refunded: gas.inner_refunded(),
        state_gas_spent: gas.state_gas_spent_final(),
        destroyed: outcome.compute_gas_destroyed,
        terms: limit.conservation_terms(),
        ledger: outcome.inspector_ledger,
    }
}

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
    BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(u128::from(INNER_CALL_GAS))
        .append(CALL)
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
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, caller_code())
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        .account_code(CALLEE, callee_code(callee))
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
        if self.fired > 0 || inputs.target_address != CALLEE || self.edit != Edit::ReservoirOnInputs
        {
            return None;
        }
        inputs.reservoir += RESERVOIR;
        self.fired += 1;
        None
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
fn transact_plain(callee: Callee) -> Reading {
    let mut db = db_for(callee);
    let mut evm = MegaEvm::new(context(&mut db));
    let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
    let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
    reading
}

/// Runs it with one edit applied, asserting the edit landed exactly once.
fn transact_edited(callee: Callee, edit: Edit) -> Reading {
    let mut db = db_for(callee);
    let mut editor = Editor::new(edit);
    let mut evm = MegaEvm::new(context(&mut db)).with_inspector(&mut editor);
    let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
    assert_eq!(
        alloy_evm::Evm::inspector(&evm).fired,
        1,
        "{edit:?}: the fixture must reach the edit's callback exactly once",
    );
    let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
    reading
}

// --- the fixture's own assumptions ---------------------------------------------------------------

/// The uninspected run is what the cells below assume it is: it succeeds, it produces a refund of
/// its own, and it reports no EIP-8037 dimension at all.
#[test]
fn test_the_fixture_refunds_on_its_own_and_holds_no_state_gas() {
    let plain = transact_plain(Callee::Returning);
    assert!(plain.result.is_success(), "{:?}", plain.result);
    assert!(
        plain.refunded > 0,
        "the callee's cleared slot must leave a refund for the lowering cell to take from",
    );
    assert_eq!(
        plain.gas_used,
        plain.total_gas_spent - plain.refunded,
        "the receipt's two gas numbers differ by exactly the refund",
    );
    assert_eq!(plain.state_gas_spent, 0, "EIP-8037 is off on every MegaETH path");
    assert!(plain.ledger.is_zero(), "no inspector ran: {:?}", plain.ledger);
    assert_identity("plain", &plain);
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
        edited.ledger,
        InspectorLedger { refund: i128::from(REFUND), ..InspectorLedger::default() },
        "the shim must book the refund and nothing else",
    );
    assert_eq!(
        edited.total_gas_spent, plain.total_gas_spent,
        "a refund does not move the envelope, which is why the law cannot see it",
    );
    assert_eq!(
        edited.refunded,
        plain.refunded + u64::try_from(REFUND).unwrap(),
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
    assert!(!edited.ledger.is_zero(), "and the block guard has to see it");
    assert_identity("refund at step", &edited);
}

/// The same edit made at the last callback that holds the finished frame's result.
#[test]
fn test_a_refund_written_into_a_finished_frame_result_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::RefundAtCallEnd(REFUND));

    assert_eq!(
        edited.ledger,
        InspectorLedger { refund: i128::from(REFUND), ..InspectorLedger::default() },
    );
    assert_eq!(edited.refunded, plain.refunded + u64::try_from(REFUND).unwrap());
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent);
    assert_identity("refund at call_end", &edited);
}

/// A refund taken *out* is booked with the sign that says so — a lane that only saw one direction
/// would report an inspector that raised the sender's bill as having done nothing.
#[test]
fn test_a_refund_taken_out_of_a_frame_is_booked_with_the_sign_that_says_so() {
    let plain = transact_plain(Callee::Returning);
    assert!(
        plain.refunded >= u64::try_from(REFUND).unwrap(),
        "fixture check: there must be a refund to take from, got {}",
        plain.refunded,
    );

    let edited = transact_edited(Callee::Returning, Edit::RefundAtCallEnd(-REFUND));
    assert_eq!(
        edited.ledger,
        InspectorLedger { refund: -i128::from(REFUND), ..InspectorLedger::default() },
    );
    assert_eq!(edited.refunded, plain.refunded - u64::try_from(REFUND).unwrap());
    assert_eq!(
        edited.gas_used,
        plain.gas_used + u64::try_from(REFUND).unwrap(),
        "the sender pays more, by exactly what was taken",
    );
    assert_identity("refund lowered", &edited);
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
        edited.ledger,
        InspectorLedger { refund: i128::from(OVERSIZED_REFUND), ..InspectorLedger::default() },
        "the lane carries the nominal edit",
    );
    assert_eq!(
        edited.refunded,
        edited.total_gas_spent / 5,
        "while the receipt carries the EIP-3529 cap",
    );
    assert!(
        edited.refunded < plain.refunded + u64::try_from(OVERSIZED_REFUND).unwrap(),
        "fixture check: the cap must actually bind, or this cell asserts nothing",
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent, "the envelope is untouched");
    assert_identity("oversized refund", &edited);
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
        edited.ledger,
        InspectorLedger { refund: i128::from(REFUND), ..InspectorLedger::default() },
        "the lane books the edit",
    );
    assert_eq!(
        edited.refunded, plain.refunded,
        "the receipt is unmoved: a reverting frame hands its caller no refund",
    );
    assert_eq!(edited.gas_used, plain.gas_used);
    assert_identity("refund on a reverting frame", &edited);
}

// --- the EIP-8037 state-gas dimension ------------------------------------------------------------

/// A reservoir an inspector fills is gas the transaction never funded: the receipt reports that
/// much less spent, and the law needs it back.
#[test]
fn test_a_reservoir_written_into_a_live_interpreter_is_booked_and_the_law_closes() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::ReservoirAtStep);

    assert_eq!(
        edited.ledger,
        InspectorLedger { reservoir: i128::from(RESERVOIR), ..InspectorLedger::default() },
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
    assert_identity("reservoir at step", &edited);
}

/// The same, written into the pool a call's inputs seed the child frame with.
#[test]
fn test_a_reservoir_written_into_a_frame_input_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::ReservoirOnInputs);

    assert_eq!(
        edited.ledger,
        InspectorLedger {
            reservoir: i128::from(RESERVOIR),
            // The inputs came back changed in a field the envelope lane does not cover, which the
            // rewrite comparison books on its own.
            interventions: 1,
            ..InspectorLedger::default()
        },
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent - RESERVOIR);
    assert_identity("reservoir on inputs", &edited);
}

/// And into the finished frame's own pool, which its caller takes whatever the classification.
#[test]
fn test_a_reservoir_written_into_a_finished_frame_result_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::ReservoirAtCallEnd);

    assert_eq!(
        edited.ledger,
        InspectorLedger { reservoir: i128::from(RESERVOIR), ..InspectorLedger::default() },
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent - RESERVOIR);
    assert_identity("reservoir at call_end", &edited);
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
        edited.ledger.is_zero(),
        "an edit the child frame's own pool replaces moved nothing: {:?}",
        edited.ledger,
    );
    assert_eq!(edited.total_gas_spent, plain.total_gas_spent);
    assert_eq!(edited.gas_used, plain.gas_used);
    assert_eq!(edited.refunded, plain.refunded);
    assert_identity("reservoir in the dead window", &edited);
}

/// The spend counter's own effect on the receipt: a successful transaction reports it, whether or
/// not EIP-8037 is enabled.
#[test]
fn test_state_gas_written_into_a_live_interpreter_reaches_the_receipt_and_is_booked() {
    let plain = transact_plain(Callee::Returning);
    let edited = transact_edited(Callee::Returning, Edit::StateGasAtStep);

    assert_eq!(plain.state_gas_spent, 0, "fixture check");
    assert_eq!(
        edited.state_gas_spent,
        u64::try_from(STATE_GAS).unwrap(),
        "the receipt reports what was written",
    );
    assert_eq!(
        edited.ledger,
        InspectorLedger { state_gas: i128::from(STATE_GAS), ..InspectorLedger::default() },
    );
    assert_eq!(
        edited.total_gas_spent, plain.total_gas_spent,
        "the envelope is untouched, so this lane is not a term of the law either",
    );
    assert_eq!(edited.terms.inspector_conjured_gas, 0);
    assert_identity("state gas at step", &edited);
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
        edited.ledger,
        InspectorLedger { reservoir: i128::from(STATE_GAS), ..InspectorLedger::default() },
        "the spend counter of a reverting frame arrives in its caller as a pool",
    );
    assert_eq!(
        edited.state_gas_spent, 0,
        "and not as a spend: a failing frame's counter is not accumulated",
    );
    assert_eq!(
        edited.total_gas_spent,
        plain.total_gas_spent - u64::try_from(STATE_GAS).unwrap(),
        "so the envelope moves, and the law's term has to move with it",
    );
    assert_identity("state gas on a reverting frame", &edited);
}
