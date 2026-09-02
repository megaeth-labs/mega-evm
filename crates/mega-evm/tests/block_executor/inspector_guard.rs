//! The canonical block-execution path admits an inspected transaction only on a declaration.
//!
//! `MegaETH` supports rewriting inspectors in full — the measurement shim books what they do and
//! the conservation law accounts for it — but supporting a rewrite is not the same as letting it
//! into a block. Block production and block validation have to produce the same numbers for the
//! same block on every node, and an inspector is one node's configuration: what it writes into a
//! gas counter reaches the receipt, the transaction's reported compute total, and through it the
//! block's cumulative counters.
//!
//! What it can also do is reach past every boundary the shim watches. Editing the contents of the
//! interpreter's stack or its memory, or writing the journal directly, changes what the
//! transaction produces and leaves every lane of the ledger at zero — so an empty ledger cannot be
//! what a block is admitted on. What can is a `TrustedObserver` declaration: a line written in
//! source, about one concrete type, by someone who had read it.
//!
//! So every entry on the canonical path — the two that run a transaction and the one funnel that
//! admits a result — refuses an inspector its type never declared, *before* running it. The ledger
//! is kept as the backstop behind that: it catches a declaration that did not hold, and a result
//! that reaches the commit funnel already carrying a rewrite from somewhere this executor cannot
//! see. Both refusals are errors rather than assertions, because they are boundaries held against
//! an embedder and have to hold in the binaries that build and validate blocks; the tests here
//! therefore pass identically in debug and release builds.
//!
//! The green half matters as much as the red: every inspector on this path today is a tracer, and
//! a tracer must keep working. That is what the declared-observer tests pin.

use std::convert::Infallible;

use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    EvmEnv, EvmFactory,
};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Log, Signature, TxHash, TxKind, B256, U256};
use mega_evm::{
    alloy_consensus::{transaction::Recovered, Signed, TxLegacy},
    alloy_evm::block::BlockExecutionError,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, InspectorLedger, Lane, MegaBlockExecutionCtx, MegaBlockExecutorFactory,
    MegaEvmFactory, MegaHardforkConfig, MegaSpecId, MegaTransactionNew as _,
    MegaTransactionOutcome, MegaTxEnvelope, TestExternalEnvs, TrustedObserver,
};
use revm::{
    bytecode::opcode::{CALL, POP, STOP},
    context::{BlockEnv, Cfg, ContextTr},
    database::State,
    handler::FrameResult,
    inspector::NoOpInspector,
    interpreter::{
        interpreter_types::MemoryTr, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        FrameInput, InstructionResult, Interpreter, InterpreterTypes,
    },
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
/// failure — which is why the ledger cannot be a gas-only check.
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

/// Grows the frame's memory and the memo of how far it has been paid for, once — the rewrite that
/// reaches through no argument the shim is handed at all.
///
/// Neither half is a rewrite on its own: moving the memo alone leaves the EVM reading out of
/// bounds, moving the memory alone leaves the growth charged for twice. Moving both leaves the
/// interpreter in a state it could have reached by paying, having paid nothing, and the next
/// expanding opcode inside the new bound is charged nothing at all. No gas moves at the moment the
/// edit is made, no frame input and no frame result exists, and the pending action is untouched —
/// so this is the shape the constant-time working-set reading exists for.
#[derive(Default)]
struct MemoryGrower {
    applied: bool,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for MemoryGrower {
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        if self.applied {
            return;
        }
        let words = interp.memory.size() / 32 + 1;
        if !interp.memory.resize(words * 32) {
            return;
        }
        // Priced through revm's own table, so the memo is exactly what the EVM would have written
        // had the frame paid for the growth.
        let cost = context.cfg().gas_params().memory_cost(words);
        interp.gas.memory_mut().set_words_num(words, cost);
        self.applied = true;
    }
}

/// Counts callbacks and changes nothing — the shape every tracer in production has, with no
/// declaration about its type.
#[derive(Default)]
struct Observer {
    steps: u64,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Observer {
    fn step(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.steps += 1;
    }
}

/// The same observer, with its author's declaration that it writes nothing back.
///
/// A separate type rather than a declaration on [`Observer`], because the pair is the experiment:
/// the two behave identically and only one of them is admitted, which is what says the criterion
/// is the declaration and not the behaviour.
#[derive(Default)]
struct DeclaredObserver {
    inner: Observer,
}

impl TrustedObserver for DeclaredObserver {}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for DeclaredObserver {
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.inner.step(interp, context);
    }
}

/// A read-only declaration around `revm-inspectors`' geth tracer.
///
/// The orphan rule keeps `TrustedObserver` from being implemented for `TracingInspector` directly
/// — from a downstream node both are foreign — so the declaration is made about a local newtype
/// that forwards every callback unchanged. `bin/mega-evme`'s replay command carries the same shape
/// for the same reason, and it is the shape a node writes to keep tracing block production.
struct TrustedTracer(revm_inspectors::tracing::TracingInspector);

impl TrustedObserver for TrustedTracer {}

impl<CTX, INTR> Inspector<CTX, INTR> for TrustedTracer
where
    INTR: InterpreterTypes,
    revm_inspectors::tracing::TracingInspector: Inspector<CTX, INTR>,
{
    fn initialize_interp(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.0.initialize_interp(interp, context);
    }

    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.0.step(interp, context);
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.0.step_end(interp, context);
    }

    fn log(&mut self, context: &mut CTX, log: Log) {
        self.0.log(context, log);
    }

    fn log_full(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX, log: Log) {
        self.0.log_full(interp, context, log);
    }

    fn frame_start(
        &mut self,
        context: &mut CTX,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        self.0.frame_start(context, frame_input)
    }

    fn frame_end(
        &mut self,
        context: &mut CTX,
        frame_input: &FrameInput,
        frame_result: &mut FrameResult,
    ) {
        self.0.frame_end(context, frame_input, frame_result);
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.0.call(context, inputs)
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.0.call_end(context, inputs, outcome);
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.0.create(context, inputs)
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.0.create_end(context, inputs, outcome);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.0.selfdestruct(contract, target, value);
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

/// Unwraps `MegaETH`'s own error out of the `alloy_evm` boxing.
///
/// Reached by downcast rather than by matching the message: the error crosses the `alloy_evm`
/// boundary as a boxed `dyn Error`, and a consumer that wants to react to it — a sequencer that
/// would rather drop the transaction than fail the block — has to get the typed value back.
#[track_caller]
fn expect_mega_error(err: &BlockExecutionError) -> &mega_evm::MegaBlockExecutionError {
    let internal = err.as_internal().unwrap_or_else(|| {
        panic!("the refusal must be an internal error, not a verdict on the transaction: {err:?}")
    });
    let other = internal
        .as_other()
        .unwrap_or_else(|| panic!("the refusal must carry MegaETH's own error: {internal:?}"));
    other
        .downcast_ref::<mega_evm::MegaBlockExecutionError>()
        .unwrap_or_else(|| panic!("the refusal must survive the boxing as a typed value: {other}"))
}

/// Asserts the refusal is the admission rule's, and that it names the transaction.
#[track_caller]
fn expect_undeclared(err: &BlockExecutionError, expected_hash: TxHash) {
    match expect_mega_error(err) {
        mega_evm::MegaBlockExecutionError::UndeclaredInspector { tx_hash } => {
            assert_eq!(*tx_hash, expected_hash, "the refusal must name the transaction it refused");
        }
        other => panic!("expected the undeclared-inspector refusal, got {other:?}"),
    }
}

/// Asserts the refusal is the ledger backstop's, and returns what it was refused over.
#[track_caller]
fn expect_adjusted(err: &BlockExecutionError, expected_hash: TxHash) -> InspectorLedger {
    match expect_mega_error(err) {
        mega_evm::MegaBlockExecutionError::InspectorAdjustedAccounting { tx_hash, ledger } => {
            assert_eq!(*tx_hash, expected_hash, "the refusal must name the transaction it refused");
            assert!(!ledger.is_zero(), "a refusal over an empty ledger is a refusal of nothing");
            **ledger
        }
        other => panic!("expected the ledger backstop's refusal, got {other:?}"),
    }
}

/// Runs the fixture transaction on an EVM the test drives itself, with `inspector` attached.
///
/// This is the path an embedder keeps: `MegaEvm` supports a rewriting inspector in full and
/// reports what it did. Every rewrite shape below is measured here and refused at the block
/// executor's entries, which is what says the boundary is where it is claimed to be.
fn run_off_path<I>(inspector: I) -> MegaTransactionOutcome
where
    I: for<'a> Inspector<mega_evm::MegaContext<&'a mut MemoryDatabase, mega_evm::EmptyExternalEnv>>,
{
    let mut db = build_db();
    let mut evm = mega_evm::MegaEvm::new(
        mega_evm::MegaContext::new(&mut db, MegaSpecId::REX7)
            .with_tx_runtime_limits(mega_evm::EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)),
    )
    .with_inspector(inspector);

    let mut tx = mega_evm::MegaTransaction::new(
        revm::context::tx::TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(1_000_000)
            .build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    evm.execute_transaction(tx).expect("the EVM supports the rewrite in full")
}

/// The producer entry: an inspector nobody declared read-only never runs on this path at all.
///
/// The inspector here only observes, and is refused anyway. That is the whole change of criterion:
/// what a transaction is admitted on is what the inspector's type promises, not what this
/// particular run was measured to have done — because the measurement cannot see an edit made to
/// the interpreter's stack contents or straight into the journal.
#[test]
fn test_run_transaction_refuses_an_undeclared_inspector() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let factory = executor_factory(MegaSpecId::REX7);
    let evm = factory
        .evm_factory()
        .create_evm(&mut state, evm_env(MegaSpecId::REX7))
        .with_inspector(Observer::default());
    let mut executor = <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
        &factory,
        evm,
        block_ctx(),
    );

    let tx = envelope(0);
    let err = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect_err("the canonical path must refuse an undeclared inspector");

    expect_undeclared(&err, *tx.hash());
    assert_eq!(
        executor.evm().inspector.steps,
        0,
        "the refusal must come before execution: an undeclared inspector does not get to run",
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
    let factory = executor_factory(MegaSpecId::REX7);
    let evm = factory
        .evm_factory()
        .create_evm(&mut state, evm_env(MegaSpecId::REX7))
        .with_inspector(GasInjector::default());
    let mut executor = <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
        &factory,
        evm,
        block_ctx(),
    );

    let tx = envelope(0);
    let err = executor
        .execute_transaction_without_commit(&Recovered::new_unchecked(&tx, CALLER))
        .expect_err("the trait entry must refuse it as well");

    expect_undeclared(&err, *tx.hash());
    assert!(!executor.evm().inspector.applied, "and must refuse before the inspector runs");
    assert!(executor.receipts.is_empty(), "nothing may have been recorded");
}

/// The consumer entry: a result whose producer this executor never saw is refused at the commit
/// funnel, before it can touch anything.
///
/// This is the entry that has to hold. Execution and commit are separate steps — the parallel
/// executor speculatively runs many transactions and commits the survivors one by one — so a
/// result arriving here may have been produced by a different executor instance, by an embedder
/// driving `MegaEvm` itself, or built by hand. What the outcome carries is the only thing the
/// funnel can read.
#[test]
fn test_commit_refuses_a_result_produced_under_an_undeclared_inspector() {
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
        !outcome.inner.undeclared_inspector,
        "fixture check: an uninspected run carries no inspector to declare",
    );

    // The shape a result produced elsewhere arrives in: the numbers are execution's, and the
    // outcome says an inspector nobody declared took part in producing them.
    outcome.inner.undeclared_inspector = true;

    let err =
        executor.commit_transaction_outcome(outcome).expect_err("the commit funnel must refuse it");

    expect_undeclared(&err, *tx.hash());
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

/// The backstop: a result that says nothing about its inspector, and carries a ledger that does.
///
/// The declaration covers the executor's own producers; it cannot cover a result built by hand or
/// produced by a version of the pipeline that did not fill the field in. The ledger is what is
/// left, and it is read at the same funnel.
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

    outcome.inner.inspector_ledger = InspectorLedger { gas: Lane::once(1), ..Default::default() };

    let err =
        executor.commit_transaction_outcome(outcome).expect_err("the commit funnel must refuse it");

    assert_eq!(expect_adjusted(&err, *tx.hash()).gas, Lane::once(1));
    assert!(executor.receipts.is_empty(), "no receipt may have been pushed");
    assert_eq!(
        executor.block_limiter.block_gas_used, 0,
        "and no limiter counter may have been advanced",
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
    assert_eq!(expect_adjusted(latched, *tx.hash()).env, Lane::once(-5));

    let err = executor.finish().expect_err("the block must not finish over a latched refusal");
    expect_adjusted(&err, *tx.hash());
}

/// The refusal governs the configuration a block is built with, which no historical block covers,
/// so it is not gated on a spec — the same inspector is refused on a frozen one.
#[test]
fn test_the_refusal_is_not_spec_gated() {
    for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX4, MegaSpecId::REX6, MegaSpecId::REX7] {
        let mut db = build_db();
        let mut state = State::builder().with_database(&mut db).build();
        let factory = executor_factory(spec);
        let evm = factory
            .evm_factory()
            .create_evm(&mut state, evm_env(spec))
            .with_inspector(GasInjector::default());
        let mut executor =
            <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
                &factory,
                evm,
                block_ctx(),
            );

        let tx = envelope(0);
        let err = executor
            .run_transaction(Recovered::new_unchecked(&tx, CALLER))
            .err()
            .unwrap_or_else(|| panic!("{spec:?}: the inspector must be refused on every spec"));
        expect_undeclared(&err, *tx.hash());
    }
}

/// The green half: a declared observer is left alone, and the block it helps build is bit-identical
/// to the one built without it.
///
/// [`Observer`] and [`DeclaredObserver`] do exactly the same thing, and only the declared one gets
/// here — so this and [`test_run_transaction_refuses_an_undeclared_inspector`] together say the
/// criterion really is the declaration.
#[test]
fn test_a_declared_observer_still_builds_a_block() {
    let build = |observe: bool| {
        let mut db = build_db();
        let mut state = State::builder().with_database(&mut db).build();
        let tx = envelope(0);
        let factory = executor_factory(MegaSpecId::REX7);
        let (gas_used, steps) = if observe {
            let mut executor = factory.create_executor_with_trusted_inspector(
                &mut state,
                block_ctx(),
                evm_env(MegaSpecId::REX7),
                DeclaredObserver::default(),
            );
            let outcome = executor
                .run_transaction(Recovered::new_unchecked(&tx, CALLER))
                .expect("a declared observer must not be refused");
            assert!(!outcome.inner.undeclared_inspector, "and must report itself declared");
            assert!(outcome.inner.inspector_ledger.is_zero(), "and must leave an empty ledger");
            let gas = executor.commit_transaction_outcome(outcome).expect("nor at commit");
            let steps = executor.evm().inspector.inner.steps;
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

/// The inspector that observes nothing at all is declared, so the trivial configuration passes.
///
/// `NoOpInspector` is the one inspector this crate can declare for itself, and it is the shape a
/// caller reaches for when a code path needs an inspector-typed EVM without wanting one.
#[test]
fn test_the_no_op_inspector_is_admitted() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor_with_trusted_inspector(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
        NoOpInspector,
    );

    let tx = envelope(0);
    let outcome = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect("a declared inspector must not be refused");
    assert!(outcome.result.is_success(), "fixture check: {:?}", outcome.result);
    executor.commit_transaction_outcome(outcome).expect("nor at commit");
    let (_, result) = executor.finish().expect("the block must finish");
    assert_eq!(result.receipts.len(), 1);
}

/// The real tracer that `mega-evme replay` attaches to this exact path is admitted through its
/// declaration, and the block it observes is the one built without it.
///
/// The `DeclaredObserver` above is a fixture; this is the production shape, newtype and all.
/// `TracingInspector` receives every callback the shim measures — including the ones handed a live
/// interpreter and the ones handed a frame's inputs — so if observation could move a lane by
/// accident, it would move one here. Run rather than reasoned about: the refusal's blast radius is
/// only acceptable if the inspectors that exist today have a way through it.
#[test]
fn test_the_production_tracer_is_admitted() {
    use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor_with_trusted_inspector(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
        TrustedTracer(TracingInspector::new(TracingInspectorConfig::all())),
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
        executor.evm().inspector.0.traces().nodes().len() >= 2,
        "fixture check: the tracer must have recorded the nested frame it was given",
    );
    let (_, result) = executor.finish().expect("the block must finish");
    assert_eq!(result.receipts.len(), 1);
}

/// The bare `TracingInspector`, without the newtype, is refused — which is what makes the newtype
/// load-bearing rather than decorative.
#[test]
fn test_the_production_tracer_without_its_declaration_is_refused() {
    use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let factory = executor_factory(MegaSpecId::REX7);
    let evm = factory
        .evm_factory()
        .create_evm(&mut state, evm_env(MegaSpecId::REX7))
        .with_inspector(TracingInspector::new(TracingInspectorConfig::all()));
    let mut executor = <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
        &factory,
        evm,
        block_ctx(),
    );

    let tx = envelope(0);
    let err = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect_err("an undeclared tracer is an undeclared inspector");
    expect_undeclared(&err, *tx.hash());
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
    let factory = executor_factory(MegaSpecId::REX7);
    let evm = factory
        .evm_factory()
        .create_evm(&mut state, evm_env(MegaSpecId::REX7))
        .with_inspector(GasInjector::default());
    let mut executor = <MegaBlockExecutorFactory<_, _, _> as BlockExecutorFactory>::create_executor(
        &factory,
        evm,
        block_ctx(),
    );

    let result = executor
        .evm_mut()
        .system_call(CONTRACT, Bytes::new())
        .expect("the system call must not surface an EVMError");

    assert!(result.result.is_success(), "fixture check: the callee must have run, got {result:?}",);
    assert!(!executor.evm().inspector.applied, "a system call must not reach the inspector at all",);
}

/// An EVM driven off the canonical path is not covered by the refusal, however much its inspector
/// rewrites — and every rewrite it makes is still measured and reported.
///
/// The boundary sits on the block executor's entries, not on `MegaEvm`, and this is what makes
/// that a property rather than an accident of the current call graph. It is what leaves a
/// simulation EVM — the oracle set-slot preflight the node runs before it publishes a value, and
/// anything else an embedder drives itself — free to attach a rewriting inspector: such a run
/// never produces a block, so there is nothing for two nodes to disagree about.
///
/// Each shape below is one the ledger can see, and the four together are why the ledger is worth
/// keeping as a backstop even though admission no longer rests on it.
#[test]
fn test_an_off_path_evm_runs_a_gas_injection_to_completion() {
    let outcome = run_off_path(GasInjector::default());
    assert_eq!(
        outcome.inspector_ledger.gas,
        Lane::once(i128::from(INJECTED)),
        "the injection must be measured: {:?}",
        outcome.inspector_ledger,
    );
    assert!(outcome.undeclared_inspector, "and the outcome must carry what a block would refuse");
    assert!(outcome.result_and_state.result.is_success(), "and the transaction still completes");
}

/// A refund rewrite: every gas lane stays at zero, and the number the sender is billed moves.
#[test]
fn test_an_off_path_evm_runs_a_refund_rewrite_to_completion() {
    let outcome = run_off_path(RefundWriter::default());
    assert_eq!(
        outcome.inspector_ledger.refund,
        Lane::once(i128::from(REFUNDED)),
        "the refund must be measured: {:?}",
        outcome.inspector_ledger,
    );
    assert_eq!(
        outcome.inspector_ledger.conjured_gas(),
        0,
        "no gas moved: a gas-only criterion would not have seen this at all",
    );
    assert!(outcome.undeclared_inspector);
}

/// A classification rewrite: nothing moves, and the transaction's state is different.
#[test]
fn test_an_off_path_evm_runs_a_classification_rewrite_to_completion() {
    let outcome = run_off_path(CallFailer::default());
    assert_eq!(
        (
            outcome.inspector_ledger.gas,
            outcome.inspector_ledger.env,
            outcome.inspector_ledger.result
        ),
        (Lane::default(), Lane::default(), Lane::default()),
        "the point of this shape is that no gas lane moves; got {:?}",
        outcome.inspector_ledger,
    );
    assert_eq!(outcome.inspector_ledger.interventions, 1, "the rewrite must still be booked");
    assert!(outcome.undeclared_inspector);
}

/// A frame grown for free: the rewrite that reaches through nothing the shim is handed.
#[test]
fn test_an_off_path_evm_runs_a_free_memory_growth_to_completion() {
    let outcome = run_off_path(MemoryGrower::default());
    assert_eq!(
        (
            outcome.inspector_ledger.gas,
            outcome.inspector_ledger.env,
            outcome.inspector_ledger.result,
            outcome.inspector_ledger.refund,
        ),
        (Lane::default(), Lane::default(), Lane::default(), Lane::default()),
        "no gas lane can see this shape; got {:?}",
        outcome.inspector_ledger,
    );
    assert_eq!(outcome.inspector_ledger.interventions, 1, "the growth must still be booked");
    assert!(outcome.undeclared_inspector);
}

/// The default shim is `NoOpInspector`'s own declaration, not an undeclared wrapper around it.
///
/// `Evm::set_inspector_enabled` is a public trait method, so an EVM built with no inspector at all
/// can have its shim switched on without any constructor being reached. Everything that runs then
/// is `NoOpInspector`, which this crate declares — so the block path must admit the transaction.
/// Building the shim undeclared would refuse an EVM for observing nothing.
#[test]
fn test_an_evm_with_no_inspector_is_admitted_after_its_shim_is_switched_on() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
    );

    alloy_evm::Evm::enable_inspector(executor.evm_mut());
    assert!(
        !executor.evm().has_undeclared_inspector(),
        "the inspector an uninspected EVM carries is the declared one",
    );

    let tx = envelope(0);
    let outcome = executor
        .run_transaction(Recovered::new_unchecked(&tx, CALLER))
        .expect("an EVM observing nothing must not be refused");
    assert!(outcome.result.is_success(), "fixture check: {:?}", outcome.result);
    assert!(!outcome.inner.undeclared_inspector, "and must report itself declared");
    executor.commit_transaction_outcome(outcome).expect("nor at commit");
    let (_, result) = executor.finish().expect("the block must finish");
    assert_eq!(result.receipts.len(), 1);
}

/// Swapping an inspector in through `InspectEvm::set_inspector` drops the declaration, whatever
/// the type being swapped in.
///
/// The constructor whose bound is `TrustedObserver` is the only route to the declared shim, and
/// `set_inspector`'s bound is plain `Inspector` — so it builds a measured one even for a type that
/// carries a declaration. That is the safe direction and it is pinned here, because it is what
/// keeps the default shim's declaration from spreading to whatever replaces it.
#[test]
fn test_swapping_an_inspector_in_drops_the_declaration() {
    let mut db = build_db();
    let mut evm = mega_evm::MegaEvm::new(
        mega_evm::MegaContext::new(&mut db, MegaSpecId::REX7)
            .with_tx_runtime_limits(mega_evm::EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)),
    );
    assert!(evm.has_trusted_inspector(), "the default shim carries `NoOpInspector`'s declaration");

    revm::InspectEvm::set_inspector(&mut evm, NoOpInspector);
    assert!(
        !evm.has_trusted_inspector(),
        "a swapped-in inspector is measured: the declaration belongs to the constructor, not the \
         type being handed over",
    );
}

/// The deprecated `inspect_transaction` runs the inspecting loop whatever the runtime flag says,
/// so what it reports about the inspector cannot be read off that flag.
///
/// `Evm::set_inspector_enabled(false)` turns off the flag `execute_transaction` picks its loop on
/// and leaves the inspector where it is. Driven through this entry the inspector still runs, so an
/// outcome saying no inspector took part would let the commit funnel admit a transaction one did.
#[test]
fn test_inspect_transaction_reports_an_inspector_the_runtime_flag_hides() {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory(MegaSpecId::REX7).create_executor(
        &mut state,
        block_ctx(),
        evm_env(MegaSpecId::REX7),
    );

    let mut inspected_db = build_db();
    let mut evm = mega_evm::MegaEvm::new(
        mega_evm::MegaContext::new(&mut inspected_db, MegaSpecId::REX7)
            .with_tx_runtime_limits(mega_evm::EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)),
    )
    .with_inspector(GasInjector::default());
    alloy_evm::Evm::set_inspector_enabled(&mut evm, false);
    assert!(
        !evm.has_undeclared_inspector(),
        "fixture check: with the flag off, the entry that honours it reports no inspector",
    );

    let mut tx_env = mega_evm::MegaTransaction::new(
        revm::context::tx::TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(1_000_000)
            .build_fill(),
    );
    tx_env.enveloped_tx = Some(Bytes::new());
    #[expect(deprecated, reason = "the entry under test is the deprecated one")]
    let outcome = evm.inspect_transaction(tx_env).expect("the EVM supports the rewrite in full");

    assert!(evm.inspector.applied, "fixture check: the inspector really did run");
    assert!(
        outcome.undeclared_inspector,
        "an entry that always inspects must report the inspector it always runs",
    );

    let tx = envelope(0);
    let err = executor
        .commit_tx_result(mega_evm::MegaBlockTxResult {
            tx_type: tx.tx_type(),
            tx_hash: *tx.hash(),
            gas_limit: 1_000_000,
            tx_size: 0,
            da_size: 0,
            depositor: None,
            inner: outcome,
        })
        .expect_err("and the commit funnel must refuse it");
    expect_undeclared(&err, *tx.hash());
    assert!(executor.receipts.is_empty(), "no receipt may have been pushed");
}
