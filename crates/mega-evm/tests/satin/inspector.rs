//! Inspectors on Satin: what a rewriting inspector gets, the one rewrite refused, and the
//! admission gate block execution keys on.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, GasInspector, MemoryDatabase},
    DeclaredObserver, MegaContext, MegaEvm, FORBIDDEN_CREATE_REVIVAL,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, GAS, INVALID, PUSH0, REVERT, SSTORE},
    context::result::EVMError,
    inspector::NoOpInspector,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Gas, InstructionResult, Interpreter, InterpreterResult,
    },
    Database, Inspector,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000400000");
const A: Address = address!("00000000000000000000000000000000000000A1");
const B: Address = address!("00000000000000000000000000000000000000B1");
const FAKE: Address = address!("00000000000000000000000000000000000FA4E0");
const GAS_LIMIT: u64 = 1_000_000;

/// Calls `B` with `value` and stores the call's success flag in slot 0.
fn call_b_and_store_flag(value: u64) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(value)
        .push_address(B)
        .append(GAS)
        .append(CALL)
        .append(PUSH0)
        .append(SSTORE)
        .stop()
        .build()
}

/// Creates a contract from `init_code` (at most 32 bytes) and stores the address in slot 0.
fn create_and_store_address(init_code: &[u8]) -> Bytes {
    let mut word = [0u8; 32];
    word[..init_code.len()].copy_from_slice(init_code);
    BytecodeBuilder::default()
        .mstore(0, word)
        .push_number(init_code.len() as u64)
        .append_many([PUSH0, PUSH0])
        .append(CREATE)
        .append(PUSH0)
        .append(SSTORE)
        .stop()
        .build()
}

/// Rewrites what execution produces, one way per field.
#[derive(Default)]
struct Rewriter {
    opcode: u8,
    /// Gas charged after every `SSTORE`.
    charge_after_sstore: u64,
    /// Gas handed back after every `SSTORE`.
    erase_after_sstore: u64,
    /// Turns every call result into a success.
    call_succeeds: bool,
    /// Answers every call to `B` itself with a success.
    answer_calls: bool,
    /// Replaces every created address.
    created_address: Option<Address>,
    /// Turns every creation result into a success.
    create_succeeds: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Rewriter {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        self.opcode = interp.bytecode.opcode();
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<DB>,
    ) {
        if self.opcode == SSTORE {
            assert!(interp.gas.record_regular_cost(self.charge_after_sstore));
            interp.gas.erase_cost(self.erase_after_sstore);
        }
    }

    fn call(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        (self.answer_calls && inputs.target_address == B).then(|| {
            CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            )
        })
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        _inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if self.call_succeeds {
            outcome.result.result = InstructionResult::Stop;
        }
    }

    fn create_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if let Some(address) = self.created_address {
            outcome.address = Some(address);
        }
        if self.create_succeeds {
            outcome.result.result = InstructionResult::Return;
            outcome.address = Some(FAKE);
        }
    }
}

fn run_with<INSP: Inspector<MegaContext<MemoryDatabase>, EthInterpreter>>(
    db: MemoryDatabase,
    inspector: INSP,
    value: u64,
) -> Result<
    revm::context::result::ResultAndState<mega_evm::MegaHaltReason>,
    EVMError<core::convert::Infallible, mega_evm::MegaTransactionError>,
> {
    let mut evm = MegaEvm::new(context(db)).with_inspector(inspector);
    evm.transact_raw(call(CALLER, A, U256::from(value), GAS_LIMIT))
}

fn writes_two_slots() -> MemoryDatabase {
    let code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(1))
        .stop()
        .build();
    MemoryDatabase::default().account_code(A, code)
}

/// Gas an inspector charges or hands back in `step_end` lands on the receipt, exactly.
#[test]
fn test_step_end_charge_and_refund_land_on_the_receipt() {
    let gas_used = |rewriter: Rewriter| {
        let result = run_with(writes_two_slots(), rewriter, 0).unwrap();
        assert!(result.result.is_success());
        result.result.gas().tx_gas_used()
    };
    let baseline = gas_used(Rewriter::default());
    assert_eq!(
        gas_used(Rewriter { charge_after_sstore: 1_000, ..Default::default() }),
        baseline + 2_000
    );
    assert_eq!(
        gas_used(Rewriter { erase_after_sstore: 500, ..Default::default() }),
        baseline - 1_000
    );
}

/// A call result rewritten in `call_end` is what the caller sees.
#[test]
fn test_call_end_rewrite_reaches_the_caller() {
    let db = || {
        MemoryDatabase::default()
            .account_code(A, call_b_and_store_flag(0))
            .account_code(B, Bytes::from_static(&[PUSH0, PUSH0, REVERT]))
    };
    let flag = |rewriter| {
        run_with(db(), rewriter, 0).unwrap().state[&A].storage[&U256::ZERO].present_value()
    };
    assert_eq!(flag(Rewriter::default()), U256::ZERO, "B reverts");
    assert_eq!(flag(Rewriter { call_succeeds: true, ..Default::default() }), U256::from(1));
}

/// A value call the inspector answers itself moves no value, and counts no write.
#[test]
fn test_short_circuited_value_call_moves_no_value() {
    let db = MemoryDatabase::default()
        .account_code(A, call_b_and_store_flag(5))
        .account_balance(A, U256::from(100));
    let mut evm = MegaEvm::new(context(db))
        .with_inspector(Rewriter { answer_calls: true, ..Default::default() });
    let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    assert!(result.result.is_success());
    assert_eq!(
        result.state[&A].storage[&U256::ZERO].present_value(),
        U256::from(1),
        "the answer is a success"
    );
    assert_eq!(result.state[&A].info.balance, U256::from(100), "no value left A");
    assert!(result.state.get(&B).is_none_or(|b| b.info.balance.is_zero()), "no value reached B");
    assert_eq!(evm.ctx().additional_limit().usage().write_records, 1, "only A's slot 0");
}

/// An address rewritten in `create_end` is what the caller sees, but the code is where the
/// creation put it.
#[test]
fn test_create_end_address_rewrite_reaches_the_caller_not_the_code() {
    // Init code returning one byte of runtime code (0x00).
    let init_code = [0x60, 0x01, 0x60, 0x00, 0xf3];
    let db = MemoryDatabase::default().account_code(A, create_and_store_address(&init_code));
    let result =
        run_with(db, Rewriter { created_address: Some(FAKE), ..Default::default() }, 0).unwrap();
    assert!(result.result.is_success(), "{:?}", result.result);
    let stored = result.state[&A].storage[&U256::ZERO].present_value();
    assert_eq!(Address::from_word(stored.into()), FAKE, "the caller got the rewritten address");
    let created = A.create(0);
    assert!(result.state[&created].info.code.as_ref().is_some_and(|code| !code.is_empty()));
    assert!(result.state.get(&FAKE).is_none_or(|fake| fake.info.is_empty_code_hash()));
}

/// A failed creation rewritten into a success is refused: the transaction fails with the
/// refusal.
#[test]
fn test_create_revival_is_refused() {
    let db = MemoryDatabase::default()
        .account_code(A, create_and_store_address(&[PUSH0, PUSH0, REVERT]));
    let result = run_with(db, Rewriter { create_succeeds: true, ..Default::default() }, 0);
    match result {
        Err(EVMError::Custom(message)) => {
            assert!(message.starts_with(FORBIDDEN_CREATE_REVIVAL), "{message}")
        }
        other => panic!("expected the refusal, got {other:?}"),
    }

    // A creation that succeeded may be rewritten freely; the refusal is for revivals only.
    let db = MemoryDatabase::default()
        .account_code(A, create_and_store_address(&[0x60, 0x01, 0x60, 0x00, 0xf3]));
    assert!(run_with(db, Rewriter { create_succeeds: true, ..Default::default() }, 0)
        .unwrap()
        .result
        .is_success());
}

/// A declared observer that writes back fails its declaration in a debug build.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a declared observer wrote to the interpreter in `step_end`")]
fn test_false_declaration_fails_in_debug() {
    let rewriter = Rewriter { charge_after_sstore: 1, ..Default::default() };
    let mut evm = MegaEvm::new(context(writes_two_slots()))
        .with_trusted_inspector(DeclaredObserver(rewriter));
    let _ = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT));
}

/// A declared call rewrite fails too.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a declared observer rewrote a call result")]
fn test_false_declaration_of_a_call_rewrite_fails_in_debug() {
    let db = MemoryDatabase::default()
        .account_code(A, call_b_and_store_flag(0))
        .account_code(B, Bytes::from_static(&[INVALID]));
    let rewriter = Rewriter { call_succeeds: true, ..Default::default() };
    let mut evm = MegaEvm::new(context(db)).with_trusted_inspector(DeclaredObserver(rewriter));
    let _ = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT));
}

/// A declared observer that only reads produces the transaction no inspector produces.
#[test]
fn test_declared_tracer_changes_nothing() {
    let db = || {
        MemoryDatabase::default()
            .account_code(A, call_b_and_store_flag(0))
            .account_code(B, Bytes::from_static(&[PUSH0, PUSH0, REVERT]))
    };
    let plain =
        MegaEvm::new(context(db())).transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    let tracer = TracingInspector::new(TracingInspectorConfig::default_parity());
    let mut evm = MegaEvm::new(context(db())).with_trusted_inspector(DeclaredObserver(tracer));
    let traced = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    assert_eq!(plain.result, traced.result);
    assert_eq!(plain.state, traced.state);
    assert_eq!(evm.inspector().0.traces().nodes().len(), 2, "the tracer saw both frames");
}

/// The admission gate: only an enabled inspector that did not arrive declared is a rewriting one.
#[test]
fn test_has_rewriting_inspector_truth_table() {
    let evm = || MegaEvm::new(context(MemoryDatabase::default()));
    assert!(!evm().has_rewriting_inspector(), "no inspector");
    assert!(evm().with_inspector(NoOpInspector).has_rewriting_inspector(), "a tool's inspector");
    assert!(
        evm().with_inspector(GasInspector::new()).has_rewriting_inspector(),
        "test utilities are tools"
    );
    assert!(!evm().with_trusted_inspector(NoOpInspector).has_rewriting_inspector());
    let tracer = TracingInspector::new(TracingInspectorConfig::default_parity());
    assert!(!evm().with_trusted_inspector(DeclaredObserver(tracer)).has_rewriting_inspector());

    let mut disabled = evm().with_inspector(Rewriter::default());
    disabled.set_inspector_enabled(false);
    assert!(!disabled.has_rewriting_inspector(), "a disabled inspector does not run");
    disabled.set_inspector_enabled(true);
    assert!(disabled.has_rewriting_inspector());
}

/// Counts frame starts and ends.
#[derive(Default)]
struct Pairs {
    calls: usize,
    call_ends: usize,
    creates: usize,
    create_ends: usize,
}

impl<CTX> Inspector<CTX, EthInterpreter> for Pairs {
    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.calls += 1;
        None
    }
    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends += 1;
    }
    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.creates += 1;
        None
    }
    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.create_ends += 1;
    }
}

/// Every frame the inspector sees start, it sees end: a frame that ran, one answered before it
/// ran (a value transfer the sender cannot fund, a latched transaction's first frame), and a
/// creation.
#[test]
fn test_frame_start_and_end_stay_paired() {
    let pairs = |db: MemoryDatabase, limits: mega_evm::EvmTxRuntimeLimits, value: u64| {
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .with_inspector(Pairs::default());
        let _ = evm.transact_raw(call(CALLER, A, U256::from(value), GAS_LIMIT)).unwrap();
        let pairs = evm.inspector();
        (pairs.calls, pairs.call_ends, pairs.creates, pairs.create_ends)
    };
    let no_limits = mega_evm::EvmTxRuntimeLimits::no_limits();
    let unfunded = MemoryDatabase::default().account_code(A, call_b_and_store_flag(5));
    assert_eq!(pairs(unfunded, no_limits, 0), (2, 2, 0, 0));
    let creates = MemoryDatabase::default().account_code(A, create_and_store_address(&[0x00]));
    assert_eq!(pairs(creates, no_limits, 0), (1, 1, 1, 1));
    let funded = MemoryDatabase::default().account_balance(CALLER, U256::from(10));
    let latched = no_limits.with_tx_data_size_limit(0);
    assert_eq!(pairs(funded, latched, 1), (1, 1, 0, 0), "the stopped first frame ends too");
}
