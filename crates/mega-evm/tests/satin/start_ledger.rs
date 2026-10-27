//! The start ledger over the engine's paths no other test drives through a transaction's start
//! lifecycle.
//!
//! The engine keeps what each frame start owes and holds — its budgets, its start charge, its
//! history allowance — on its own start ledger, driven by revm's start lifecycle. These tests drive
//! the lifecycle's paths the rest of the suite does not: the depth guard's answer inside a running
//! transaction, a fatal error with starts open and the reuse of the EVM after it, an EVM run inside
//! an inspector's callback, an inspector's answer to the transaction's own frame, the end-callback
//! rewrites the ledger settles by the new status and revm's flags, and a creation that takes no
//! address. Each reads the starts revm settled off its debug mirror of the lifecycle, so the path
//! is shown taken, and what the ledger settled for them (`take_start_ledger_settlements`).

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{
        neutral_cfg, neutralize_evm, record_start_ledger_settlements,
        take_start_ledger_settlements, BytecodeBuilder, ErrorInjectingDatabase, LedgerSettlement,
        MemoryDatabase,
    },
    EthSpecId, EvmTxRuntimeLimits, MegaContext, MegaEvm, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{ADDRESS, CALL, GAS, POP, PUSH0, REVERT, STOP},
    context::{BlockEnv, ContextTr},
    context_interface::{
        host::{FrameSettle, LifecycleEnd, SettleStatus},
        LifecycleEvent, LocalContextTr,
    },
    handler::FrameResult,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        FrameInput, Gas, InstructionResult, InterpreterResult, PoolAmounts,
    },
    primitives::CALL_STACK_LIMIT,
    Database, Inspector,
};

use crate::common::{block, call, context, create, history};

const CALLER: Address = address!("0000000000000000000000000000000000d00000");
const OUTER: Address = address!("0000000000000000000000000000000000d00001");
const CHILD: Address = address!("0000000000000000000000000000000000d00002");
/// Calls a contract whose storage read the database fails.
const CALLS_FAILS: Address = address!("0000000000000000000000000000000000d00005");
const GAS_LIMIT: u64 = 10_000_000;

/// The settlements of the last transaction's start lifecycle, as revm's debug mirror recorded
/// them, and how it ended: each settlement is one the start ledger compared with the fork's
/// figures.
fn settlements<DB: Database>(ctx: &mut MegaContext<DB>) -> (Vec<FrameSettle>, LifecycleEnd) {
    let mirror = ctx.local_mut().lifecycle_mirror().expect("a debug build's mirror");
    let mut settled = Vec::new();
    let mut end = None;
    for event in mirror.events() {
        match event {
            LifecycleEvent::Settle(settle) => settled.push(*settle),
            LifecycleEvent::End(ended) => end = Some(*ended),
            LifecycleEvent::Split(_) => {}
        }
    }
    (settled, end.expect("the lifecycle ended"))
}

/// `(start, depth, status)` of each settlement, innermost first.
fn shape(settled: &[FrameSettle]) -> Vec<(u32, usize, SettleStatus)> {
    settled.iter().map(|settle| (settle.id, settle.depth, settle.status)).collect()
}

/// What the start ledger settled for `start`.
fn ledger_settled(settlements: &[LedgerSettlement], start: u32) -> LedgerSettlement {
    *settlements
        .iter()
        .find(|settled| settled.start == start)
        .unwrap_or_else(|| panic!("start {start} settled on the ledger: {settlements:?}"))
}

/// `OUTER` sends one wei to `CHILD`, which writes a fresh slot and then returns or reverts as
/// `reverts` says; `OUTER` stops either way. The child's start makes two write records, which its
/// caller is charged for at the `CALL` and which its failure gives back.
fn value_call_to_a_writer(reverts: bool) -> MemoryDatabase {
    let outer = BytecodeBuilder::default().call(CHILD, U256::from(1)).append(POP).stop().build();
    let child = BytecodeBuilder::default().sstore(U256::from(7), U256::from(1));
    let child = if reverts { child.append_many([PUSH0, PUSH0, REVERT]) } else { child.stop() };
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(OUTER, U256::from(10))
        .account_code(OUTER, outer)
        .account_code(CHILD, child.build())
}

/// A frame past the call-stack limit is answered by the depth guard inside a running transaction:
/// a contract that calls itself with all its gas reaches depth 1,024, whose call is answered
/// `CallTooDeep` at depth 1,025 without a frame, and the guard's answer settles on the start revm
/// opened for it as every frame below it settles. No Satin transaction reaches that depth, a
/// child taking 98/100 of its caller's gas under an execution cap, so this one runs on the neutral
/// configuration, EIP-150's 63/64, without a cap.
#[test]
fn test_the_depth_guards_answer_settles_on_its_start() {
    const RECURSES: Address = address!("0000000000000000000000000000000000d00003");
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0, ADDRESS, GAS, CALL])
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(RECURSES, code);
    let mut cfg = neutral_cfg(EthSpecId::OSAKA).expect("Osaka has a neutral configuration");
    cfg.tx_gas_limit_cap = Some(u64::MAX);
    let ctx = context(db)
        .with_block(BlockEnv { gas_limit: u64::MAX, ..block() })
        .with_neutral_cfg(cfg)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    let mut evm = MegaEvm::new(ctx);
    neutralize_evm(&mut evm, EthSpecId::OSAKA).expect("Osaka neutralizes");
    let outcome = evm
        .execute_transaction(call(CALLER, RECURSES, U256::ZERO, 1_000_000_000_000))
        .expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);

    let (settled, end) = settlements(evm.ctx_mut());
    assert_eq!(end, LifecycleEnd::Settled);
    let past = CALL_STACK_LIMIT as usize + 1;
    assert_eq!(settled.len(), past + 1, "a start for every frame and the guard's answer");
    // revm counts `CallTooDeep` among its revert results.
    let guards = [(past as u32 + 1, past, SettleStatus::Revert)];
    assert_eq!(shape(&settled[..1]), guards, "the guard's answer");
    assert!(settled[1..].iter().all(|settle| settle.status == SettleStatus::Ok), "the frames'");
}

/// A database error in a frame two starts deep fails the transaction with both starts open: the
/// lifecycle is discarded and the start ledger dropped with it, and the same EVM runs the next
/// transaction from a ledger its own transaction's start resets, every start compared.
#[test]
fn test_a_fatal_error_with_starts_open_discards_the_ledger_and_the_evm_runs_on() {
    const FAILS: Address = address!("0000000000000000000000000000000000d00004");
    let key = U256::from(0x42);
    let outer = BytecodeBuilder::default().call(FAILS, U256::ZERO).append(POP).stop().build();
    let fails = BytecodeBuilder::default().sstore(key, U256::from(1)).stop().build();
    let db =
        value_call_to_a_writer(false).account_code(FAILS, fails).account_code(CALLS_FAILS, outer);
    let mut db = ErrorInjectingDatabase::new(db);
    db.fail_on_storage = Some((FAILS, key));
    let mut evm = MegaEvm::new(context(db));
    record_start_ledger_settlements(evm.ctx_mut());

    let failing = call(CALLER, CALLS_FAILS, U256::ZERO, GAS_LIMIT);
    let error = evm.execute_transaction(failing).expect_err("the storage read fails");
    assert!(format!("{error:?}").contains("injected storage()"), "{error:?}");
    let (settled, end) = settlements(evm.ctx_mut());
    assert_eq!((settled.len(), end), (0, LifecycleEnd::Discarded), "both starts discarded");
    assert_eq!(take_start_ledger_settlements(evm.ctx_mut()), [], "the ledger settled nothing");

    let outcome = evm
        .execute_transaction(call(CALLER, OUTER, U256::ZERO, GAS_LIMIT))
        .expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let (settled, end) = settlements(evm.ctx_mut());
    assert_eq!(end, LifecycleEnd::Settled);
    assert_eq!(shape(&settled), [(2, 1, SettleStatus::Ok), (1, 0, SettleStatus::Ok)]);
    // The ledger started over at the transaction's start: its own two starts, the value call's
    // charge kept by the call's success.
    let ledger = take_start_ledger_settlements(evm.ctx_mut());
    let starts: Vec<_> = ledger.iter().map(|settled| (settled.start, settled.depth)).collect();
    assert_eq!(starts, [(2, 1), (1, 0)]);
    assert!(ledger.iter().all(|settled| settled.refund == 0), "{ledger:?}");
}

/// Runs a transaction on an EVM of its own, over a copy of the state, in its `call` callback for
/// `CHILD`: a simulation a tool makes while the outer transaction runs.
struct Simulates {
    db: MemoryDatabase,
    simulated: usize,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Simulates {
    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address == CHILD {
            let mut nested = MegaEvm::new(context(self.db.clone()));
            record_start_ledger_settlements(nested.ctx_mut());
            let outcome = nested
                .execute_transaction(call(CALLER, OUTER, U256::from(1), GAS_LIMIT))
                .expect("the simulation is valid");
            assert!(outcome.result.is_success(), "{:?}", outcome.result);
            let (settled, end) = settlements(nested.ctx_mut());
            assert_eq!(end, LifecycleEnd::Settled);
            assert_eq!(shape(&settled), [(2, 1, SettleStatus::Ok), (1, 0, SettleStatus::Ok)]);
            let ledger = take_start_ledger_settlements(nested.ctx_mut());
            let starts: Vec<_> = ledger.iter().map(|settled| settled.start).collect();
            assert_eq!(starts, [2, 1], "the simulation's own starts, on its own ledger");
            self.simulated += 1;
        }
        None
    }
}

/// An EVM a tool runs inside an inspector's callback keeps a start ledger of its own, which its
/// transaction's start resets and every start of which it compares; the outer transaction's ledger,
/// with a start open in it, goes on as if nothing ran, and compares every start of its own.
/// `MegaContext` is not `Clone`, so a simulation always runs on a context of its own.
#[test]
fn test_an_evm_run_inside_a_callback_keeps_a_ledger_of_its_own() {
    let db = value_call_to_a_writer(false);
    let inspector = Simulates { db: db.clone(), simulated: 0 };
    let mut evm = MegaEvm::new(context(db)).with_inspector(inspector);
    record_start_ledger_settlements(evm.ctx_mut());
    let outcome = evm
        .execute_transaction(call(CALLER, OUTER, U256::ZERO, GAS_LIMIT))
        .expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(evm.inspector().simulated, 1);
    let (settled, end) = settlements(evm.ctx_mut());
    assert_eq!(end, LifecycleEnd::Settled);
    assert_eq!(shape(&settled), [(2, 1, SettleStatus::Ok), (1, 0, SettleStatus::Ok)]);
    let ledger = take_start_ledger_settlements(evm.ctx_mut());
    let starts: Vec<_> = ledger.iter().map(|settled| settled.start).collect();
    assert_eq!(starts, [2, 1], "the outer transaction's own starts, none of the simulation's");
}

/// Answers the transaction's own frame on `Gas::new`, as Foundry answers a cheatcode: in
/// `frame_start` when `at_frame_start`, otherwise in `call` or `create`.
struct AnswersTheTransaction {
    result: InstructionResult,
    at_frame_start: bool,
}

impl AnswersTheTransaction {
    fn answer_call(&self, inputs: &CallInputs) -> CallOutcome {
        let gas = Gas::new(inputs.gas_limit);
        let result = InterpreterResult::new(self.result, Bytes::new(), gas);
        CallOutcome::new(result, inputs.return_memory_offset.clone())
    }

    fn answer_create(&self, inputs: &CreateInputs) -> CreateOutcome {
        let gas = Gas::new(inputs.gas_limit());
        CreateOutcome::new(InterpreterResult::new(self.result, Bytes::new(), gas), None)
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for AnswersTheTransaction {
    fn frame_start(
        &mut self,
        _: &mut MegaContext<DB>,
        input: &mut FrameInput,
    ) -> Option<FrameResult> {
        if !self.at_frame_start {
            return None;
        }
        match input {
            FrameInput::Call(inputs) => Some(FrameResult::Call(self.answer_call(inputs))),
            FrameInput::Create(inputs) => Some(FrameResult::Create(self.answer_create(inputs))),
            FrameInput::Empty => None,
        }
    }

    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (!self.at_frame_start).then(|| self.answer_call(inputs))
    }

    fn create(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        (!self.at_frame_start).then(|| self.answer_create(inputs))
    }
}

/// An inspector's answer to the transaction's own frame, on `Gas::new`, settles as the start
/// ledger settles an inspector's answer — unlimited budgets, which the transaction is left with,
/// and no start charge back: a value transfer and a creation, answered with a success, a revert
/// and a halt, in `frame_start` and in `call` or `create`.
#[test]
fn test_an_inspectors_answer_to_the_transactions_frame_settles_as_unlimited_pools() {
    let db = || MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let results = [InstructionResult::Stop, InstructionResult::Revert, InstructionResult::OutOfGas];
    for at_frame_start in [false, true] {
        for result in results {
            for transaction in [
                call(CALLER, CHILD, U256::from(1), GAS_LIMIT),
                create(CALLER, Bytes::from_static(&[0x00]), GAS_LIMIT),
            ] {
                let case = format!(
                    "{result:?}, frame start {at_frame_start}, {:?}",
                    transaction.0.base.kind
                );
                let inspector = AnswersTheTransaction { result, at_frame_start };
                let mut evm = MegaEvm::new(context(db())).with_inspector(inspector);
                record_start_ledger_settlements(evm.ctx_mut());
                let outcome = evm.execute_transaction(transaction).expect("valid");
                assert_eq!(outcome.result.is_success(), result.is_ok(), "{case}");
                let (settled, end) = settlements(evm.ctx_mut());
                assert_eq!(end, LifecycleEnd::Settled, "{case}");
                assert_eq!(settled.len(), 1, "{case}");
                assert!(settled[0].inspector_answer, "{case}: flagged");
                let ledger = ledger_settled(&take_start_ledger_settlements(evm.ctx_mut()), 1);
                assert_eq!(ledger.refund, 0, "{case}: no start charge back");
                assert_eq!(ledger.frame.remaining, PoolAmounts::UNLIMITED, "{case}");
                assert_eq!(ledger.caller.remaining, PoolAmounts::UNLIMITED, "{case}");
            }
        }
    }
}

/// How [`Rewrites`] rewrites `CHILD`'s result in `call_end`.
#[derive(Clone, Copy, Debug)]
enum Rewrite {
    /// The status alone, a success into a revert and a failure into a success.
    Status,
    /// The tracker, replaced with `Gas::new` of the call's gas limit.
    Tracker,
    /// The outcome, rebuilt around the same result with `CallOutcome::new`.
    Outcome,
    /// The outcome, rebuilt keeping every other field of it.
    KeepingTheOutcome,
}

/// Rewrites `CHILD`'s result in `call_end` as its [`Rewrite`] says.
struct Rewrites(Rewrite);

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Rewrites {
    fn call_end(&mut self, _: &mut MegaContext<DB>, inputs: &CallInputs, out: &mut CallOutcome) {
        if inputs.target_address != CHILD {
            return;
        }
        match self.0 {
            Rewrite::Status => {
                out.result.result = if out.result.result.is_ok() {
                    InstructionResult::Revert
                } else {
                    InstructionResult::Stop
                }
            }
            Rewrite::Tracker => out.result.gas = Gas::new(inputs.gas_limit),
            Rewrite::Outcome => {
                *out = CallOutcome::new(out.result.clone(), out.memory_offset.clone());
            }
            Rewrite::KeepingTheOutcome => {
                *out = CallOutcome { result: out.result.clone(), ..out.clone() };
            }
        }
    }
}

/// The end-callback rewrites settle on the start ledger by the new status and revm's flags: a
/// status rewritten either way settles by the new status, the start charge — the two write records
/// of the value call's start — back on a failure; a tracker replaced with `Gas::new` settles as
/// unlimited budgets, and its start charge comes back by the status; an outcome rebuilt around the
/// same result settles on the frame's budgets and gives no start charge back, as the constructor's
/// outcome carries none; one rebuilt keeping the outcome is no rewrite at all. Each on a value call
/// to a child that writes a fresh slot and then succeeds or reverts.
#[test]
fn test_end_callback_rewrites_settle_by_the_new_status_and_the_flags() {
    for rewrite in [Rewrite::Status, Rewrite::Tracker, Rewrite::Outcome, Rewrite::KeepingTheOutcome]
    {
        for reverts in [false, true] {
            let case = format!("{rewrite:?}, the child reverts {reverts}");
            let mut evm = MegaEvm::new(context(value_call_to_a_writer(reverts)))
                .with_inspector(Rewrites(rewrite));
            record_start_ledger_settlements(evm.ctx_mut());
            let outcome = evm
                .execute_transaction(call(CALLER, OUTER, U256::ZERO, GAS_LIMIT))
                .expect("the transaction is valid");
            assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
            let (settled, end) = settlements(evm.ctx_mut());
            assert_eq!(end, LifecycleEnd::Settled, "{case}");
            let child = settled[0];
            let ok = matches!(rewrite, Rewrite::Status) == reverts;
            assert_eq!(child.status == SettleStatus::Ok, ok, "{case}: the settled status");
            let flags = (child.inspector_answer, child.gas_replaced, child.outcome_rebuilt);
            let expected = match rewrite {
                Rewrite::Status | Rewrite::KeepingTheOutcome => (false, false, false),
                Rewrite::Tracker => (false, true, false),
                Rewrite::Outcome => (false, false, true),
            };
            assert_eq!(flags, expected, "{case}: the flags");
            let ledger = ledger_settled(&take_start_ledger_settlements(evm.ctx_mut()), 2);
            let charge = history(2 * WRITE_RECORD_SIZE);
            let refunded = !ok && !matches!(rewrite, Rewrite::Outcome);
            assert_eq!(ledger.refund, if refunded { charge } else { 0 }, "{case}: the refund");
            let unlimited = ledger.frame.remaining == PoolAmounts::UNLIMITED;
            assert_eq!(unlimited, matches!(rewrite, Rewrite::Tracker), "{case}: the budgets");
        }
    }
}

/// `OUTER` creates a contract whose init code stops, deploying no code, and stops. The creation's
/// start makes one write record on the creation, the created account's, which its caller is
/// charged for at the `CREATE` and which the creation's start charge gives back when it fails or
/// takes no address; the creator's nonce record is charged there too and is not given back.
fn creates_an_empty_contract() -> MemoryDatabase {
    let outer = BytecodeBuilder::default().create(U256::ZERO, [STOP]).append(POP).stop().build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(OUTER, outer)
}

/// How [`CreationRewrites`] takes the creation `OUTER` makes its address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressRewrite {
    /// `create_end` clears the address of the creation, which succeeded.
    Cleared,
    /// `create` answers the creation with a success that took no address, on `Gas::new`.
    AnsweredWithout,
}

/// Takes the creation `OUTER` makes its address as its [`AddressRewrite`] says.
struct CreationRewrites(AddressRewrite);

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for CreationRewrites {
    fn create(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        (self.0 == AddressRewrite::AnsweredWithout && inputs.caller() == OUTER).then(|| {
            let gas = Gas::new(inputs.gas_limit());
            CreateOutcome::new(
                InterpreterResult::new(InstructionResult::Stop, Bytes::new(), gas),
                None,
            )
        })
    }

    fn create_end(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &CreateInputs,
        out: &mut CreateOutcome,
    ) {
        if self.0 == AddressRewrite::Cleared && inputs.caller() == OUTER {
            assert!(out.result.is_ok() && out.address.is_some(), "the creation took its address");
            out.address = None;
        }
    }
}

/// A creation that took no address settles as one, whoever took it: an end callback that clears a
/// successful creation's address has its start charge given back, the created account's record,
/// whose history the transaction no longer pays, as revm reports the creation took no address; an
/// inspector's success answered without an address gives nothing back, an inspector's answer
/// carrying no start charge, and the transaction pays what the creation's opcode was charged, as
/// it pays when the creation runs and keeps it.
#[test]
fn test_a_creation_that_takes_no_address_settles_as_one() {
    let run = |inspector: Option<CreationRewrites>| {
        let tx = call(CALLER, OUTER, U256::ZERO, GAS_LIMIT);
        let outcome = match inspector {
            Some(inspector) => MegaEvm::new(context(creates_an_empty_contract()))
                .with_inspector(inspector)
                .execute_transaction(tx),
            None => MegaEvm::new(context(creates_an_empty_contract())).execute_transaction(tx),
        };
        let outcome = outcome.expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        outcome
    };
    let kept = run(None);
    for rewrite in [AddressRewrite::Cleared, AddressRewrite::AnsweredWithout] {
        let mut evm = MegaEvm::new(context(creates_an_empty_contract()))
            .with_inspector(CreationRewrites(rewrite));
        record_start_ledger_settlements(evm.ctx_mut());
        let outcome = evm
            .execute_transaction(call(CALLER, OUTER, U256::ZERO, GAS_LIMIT))
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{rewrite:?}: {:?}", outcome.result);
        let (settled, end) = settlements(evm.ctx_mut());
        assert_eq!(end, LifecycleEnd::Settled, "{rewrite:?}");
        let creation = settled[0];
        assert_eq!((creation.id, creation.depth), (2, 1), "{rewrite:?}: the creation's start");
        assert_eq!(
            (creation.status, creation.created, creation.inspector_answer),
            (SettleStatus::Ok, Some(false), rewrite == AddressRewrite::AnsweredWithout),
            "{rewrite:?}: a success that took no address"
        );
        let given_back = match rewrite {
            AddressRewrite::Cleared => history(WRITE_RECORD_SIZE),
            AddressRewrite::AnsweredWithout => 0,
        };
        assert_eq!(outcome.gas.history, kept.gas.history - given_back, "{rewrite:?}");
        let ledger = ledger_settled(&take_start_ledger_settlements(evm.ctx_mut()), 2);
        assert_eq!(ledger.refund, given_back, "{rewrite:?}: the ledger's refund");
    }
}
