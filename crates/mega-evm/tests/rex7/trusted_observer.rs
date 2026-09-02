//! The one inspector the shim does not measure, and the two things that keep that safe.
//!
//! A type whose author has declared it `TrustedObserver` is delegated to without any of the
//! shim's readings. The declaration is a promise about source, not a detection, so it is held up
//! by exactly two things and both are here:
//!
//! - **A declared type that keeps the promise is indistinguishable from one that is measured.** The
//!   same observer, over the same fixture, run three ways — with no inspector, declared, and
//!   undeclared — produces the same receipt, the same four resource dimensions, the same state, and
//!   the same callbacks in the same numbers.
//! - **A declared type that breaks it fails where it is exercised.** Debug builds take the
//!   measuring path anyway and assert the ledger stayed empty, so a wrong declaration panics at the
//!   callback that broke it rather than reaching a node.
//!
//! The second half is a `debug_assertions` property by construction, so the two anchors below are
//! compiled only into debug builds — which is how this repository's tests run.
//!
//! Which leaves the first half needing a release run to mean anything: in a debug build the
//! declared run takes the measuring path like every other, so it is the same code the other two
//! runs exercise. `cargo test -p mega-evm --release --test rex7` is where the comparison is
//! actually against the fast path, and it is an acceptance gate for that reason.

use crate::common::{CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, InspectorLedger, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionNew as _, TrustedObserver,
};
use revm::{
    bytecode::opcode::{CALL, POP, STOP},
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    interpreter::{CallInputs, CallOutcome, Interpreter, InterpreterTypes},
    state::EvmState,
    Inspector,
};

/// Transaction gas limit used throughout: high enough that EVM gas is never what binds.
const TX_GAS_LIMIT: u64 = 100_000_000;

/// Everything one run of the fixture produces, for the three-way comparison.
#[derive(Debug)]
struct Reading {
    result: ExecutionResult<MegaHaltReason>,
    compute_gas: u64,
    enforced: u64,
    destroyed: u64,
    data_size: u64,
    kv_updates: u64,
    state_growth: u64,
    gas_used: u64,
    total_gas_spent: u64,
    ledger: InspectorLedger,
    state: EvmState,
}

/// Asserts two runs of the fixture are the same run, field by field.
///
/// Written out rather than derived from `PartialEq` on the whole struct so that the field that
/// disagrees is the one the failure names.
fn assert_same(label: &str, left: &Reading, right: &Reading) {
    assert_eq!(format!("{:?}", left.result), format!("{:?}", right.result), "{label}: result");
    assert_eq!(left.compute_gas, right.compute_gas, "{label}: compute gas");
    assert_eq!(left.enforced, right.enforced, "{label}: enforced compute gas");
    assert_eq!(left.destroyed, right.destroyed, "{label}: destroyed compute gas");
    assert_eq!(left.data_size, right.data_size, "{label}: data size");
    assert_eq!(left.kv_updates, right.kv_updates, "{label}: kv updates");
    assert_eq!(left.state_growth, right.state_growth, "{label}: state growth");
    assert_eq!(left.gas_used, right.gas_used, "{label}: gas used");
    assert_eq!(left.total_gas_spent, right.total_gas_spent, "{label}: total gas spent");
    assert_eq!(left.state, right.state, "{label}: produced state");
}

fn tx() -> MegaTransaction {
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(TX_GAS_LIMIT).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn context(db: &mut MemoryDatabase) -> MegaContext<&mut MemoryDatabase, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, MegaSpecId::REX7);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context
}

fn read(outcome: mega_evm::MegaTransactionOutcome) -> Reading {
    let gas_used = outcome.result_and_state.result.tx_gas_used();
    let total_gas_spent = outcome.result_and_state.result.gas().total_gas_spent();
    Reading {
        result: outcome.result_and_state.result,
        compute_gas: outcome.compute_gas_used,
        enforced: outcome.compute_gas_enforced,
        destroyed: outcome.compute_gas_destroyed,
        data_size: outcome.data_size,
        kv_updates: outcome.kv_updates,
        state_growth: outcome.state_growth_used,
        gas_used,
        total_gas_spent,
        ledger: outcome.inspector_ledger,
        state: outcome.result_and_state.state,
    }
}

/// No inspector at all: revm's plain frame loops, which never call one.
fn transact_plain(mut db: MemoryDatabase) -> Reading {
    let mut evm = MegaEvm::new(context(&mut db));
    read(evm.execute_transaction(tx()).expect("tx should not surface EVMError"))
}

/// The inspected loop with the shim measuring, which is what every undeclared inspector gets.
fn transact_measured<I>(mut db: MemoryDatabase, inspector: &mut I) -> Reading
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    let mut evm = MegaEvm::new(context(&mut db)).with_inspector(inspector);
    read(evm.execute_transaction(tx()).expect("tx should not surface EVMError"))
}

/// The inspected loop with the shim delegating on the strength of the declaration.
fn transact_trusted<I>(mut db: MemoryDatabase, inspector: &mut I) -> Reading
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>> + TrustedObserver,
{
    let mut evm = MegaEvm::new(context(&mut db)).with_trusted_inspector(inspector);
    read(evm.execute_transaction(tx()).expect("tx should not surface EVMError"))
}

/// Counts the callbacks it is handed and changes nothing — a declaration that holds.
#[derive(Default, Debug, PartialEq, Eq)]
struct Observer {
    initialize_interps: u64,
    steps: u64,
    step_ends: u64,
    calls: u64,
    call_ends: u64,
}

impl TrustedObserver for Observer {}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Observer {
    fn initialize_interp(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.initialize_interps += 1;
    }

    fn step(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.steps += 1;
    }

    fn step_end(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.step_ends += 1;
    }

    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.calls += 1;
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends += 1;
    }
}

/// The fixture: a frame that writes storage and makes an inner call, so every dimension the
/// comparison covers has something in it.
fn fixture_db() -> MemoryDatabase {
    let callee =
        BytecodeBuilder::default().sstore(U256::from(0x11), U256::from(0x22)).append(STOP).build();
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0x20), U256::from(0x99))
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(CALLEE)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        .account_code(CALLEE, callee)
}

/// ★ Declaring an observer read-only changes what the measurement costs and nothing it says.
///
/// The same observer runs the same fixture three ways: uninspected, measured, and declared. All
/// three produce the same receipt, the same four resource dimensions and the same state; both
/// inspected runs leave an empty ledger; and the declared run is handed exactly the callbacks the
/// measured one was, in the same numbers.
///
/// That last part is what separates "the shim skipped its own work" from "the shim skipped the
/// inspector": a fast path that delegated less would pass every other assertion here.
#[test]
fn test_a_declared_observer_runs_the_transaction_the_other_two_runs_produce() {
    let plain = transact_plain(fixture_db());

    let mut measured_observer = Observer::default();
    let measured = transact_measured(fixture_db(), &mut measured_observer);

    let mut trusted_observer = Observer::default();
    let trusted = transact_trusted(fixture_db(), &mut trusted_observer);

    assert!(measured_observer.steps > 0, "the fixture must run opcodes under the inspector");
    assert_eq!(measured_observer.calls, 2, "one top-level frame plus one inner call");
    assert_eq!(
        measured_observer, trusted_observer,
        "the declared run must be handed the same callbacks as the measured one",
    );

    assert!(measured.ledger.is_zero(), "measured: {:?}", measured.ledger);
    assert!(
        trusted.ledger.is_zero(),
        "the fast path books nothing by construction: {:?}",
        trusted.ledger,
    );

    assert_same("declared against uninspected", &trusted, &plain);
    assert_same("declared against measured", &trusted, &measured);
}

/// A rewriting inspector that moves gas, wearing a declaration it has no right to.
///
/// `TrustedObserver` is implemented for it here and nowhere else: this is the only place in the
/// repository where the promise is deliberately broken, and it exists so that breaking it is
/// known to be caught.
#[cfg(debug_assertions)]
#[derive(Default)]
struct LiarThatMovesGas {
    fired: bool,
}

#[cfg(debug_assertions)]
impl TrustedObserver for LiarThatMovesGas {}

#[cfg(debug_assertions)]
impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for LiarThatMovesGas {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if !self.fired {
            self.fired = true;
            interp.gas.set_remaining(interp.gas.remaining() + 10_000);
        }
    }
}

/// A rewriting inspector that moves no gas at all, wearing the same declaration.
///
/// Stepping the program counter deletes an instruction from the frame and costs the transaction
/// nothing, so no gas lane sees it — only `interventions` does. It is here because a verification
/// written over the gas lanes alone would pass this one.
#[cfg(debug_assertions)]
#[derive(Default)]
struct LiarThatMovesNoGas {
    fired: bool,
}

#[cfg(debug_assertions)]
impl TrustedObserver for LiarThatMovesNoGas {}

#[cfg(debug_assertions)]
impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for LiarThatMovesNoGas {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        use revm::interpreter::interpreter_types::{Jumps, LoopControl};
        if !self.fired && interp.bytecode.is_not_end() {
            self.fired = true;
            interp.bytecode.relative_jump(1);
        }
    }
}

/// ★ A declaration that is false fails at the callback that made it false.
///
/// Debug builds run the whole measurement behind the declaration and assert the ledger came back
/// empty, so this is what "trust, and verify" means in practice: the release build pays nothing
/// and the build every test, every CI job and every chaos sweep runs catches the mis-declaration
/// on the spot.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "declared `TrustedObserver` wrote something back at `step`")]
fn test_a_declared_inspector_that_moves_gas_fails_the_debug_verification() {
    let mut liar = LiarThatMovesGas::default();
    let _ = transact_trusted(fixture_db(), &mut liar);
}

/// ★ And so does one whose rewrite moves no gas.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "declared `TrustedObserver` wrote something back at `step`")]
fn test_a_declared_inspector_that_moves_no_gas_fails_the_debug_verification_too() {
    let mut liar = LiarThatMovesNoGas::default();
    let _ = transact_trusted(fixture_db(), &mut liar);
}
