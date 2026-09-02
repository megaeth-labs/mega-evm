//! The measurement shim: what an inspector does to gas is measured, booked, and kept out of
//! enforcement.
//!
//! `MegaETH` wraps every inspector it is handed. The EVM does not execute inside an inspector
//! callback, so anything that changes across one is the inspector's doing by construction — which
//! is what makes the callback boundary a sound place to measure from.
//!
//! Each test here is one shape a rewriting inspector can take, and each pins a different half of
//! the mechanism:
//!
//! - injecting gas into a running interpreter must not buy compute headroom, and the gas clamp must
//!   tighten again immediately rather than at the next checkpoint;
//! - raising a child frame's gas limit conjures gas the transaction never funded, which the ledger
//!   has to account for or the conservation law breaks;
//! - resurrecting a failed contract creation is refused outright;
//! - an observation-only inspector changes nothing at all;
//! - and removing gas is measured with the same machinery as adding it.

use crate::common::{base_db, CALLEE, CALLER, CONTRACT};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    AdditionalLimit, ConservationTerms, EmptyExternalEnv, EvmTxRuntimeLimits, InspectorLedger,
    Lane, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{
        CALL, CREATE, DUP1, JUMPDEST, JUMPI, MSTORE, POP, RETURN, STOP, SUB, SWAP1,
    },
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    handler::EvmTr,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Gas, InstructionResult, Interpreter,
        InterpreterResult, InterpreterTypes,
    },
    state::EvmState,
    Inspector,
};

/// Transaction gas limit used throughout: high enough that EVM gas is never what binds.
const TX_GAS_LIMIT: u64 = 100_000_000;

/// Everything one transaction reports, plus what the shim booked for it.
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
    terms: ConservationTerms,
    ledger: InspectorLedger,
    state: EvmState,
}

impl Reading {
    fn halt_reason(&self) -> &MegaHaltReason {
        match &self.result {
            ExecutionResult::Halt { reason, .. } => reason,
            other => panic!("expected a halt, got {other:?}"),
        }
    }
}

/// The conservation identity, stated with the term the measurement shim contributes.
///
/// Uninspected, this is the identity `common::assert_terminal_identity` checks: the reported
/// compute total plus `MegaETH` storage gas, less the `CALL_STIPEND` the EVM minted into child
/// frames, is exactly the envelope the receipt reports. An inspector that conjures gas — by writing
/// into an interpreter's counter or into a frame's gas limit — makes the transaction spend less
/// than its frames recorded, by exactly what it conjured, so the identity only closes once the
/// ledger's term is taken out of the accounted side.
///
/// This is what goes red when a lane the shim is supposed to book goes unbooked: the two sides
/// disagree by precisely the unbooked amount.
fn assert_identity(label: &str, r: &Reading) {
    assert_eq!(
        r.compute_gas,
        r.enforced + r.destroyed,
        "{label}: reported compute must split into enforced + destroyed",
    );
    assert_eq!(
        r.terms.inspector_conjured_gas,
        r.ledger.conjured_gas(),
        "{label}: the law's `I` term is the ledger's net, and nothing else",
    );
    assert_eq!(
        r.terms.envelope_for(r.destroyed),
        i128::from(r.total_gas_spent),
        "{label}: the law must close against the envelope the receipt reports; \
         reported compute={} destroyed={} envelope={} ({})",
        r.compute_gas,
        r.destroyed,
        r.total_gas_spent,
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

/// The context every run in this module uses: REX7, no external environment, no operator fee.
fn context(
    db: &mut MemoryDatabase,
    limits: EvmTxRuntimeLimits,
) -> MegaContext<&mut MemoryDatabase, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, MegaSpecId::REX7).with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context
}

/// Runs the transaction with no inspector at all — the reference every inspected run is compared
/// against.
fn transact_plain(mut db: MemoryDatabase, limits: EvmTxRuntimeLimits) -> Reading {
    let mut evm = MegaEvm::new(context(&mut db, limits));
    let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
    let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
    reading
}

/// Runs the transaction with `inspector` attached, borrowed so the caller can read it back
/// afterwards.
fn transact_inspected<I>(
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    inspector: &mut I,
) -> Reading
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    let mut evm = MegaEvm::new(context(&mut db, limits)).with_inspector(inspector);
    let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
    let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
    reading
}

/// Like [`transact_inspected`], but surfaces the `EVMError` instead of panicking on it.
fn try_transact_inspected<I>(
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    inspector: &mut I,
) -> Result<(), String>
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    let mut evm = MegaEvm::new(context(&mut db, limits)).with_inspector(inspector);
    evm.execute_transaction(tx()).map(|_| ()).map_err(|e| format!("{e:?}"))
}

/// Reads one transaction's outcome, and pins the outcome's own ledger field against the tracker's
/// on every shape this module runs.
///
/// The outcome is what a consumer sees; the tracker is where the shim booked. Checking them here
/// means every test below asserts the outcome API carries the measurement, not just the two that
/// look at it on purpose.
fn read(limit: &AdditionalLimit, outcome: MegaTransactionOutcome) -> Reading {
    assert_eq!(
        outcome.inspector_ledger,
        limit.inspector_ledger(),
        "the outcome must report the ledger the shim booked, unchanged",
    );
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
        terms: limit.conservation_terms(),
        ledger: outcome.inspector_ledger,
        state: outcome.result_and_state.state,
    }
}

/// A countdown loop of plain opcodes with no checkpoint anywhere in the body, so the whole run is
/// one settlement segment and the gas clamp is the only thing enforcing the compute limit inside
/// it.
fn countdown_loop_code(iterations: u16) -> Bytes {
    let mut code = Vec::new();
    code.push(0x61); // PUSH2
    code.extend_from_slice(&iterations.to_be_bytes());
    let loop_target = u8::try_from(code.len()).expect("loop target must fit in a PUSH1");
    code.push(JUMPDEST);
    code.extend_from_slice(&[0x60, 0x01]); // PUSH1 1
    code.push(SWAP1);
    code.push(SUB);
    code.push(DUP1);
    code.extend_from_slice(&[0x60, loop_target]); // PUSH1 loop
    code.push(JUMPI);
    code.push(STOP);
    Bytes::from(code)
}

/// A straight run of plain opcodes that always succeeds.
fn plain_run_code(pairs: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder.append(STOP).build()
}

fn limits_with_compute(limit: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(limit)
}

fn default_limits() -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)
}

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

/// Rewrites every failed contract creation into a successful one — the shape the shim refuses.
#[derive(Default)]
struct CreateReviver;

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CreateReviver {
    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if !outcome.result.result.is_ok() {
            outcome.result.result = InstructionResult::Return;
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
    let intrinsic = transact_plain(base_db(plain_run_code(0)), default_limits()).compute_gas;
    let limits = limits_with_compute(intrinsic + 5_000);

    let plain = transact_plain(base_db(code.clone()), limits);
    let mut inspector = GasEditor::new(20, INJECTED as i64);
    let inspected = transact_inspected(base_db(code), limits, &mut inspector);

    assert!(inspector.applied, "the fixture must reach the injection point");
    assert!(
        matches!(plain.halt_reason(), MegaHaltReason::ComputeGasLimitExceeded { .. }),
        "fixture check: the uninspected run must stop on the compute limit, got {:?}",
        plain.halt_reason(),
    );
    assert_eq!(
        inspected.enforced, plain.enforced,
        "the injection must be neither counted as work nor deducted from it, and the re-derived \
         clamp must stop the loop at the same opcode the uninspected run stopped at; \
         inspected result {:?}",
        inspected.result,
    );
    assert!(
        matches!(inspected.halt_reason(), MegaHaltReason::ComputeGasLimitExceeded { .. }),
        "injected gas must not turn a compute-limit halt into something else, got {:?}",
        inspected.halt_reason(),
    );
    assert_eq!(
        inspected.ledger.gas,
        Lane::once(i128::from(INJECTED)),
        "the ledger must hold exactly what was injected",
    );
    assert_eq!(inspected.ledger.env, Lane::default(), "no frame envelope was touched");
    assert_eq!(
        i128::from(inspected.total_gas_spent) + i128::from(INJECTED),
        i128::from(plain.total_gas_spent),
        "the injected gas is refunded with the rest of the rescued remainder, so the transaction \
         spends exactly that much less than the uninspected run",
    );
    assert_identity("injected", &inspected);
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

    let plain = transact_plain(base_db(code.clone()), default_limits());
    let mut inspector = GasEditor::new(20, -(REMOVED as i64));
    let inspected = transact_inspected(base_db(code), default_limits(), &mut inspector);

    assert!(inspector.applied, "the fixture must reach the removal point");
    assert!(plain.result.is_success(), "fixture check: {:?}", plain.result);
    assert!(inspected.result.is_success(), "removing gas must not fail the transaction");
    assert_eq!(
        inspected.ledger.gas,
        Lane::once(-i128::from(REMOVED)),
        "the ledger must hold the removal as a negative entry",
    );
    assert_eq!(
        inspected.enforced, plain.enforced,
        "gas the inspector destroyed is not work the EVM performed",
    );
    assert_eq!(
        inspected.total_gas_spent,
        plain.total_gas_spent + REMOVED,
        "the removed gas never comes back, so the envelope is exactly that much larger",
    );
    assert_identity("removed", &inspected);
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
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let plain = transact_plain(build_db(), default_limits());
    let mut inspector = CallGasLimitRaiser { bonus: BONUS, raises: 0 };
    let inspected = transact_inspected(build_db(), default_limits(), &mut inspector);

    assert_eq!(inspector.raises, 1, "the fixture must make exactly one inner call");
    assert!(plain.result.is_success(), "fixture check: {:?}", plain.result);
    assert!(inspected.result.is_success(), "the inner call must still succeed");
    assert_eq!(
        inspected.ledger.env,
        Lane::once(i128::from(BONUS)),
        "the ledger must hold exactly the gas the inspector added to the child's envelope",
    );
    assert_eq!(inspected.ledger.gas, Lane::default(), "no interpreter counter was touched");
    assert_eq!(
        inspected.total_gas_spent + BONUS,
        plain.total_gas_spent,
        "the child returns the conjured gas to its caller, so the transaction spends that much less",
    );
    assert_eq!(
        inspected.enforced, plain.enforced,
        "a wider envelope is not more work: the child's compute budget comes from the compute \
         tracker, not from its gas limit",
    );
    assert_identity("raised child gas limit", &inspected);
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
    let code = BytecodeBuilder::default()
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
    let db = base_db(code).account_code(CALLEE, callee);

    let mut inspector = Interceptor::default();
    let inspected = transact_inspected(db, default_limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert!(inspected.result.is_success(), "fixture check: {:?}", inspected.result);
    assert_eq!(
        inspected.ledger,
        InspectorLedger { interventions: 1, ..InspectorLedger::default() },
        "an edit to inputs that never reach a frame conjures nothing, but answering the frame is \
         itself a rewrite",
    );
    assert_eq!(inspected.ledger.conjured_gas(), 0, "no gas lane may move on this shape");
    assert_identity("intercepted", &inspected);
}

/// (iii) A `create_end` that turns a failed contract creation into a successful one is refused,
/// loudly.
///
/// By that point revm has already reverted the frame's journal checkpoint and already declined to
/// deposit any code, so the rewrite would report a deployment that did not happen. The shim
/// restores the original classification and refuses to let the transaction produce a receipt at
/// all: debug builds assert, release builds surface the refusal as an `EVMError`.
#[test]
fn test_reviving_a_failed_creation_is_refused() {
    // Init code that reverts immediately: PUSH1 0, PUSH1 0, REVERT.
    let init_code: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xfd];
    let mut builder = BytecodeBuilder::default();
    for (offset, byte) in init_code.iter().enumerate() {
        builder = builder
            .push_number(u64::from(*byte))
            .push_number(offset as u64)
            .append(revm::bytecode::opcode::MSTORE8);
    }
    let code = builder
        .push_number(init_code.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build();
    let db = base_db(code);

    let run = || {
        let mut inspector = CreateReviver;
        try_transact_inspected(db.clone(), default_limits(), &mut inspector)
    };

    if cfg!(debug_assertions) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
        std::panic::set_hook(previous);
        let payload = panicked.expect_err("the detector must fire in debug builds");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default()
            .to_string();
        assert!(
            message.contains("inspector rewrote a failed contract creation into a successful one"),
            "the assertion must name the shape it caught; got {message:?}",
        );
    } else {
        let error = run().expect_err("the refusal must surface as an EVMError in release builds");
        assert!(
            error.contains("inspector rewrote a failed contract creation into a successful one"),
            "the error must name the shape it caught; got {error:?}",
        );
    }
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

    let code = BytecodeBuilder::default()
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
    let db = base_db(code).account_code(CALLEE, plain_run_code(20));

    let mut inspector = HaltingInterceptor::default();
    let inspected = transact_inspected(db, default_limits(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert!(inspected.result.is_success(), "the caller absorbs the halt: {:?}", inspected.result);
    assert_eq!(
        inspected.destroyed, inspector.forwarded,
        "the whole intercepted envelope is destroyed — nothing hands it back",
    );
    assert_eq!(
        inspected.compute_gas,
        inspected.enforced + inspected.destroyed,
        "and it is reported without being enforced",
    );
    assert_identity("intercepted halt", &inspected);
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

    let mut builder = BytecodeBuilder::default();
    for (offset, byte) in init_code.iter().enumerate() {
        builder = builder
            .push_number(u64::from(*byte))
            .push_number(offset as u64)
            .append(revm::bytecode::opcode::MSTORE8);
    }
    let code = builder
        .push_number(init_code.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build();

    let deployed = CONTRACT.create(0);

    // The uninspected run deploys, so the rewrite has something to undo.
    let mut observer = Observer::default();
    let plain = transact_inspected(base_db(code.clone()), default_limits(), &mut observer);
    assert!(plain.result.is_success(), "fixture check: {:?}", plain.result);
    let deployed_account = plain.state.get(&deployed).expect("the fixture must deploy a contract");
    assert!(
        !deployed_account.info.is_empty_code_hash(),
        "the fixture must deploy code for the rewrite to have something to undo",
    );

    let mut killer = CreateKiller::default();
    let killed = transact_inspected(base_db(code), default_limits(), &mut killer);

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
    assert_identity("killed creation", &killed);
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
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let plain = transact_plain(build_db(), default_limits());
    let mut inspector = Observer::default();
    let inspected = transact_inspected(build_db(), default_limits(), &mut inspector);

    assert!(inspector.steps > 0, "the fixture must actually run opcodes under the inspector");
    assert_eq!(inspector.calls, 2, "one top-level frame plus one inner call");
    assert_eq!(inspector.call_ends, 2, "every call must be paired");

    assert!(
        inspected.ledger.is_zero(),
        "an observation-only inspector must leave an empty ledger; got {:?}",
        inspected.ledger,
    );
    assert_eq!(format!("{:?}", inspected.result), format!("{:?}", plain.result));
    assert_eq!(inspected.compute_gas, plain.compute_gas);
    assert_eq!(inspected.enforced, plain.enforced);
    assert_eq!(inspected.destroyed, plain.destroyed);
    assert_eq!(inspected.data_size, plain.data_size);
    assert_eq!(inspected.kv_updates, plain.kv_updates);
    assert_eq!(inspected.state_growth, plain.state_growth);
    assert_eq!(inspected.gas_used, plain.gas_used);
    assert_eq!(inspected.total_gas_spent, plain.total_gas_spent);
    assert_eq!(inspected.terms, plain.terms);
    assert_eq!(inspected.state, plain.state, "the produced state must be identical");
    assert_identity("observed", &inspected);
    assert_identity("plain", &plain);
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
    let db = base_db(code).account_code(CALLEE, callee);

    let plain = transact_plain(db, default_limits());

    assert!(plain.result.is_success(), "fixture check: {:?}", plain.result);
    assert!(
        plain.terms.non_compute_gas > 0,
        "fixture check: the transaction must have moved a lane other than compute",
    );
    assert_eq!(
        plain.ledger,
        InspectorLedger::default(),
        "no inspector ran, so every lane must be untouched",
    );
    assert_eq!(plain.terms.inspector_conjured_gas, 0, "and the law's inspector term must be zero");
    assert_identity("uninspected", &plain);
}
