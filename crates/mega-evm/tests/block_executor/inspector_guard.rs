//! The canonical block-execution path admits no transaction an inspector took part in.
//!
//! `MegaETH` supports rewriting inspectors in full — the measurement shim books what they do and
//! the conservation law accounts for it — but supporting a rewrite is not the same as letting it
//! into a block. Block production and block validation have to produce the same numbers for the
//! same block on every node, and an inspector is one node's configuration: what it writes into a
//! gas counter reaches the receipt, the transaction's reported compute total, and through it the
//! block's cumulative counters.
//!
//! It can also rewrite what a frame *did* — its classification, its output, or the frame itself,
//! answered with a synthetic outcome — which moves no gas anywhere and reaches the transaction's
//! state and its receipt directly. A gas-only criterion would admit every one of those, so the
//! criterion is the whole ledger: no gas the EVM did not move, and nothing the shim was handed
//! coming back changed.
//!
//! So every entry on the canonical path — the two that run a transaction and the one funnel that
//! admits a result — refuses a non-zero ledger. The refusal is an error rather than an assertion,
//! because it is a boundary held against an embedder and has to hold in the binaries that build
//! and validate blocks; the tests here therefore pass identically in debug and release builds.
//!
//! The green half matters as much as the red: every inspector on this path today is a tracer, and
//! a tracer must keep working. That is what the observation tests pin.

use std::convert::Infallible;

use alloy_evm::{block::BlockExecutor, EvmEnv};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxHash, TxKind, B256, U256};
use mega_evm::{
    alloy_consensus::{transaction::Recovered, Signed, TxLegacy},
    alloy_evm::block::BlockExecutionError,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, InspectorLedger, Lane, MegaBlockExecutionCtx, MegaBlockExecutorFactory,
    MegaEvmFactory, MegaHardforkConfig, MegaSpecId, MegaTransactionNew as _, MegaTxEnvelope,
    TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{CALL, POP, STOP},
    context::{BlockEnv, ContextTr},
    database::State,
    interpreter::{CallInputs, CallOutcome, InstructionResult, Interpreter, InterpreterTypes},
    Inspector,
};

/// Sends every transaction in these tests.
const CALLER: Address = address!("2000000000000000000000000000000000000002");
/// A callee with enough plain opcodes for an inspector to land an edit mid-run.
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");
/// A second callee, so a tracer has a nested frame to record.
const CALLEE: Address = address!("1000000000000000000000000000000000000002");

/// Gas the injecting inspector writes into the interpreter's counter.
const INJECTED: u64 = 7_000;

/// Refund the refund-writing inspector records.
const REFUNDED: i64 = 3_000;

/// Writes gas into the running interpreter's counter, once — the smallest rewrite that moves the
/// ledger's [`gas`](InspectorLedger::gas) lane.
#[derive(Default)]
struct GasInjector {
    applied: bool,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for GasInjector {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.applied {
            return;
        }
        self.applied = true;
        interp.gas.erase_cost(INJECTED);
    }
}

/// Rewrites the classification of the fixture's inner call, once — the smallest rewrite that moves
/// no gas at all.
///
/// Every one of the ledger's gas lanes stays at zero under this inspector: the call's remaining
/// gas, its envelope and every interpreter counter are exactly what the EVM left. What changes is
/// what the transaction did — the callee's storage write is rolled back and the caller reads a
/// failure — which is why the guard cannot be a gas-only check.
#[derive(Default)]
struct CallFailer {
    applied: bool,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for CallFailer {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if self.applied || inputs.target_address != CALLEE {
            return;
        }
        self.applied = true;
        outcome.result.result = InstructionResult::Revert;
    }
}

/// Writes a refund into the running interpreter's counter, once — the rewrite that moves what the
/// sender pays without moving the envelope at all.
///
/// Every gas lane stays at zero under this inspector, and so does the conservation law: the law is
/// stated over `total_gas_spent`, which is `limit - remaining`, and a refund enters neither term.
/// What moves is the receipt's `gas_used`, which is the number the sender is billed on — so a
/// gas-lane criterion would admit it and two nodes would disagree about a receipt.
#[derive(Default)]
struct RefundWriter {
    applied: bool,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for RefundWriter {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if self.applied {
            return;
        }
        self.applied = true;
        interp.gas.record_refund(REFUNDED);
    }
}

/// Counts callbacks and changes nothing — the shape every tracer in production has.
#[derive(Default)]
struct Observer {
    steps: u64,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Observer {
    fn step(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.steps += 1;
    }
}

fn envelope(nonce: u64) -> MegaTxEnvelope {
    let tx = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit: 1_000_000,
        to: TxKind::Call(CONTRACT),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO))
}

fn build_db() -> MemoryDatabase {
    let mut code = BytecodeBuilder::default();
    for _ in 0..16 {
        code = code.push_number(1u64).append(POP);
    }
    let code = code
        .sstore(U256::from(1), U256::from(9))
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(50_000u64) // gas
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, code);
    db.set_account_code(CALLEE, BytecodeBuilder::default().stop().build());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000_000u64));
    db
}

fn evm_env(spec: MegaSpecId) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.set_spec_and_mainnet_gas_params(spec);
    EvmEnv::new(
        cfg_env,
        BlockEnv {
            number: U256::from(1000),
            timestamp: U256::from(1_800_000_000),
            gas_limit: 30_000_000,
            ..Default::default()
        },
    )
}

fn executor_factory(
    spec: MegaSpecId,
) -> MegaBlockExecutorFactory<
    MegaHardforkConfig,
    MegaEvmFactory<TestExternalEnvs<Infallible>>,
    OpAlloyReceiptBuilder,
> {
    MegaBlockExecutorFactory::new(
        MegaHardforkConfig::default().with_all_activated_through(spec),
        MegaEvmFactory::new().with_external_env_factory(TestExternalEnvs::<Infallible>::new()),
        OpAlloyReceiptBuilder::default(),
    )
}

fn block_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits())
}

/// Unwraps the refusal, checking it is the one this module is about and that it names the
/// transaction and the measurement it refused over.
///
/// Reached by downcast rather than by matching the message: the error crosses the `alloy_evm`
/// boundary as a boxed `dyn Error`, and a consumer that wants to react to it — a sequencer that
/// would rather drop the transaction than fail the block — has to get the typed value back.
#[track_caller]
fn expect_refusal(err: &BlockExecutionError, expected_hash: TxHash) -> InspectorLedger {
    let internal = err.as_internal().unwrap_or_else(|| {
        panic!("the refusal must be an internal error, not a verdict on the transaction: {err:?}")
    });
    let other = internal
        .as_other()
        .unwrap_or_else(|| panic!("the refusal must carry MegaETH's own error: {internal:?}"));
    let mega = other
        .downcast_ref::<mega_evm::MegaBlockExecutionError>()
        .unwrap_or_else(|| panic!("the refusal must survive the boxing as a typed value: {other}"));
    let mega_evm::MegaBlockExecutionError::InspectorAdjustedAccounting { tx_hash, ledger } = mega;
    assert_eq!(*tx_hash, expected_hash, "the refusal must name the transaction it refused");
    assert!(!ledger.is_zero(), "a refusal over an empty ledger is a refusal of nothing");
    *ledger
}

/// The producer entry: a transaction an inspector adjusted never becomes an outcome the block
/// path will hand back.
///
/// The rewrite is booked, the transaction itself executes fine, and the refusal comes from the
/// executor rather than from the EVM — which is the whole point, since the EVM is required to keep
/// supporting the rewrite.
#[test]
fn test_run_transaction_refuses_an_inspector_adjusted_transaction() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor_with_inspector(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
        GasInjector::default(),
    );

    let tx = envelope(0);
    let err = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect_err("the canonical path must refuse an inspector-adjusted transaction");

    assert!(executor.evm().inspector.applied, "the fixture must reach the injection point");
    let ledger = expect_refusal(&err, *tx.hash());
    assert_eq!(
        ledger.gas,
        Lane::once(i128::from(INJECTED)),
        "the refusal must carry what was actually injected, so a caller can see the size of it",
    );
    assert_eq!(ledger.env, Lane::default(), "no frame envelope was touched");
    assert_eq!(
        executor.block_limiter.block_compute_gas_used, 0,
        "a refused transaction must leave the block's counters where they were",
    );
}

/// The refusal is over the whole ledger, not over the lanes the conservation law reads.
///
/// A refund rewrite is the shape that makes the distinction load-bearing: it leaves every gas lane
/// at zero, closes the law exactly as an uninspected run does, and still changes the number the
/// sender is billed. A node that ran this inspector and a node that did not would build the same
/// block with different receipts.
#[test]
fn test_run_transaction_refuses_a_refund_rewrite() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor_with_inspector(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
        RefundWriter::default(),
    );

    let tx = envelope(0);
    let err = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect_err("the canonical path must refuse a transaction whose receipt was rewritten");

    assert!(executor.evm().inspector.applied, "the fixture must reach the refund write");
    let ledger = expect_refusal(&err, *tx.hash());
    assert_eq!(
        ledger.refund,
        Lane::once(i128::from(REFUNDED)),
        "the refusal must carry the refund that was written",
    );
    assert_eq!(
        ledger.conjured_gas(),
        0,
        "no gas moved: a gas-only criterion would have admitted this transaction",
    );
    assert_eq!(
        executor.block_limiter.block_compute_gas_used, 0,
        "a refused transaction must leave the block's counters where they were",
    );
}

/// The other producer entry, reached through the `alloy_evm` trait rather than the inherent
/// method: the two resolve their transaction sizes differently and share no body, so a guard on
/// one says nothing about the other.
#[test]
fn test_execute_transaction_without_commit_refuses_it_too() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor_with_inspector(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
        GasInjector::default(),
    );

    let tx = envelope(0);
    let err = executor
        .execute_transaction_without_commit(&Recovered::new_unchecked(&tx, CALLER))
        .expect_err("the trait entry must refuse it as well");

    assert_eq!(expect_refusal(&err, *tx.hash()).gas, Lane::once(i128::from(INJECTED)));
    assert!(executor.receipts.is_empty(), "nothing may have been recorded");
}

/// The consumer entry: a result whose adjustment was not made by *this* executor is refused at
/// the commit funnel, before it can touch anything.
///
/// This is the entry that has to hold. Execution and commit are separate steps — the parallel
/// executor speculatively runs many transactions and commits the survivors one by one — so a
/// result arriving here may have been produced by a different executor instance, or built by
/// hand. The producer-side guards cannot see any of those; the outcome's own ledger can.
#[test]
fn test_commit_refuses_a_result_an_inspector_took_part_in() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
    );

    let tx = envelope(0);
    let mut outcome = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect("fixture check: an uninspected run must be admitted");
    assert!(
        outcome.inner.inspector_ledger.is_zero(),
        "fixture check: an uninspected run reports an empty ledger",
    );

    // The shape a result produced elsewhere arrives in: the numbers are execution's, the ledger
    // says an inspector moved some of them.
    outcome.inner.inspector_ledger = InspectorLedger { gas: Lane::once(1), ..Default::default() };

    let err =
        executor.commit_transaction_outcome(outcome).expect_err("the commit funnel must refuse it");

    assert_eq!(expect_refusal(&err, *tx.hash()).gas, Lane::once(1));
    assert!(executor.receipts.is_empty(), "no receipt may have been pushed");
    assert_eq!(
        executor.block_limiter.block_gas_used, 0,
        "and no limiter counter may have been advanced",
    );
    assert!(
        executor.take_pending_commit_error().is_none(),
        "the fallible entry reports rather than latches, so the executor stays usable",
    );
}

/// The infallible commit hook has no way to report the refusal, so it latches it and the block
/// fails at `finish` — the same contract it already holds for a late block-limit rejection.
#[test]
fn test_the_infallible_commit_hook_latches_the_refusal() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
    );

    let tx = envelope(0);
    let outcome = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect("fixture check: an uninspected run must be admitted");
    let mut result = mega_evm::MegaBlockTxResult {
        tx_type: tx.tx_type(),
        tx_hash: *tx.hash(),
        gas_limit: 1_000_000,
        tx_size: outcome.tx_size,
        da_size: outcome.da_size,
        depositor: outcome.depositor,
        inner: outcome.inner,
    };
    result.inner.inspector_ledger = InspectorLedger { env: Lane::once(-5), ..Default::default() };

    let gas = executor.commit_transaction(result);
    assert_eq!(gas.tx_gas_used(), 0, "a transaction that contributed nothing must report zero gas",);
    let latched = executor
        .pending_commit_error()
        .expect("the refusal must be latched where `finish` will find it");
    assert_eq!(expect_refusal(latched, *tx.hash()).env, Lane::once(-5));

    let err = executor.finish().expect_err("the block must not finish over a latched refusal");
    expect_refusal(&err, *tx.hash());
}

/// The guard governs the configuration a block is built with, which no historical block covers, so
/// it is not gated on a spec — the same rewrite is refused on a frozen one.
///
/// The measurement it reads is spec-independent too, for its own reason: the shim books what an
/// inspector writes into a gas counter whether or not the spec has a lane the write could make
/// unsound.
#[test]
fn test_the_guard_is_not_spec_gated() {
    for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX4, MegaSpecId::REX6, MegaSpecId::REX7] {
        let mut db = build_db();
        let mut state = State::builder().with_database(&mut db).build();
        let mut executor = executor_factory(spec).create_executor_with_inspector(
            &mut state,
            block_ctx(),
            evm_env(spec),
            GasInjector::default(),
        );

        let tx = envelope(0);
        let err = executor
            .run_transaction(Recovered::new_unchecked(&tx, CALLER))
            .err()
            .unwrap_or_else(|| panic!("{spec:?}: the rewrite must be refused on every spec"));
        assert_eq!(
            expect_refusal(&err, *tx.hash()).gas,
            Lane::once(i128::from(INJECTED)),
            "{spec:?}: and the measurement it is refused over must be the same one",
        );
    }
}

/// The green half: an observation-only inspector is left alone, and the block it helps build is
/// bit-identical to the one built without it.
///
/// Every inspector on this path today is a tracer. If the guard could not tell one from a
/// rewriting inspector, it would take tracing off block production entirely.
#[test]
fn test_an_observing_inspector_still_builds_a_block() {
    let build = |observe: bool| {
        let mut db = build_db();
        let mut state = State::builder().with_database(&mut db).build();
        let tx = envelope(0);
        let factory = executor_factory(MegaSpecId::REX7);
        let (gas_used, steps) = if observe {
            let mut executor = factory.create_executor_with_inspector(
                &mut state,
                block_ctx(),
                evm_env(MegaSpecId::REX7),
                Observer::default(),
            );
            let outcome = executor
                .run_transaction(Recovered::new_unchecked(&tx, CALLER))
                .expect("an observing inspector must not be refused");
            assert!(outcome.inner.inspector_ledger.is_zero(), "and must leave an empty ledger");
            let gas = executor.commit_transaction_outcome(outcome).expect("nor at commit");
            let steps = executor.evm().inspector.steps;
            let (_, result) = executor.finish().expect("the block must finish");
            assert_eq!(result.receipts.len(), 1, "the observed block still has its receipt");
            (gas, steps)
        } else {
            let mut executor =
                factory.create_executor(&mut state, block_ctx(), evm_env(MegaSpecId::REX7));
            let outcome = executor
                .run_transaction(Recovered::new_unchecked(&tx, CALLER))
                .expect("the reference run must be admitted");
            let gas = executor.commit_transaction_outcome(outcome).expect("and committed");
            let (_, result) = executor.finish().expect("the block must finish");
            assert_eq!(result.receipts.len(), 1);
            (gas, 0)
        };
        (gas_used, steps)
    };

    let (observed_gas, steps) = build(true);
    let (plain_gas, _) = build(false);
    assert!(steps > 0, "the fixture must actually have observed something");
    assert_eq!(observed_gas, plain_gas, "observation must not move a single unit of gas");
}

/// A pre- or post-block system call never runs the inspector, so it is not an entry the guard has
/// to cover.
///
/// Two independent reasons, and this pins the one that is not visible from the block executor's
/// own signatures. Structurally, a system call produces a `ResultAndState` rather than a
/// `MegaTransactionOutcome`, and the ledger is reset at the start of every transaction, so nothing
/// a system call booked could reach a transaction's outcome anyway. Underneath that, the system
/// call path takes revm's plain frame loop rather than the inspecting one — which is what this
/// runs to find out, rather than reading it off upstream's source.
#[test]
fn test_a_system_call_does_not_run_the_inspector() {
    use revm::SystemCallEvm as _;

    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor_with_inspector(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
        GasInjector::default(),
    );

    let result = executor
        .evm_mut()
        .system_call(CONTRACT, Bytes::new())
        .expect("the system call must not surface an EVMError");

    assert!(result.result.is_success(), "fixture check: the callee must have run, got {result:?}",);
    assert!(!executor.evm().inspector.applied, "a system call must not reach the inspector at all",);
}

/// The real tracer that `mega-evme replay` attaches to this exact path is admitted, and the block
/// it observes is the one built without it.
///
/// The `Observer` above is a fixture; this is the production shape. `TracingInspector` receives
/// every callback the shim measures — including the ones handed a live interpreter and the ones
/// handed a frame's inputs — so if observation could move a lane by accident, it would move one
/// here. Run rather than reasoned about: the guard's blast radius is only acceptable if the
/// inspectors that exist today pass it.
#[test]
fn test_the_production_tracer_is_admitted() {
    use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor_with_inspector(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
        TracingInspector::new(TracingInspectorConfig::all()),
    );

    let tx = envelope(0);
    let outcome = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect("the tracer every replay uses must not be refused");

    assert!(outcome.result.is_success(), "fixture check: {:?}", outcome.result);
    assert!(
        outcome.inner.inspector_ledger.is_zero(),
        "a tracer must leave every lane untouched; got {:?}",
        outcome.inner.inspector_ledger,
    );
    executor.commit_transaction_outcome(outcome).expect("nor at commit");

    assert!(
        executor.evm().inspector.traces().nodes().len() >= 2,
        "fixture check: the tracer must have recorded the nested frame it was given",
    );
    let (_, result) = executor.finish().expect("the block must finish");
    assert_eq!(result.receipts.len(), 1);
}

/// The rewrite a gas-only guard could not see: a frame's classification, changed at `call_end`.
///
/// Nothing moves. The call's remaining gas, its envelope and every interpreter counter are the
/// EVM's own, so all three gas lanes read zero — and yet the callee's write is rolled back and the
/// caller is handed a failure, which is a different transaction with a different state root. The
/// intervention counter is the only thing standing between this and admission.
#[test]
fn test_a_rewrite_that_moves_no_gas_is_refused_too() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor_with_inspector(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
        CallFailer::default(),
    );

    let tx = envelope(0);
    let err = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect_err("a classification rewrite must be refused like any other");

    assert!(executor.evm().inspector.applied, "the fixture must reach the rewrite point");
    let ledger = expect_refusal(&err, *tx.hash());
    assert_eq!(
        (ledger.gas, ledger.env, ledger.result),
        (Lane::default(), Lane::default(), Lane::default()),
        "the point of this shape is that no gas lane moves; got {ledger:?}",
    );
    assert_eq!(ledger.interventions, 1, "the rewrite must be the thing the refusal names");
    assert_eq!(
        executor.block_limiter.block_compute_gas_used, 0,
        "a refused transaction must leave the block's counters where they were",
    );
}

/// An EVM driven off the canonical path is not covered by the guard, however much its inspector
/// rewrites.
///
/// The guard sits on the block executor's entries, not on `MegaEvm`, and this is what makes that a
/// property rather than an accident of the current call graph. It is what leaves a simulation EVM
/// — the oracle set-slot preflight the node runs before it publishes a value, and anything else an
/// embedder drives itself — free to attach a rewriting inspector: such a run never produces a
/// block, so there is nothing for two nodes to disagree about.
///
/// The same transaction is refused by the executor in
/// [`test_a_rewrite_that_moves_no_gas_is_refused_too`], so the two together say the boundary is
/// where it is claimed to be rather than nowhere.
#[test]
fn test_an_off_path_evm_runs_the_same_rewrite_to_completion() {
    let mut db = build_db();
    let mut inspector = CallFailer::default();
    let mut evm = mega_evm::MegaEvm::new(
        mega_evm::MegaContext::new(&mut db, MegaSpecId::REX7)
            .with_tx_runtime_limits(mega_evm::EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)),
    )
    .with_inspector(&mut inspector);

    let mut tx = mega_evm::MegaTransaction::new(
        revm::context::tx::TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(1_000_000)
            .build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = evm.execute_transaction(tx).expect("the EVM supports the rewrite in full");

    assert!(inspector.applied, "the fixture must reach the rewrite point");
    assert_eq!(
        outcome.inspector_ledger.interventions, 1,
        "the rewrite is still measured and reported — it is simply not refused here",
    );
    assert!(outcome.result_and_state.result.is_success(), "and the transaction still completes");
}
