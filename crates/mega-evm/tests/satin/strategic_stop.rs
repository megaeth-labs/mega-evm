//! The abort protocol: a transaction-level limit stops the transaction with a revert.
//!
//! The frame that crosses the limit reverts with `MegaLimitExceeded`, the transaction is latched,
//! no caller resumes, and the transaction settles like an EIP-8037 revert: the sender gets the
//! unspent regular gas and the whole reservoir back. A frame budget reverts only its frame. A real
//! out-of-gas still halts and burns. The limit here is the configurable data-size cap of
//! `EvmTxRuntimeLimits`; a write record is 40 bytes.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaContext, MegaEvm, MegaHaltReason,
    MegaLimitExceeded, MegaTransaction,
};
use revm::{
    bytecode::opcode::{CALL, GAS, LOG0, POP, PUSH0, SSTORE},
    context::result::{ExecutionResult, ResultAndState},
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        InstructionResult, Interpreter,
    },
    Database, Inspector,
};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const A: Address = address!("00000000000000000000000000000000000000A0");
const B: Address = address!("00000000000000000000000000000000000000B0");
const C: Address = address!("00000000000000000000000000000000000000C0");
const D: Address = address!("00000000000000000000000000000000000000D0");
const GAS_LIMIT: u64 = 1_000_000;

/// Writes slot 1, calls `next` with all its gas, then writes slot 2 and emits a log: the part
/// after the call is what a resumed caller would run.
fn relay(next: Address) -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(next)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .sstore(U256::from(2), U256::from(1))
        .append_many([PUSH0, PUSH0])
        .append(LOG0)
        .stop()
        .build()
}

/// Writes slots 1 to 3.
fn writer() -> Bytes {
    (1..=3u64)
        .fold(BytecodeBuilder::default(), |code, slot| code.sstore(U256::from(slot), U256::from(1)))
        .stop()
        .build()
}

/// `A -> B -> C -> D`, each writing a slot before its call; `D`, at depth 3, writes three.
fn chain() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_code(A, relay(B))
        .account_code(B, relay(C))
        .account_code(C, relay(D))
        .account_code(D, writer())
}

fn evm_with<INSP>(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    inspector: INSP,
) -> MegaEvm<MemoryDatabase, INSP> {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits)).with_inspector(inspector)
}

fn run(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: MegaTransaction,
) -> (ResultAndState<MegaHaltReason>, Option<LimitCheck>) {
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
    let result = evm.transact_raw(tx).unwrap();
    (result, evm.ctx().additional_limit().latched().copied())
}

fn cap(bytes: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(bytes)
}

fn limit_exceeded(kind: LimitKind, limit: u64) -> Bytes {
    MegaLimitExceeded { kind: kind.as_u8(), limit }.abi_encode().into()
}

fn assert_stopped(result: &ExecutionResult<MegaHaltReason>, kind: LimitKind, limit: u64) {
    match result {
        ExecutionResult::Revert { output, .. } => {
            assert_eq!(output, &limit_exceeded(kind, limit), "the stop's revert data")
        }
        other => panic!("expected a revert-class stop, got {other:?}"),
    }
}

/// Records every step, and every call result the inspector sees end.
#[derive(Default)]
struct Probe {
    steps: Vec<u8>,
    call_results: Vec<(Address, InstructionResult, Bytes)>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Probe {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        self.steps.push(interp.bytecode.opcode());
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.call_results.push((
            inputs.target_address,
            outcome.result.result,
            outcome.result.output.clone(),
        ));
    }
}

/// Crossing the cap at depth 3 reverts the whole transaction with `MegaLimitExceeded`: nothing
/// it wrote or logged stays, no caller runs another instruction, and the transaction is latched.
#[test]
fn test_cap_crossed_at_depth_three_stops_the_transaction() {
    // A, B, C and D's first write are four records (160 bytes); D's second crosses 180.
    let limit = 180;
    for inspect in [false, true] {
        let mut evm = evm_with(chain(), cap(limit), Probe::default());
        alloy_evm::Evm::set_inspector_enabled(&mut evm, inspect);
        let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();

        assert_stopped(&result.result, LimitKind::DataSize, limit);
        assert!(result.result.logs().is_empty());
        for account in [A, B, C, D] {
            let storage = &result.state[&account].storage;
            assert!(storage.values().all(|slot| !slot.is_changed()), "{account} kept a write");
        }
        let latched = evm.ctx().additional_limit().latched().copied();
        assert_eq!(
            latched,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit,
                used: 200,
                frame_local: false
            })
        );
        if inspect {
            let probe = evm.inspector();
            let sstores = probe.steps.iter().filter(|op| matches!(op, &&SSTORE)).count();
            assert_eq!(sstores, 5, "A, B, C and D's first two writes ran");
            assert_eq!(probe.steps.last(), Some(&SSTORE), "nothing ran after the crossing write");
            assert!(!probe.steps.contains(&LOG0), "no caller resumed");
            let stops = probe.call_results.iter().map(|(target, result, output)| {
                assert_eq!(output, &limit_exceeded(LimitKind::DataSize, limit));
                (*target, *result)
            });
            assert_eq!(
                stops.collect::<Vec<_>>(),
                vec![
                    (D, InstructionResult::Revert),
                    (C, InstructionResult::Revert),
                    (B, InstructionResult::Revert),
                    (A, InstructionResult::Revert)
                ],
                "every frame returned the stop, the deepest first"
            );
        }
    }
}

/// With and without an inspector the stopped transaction spends the same gas, and the gas does
/// not depend on what the callers would have run after the call: they never resume.
#[test]
fn test_stop_bills_only_what_ran() {
    let gas_of = |db: MemoryDatabase, inspect: bool| {
        let mut evm = evm_with(db, cap(180), Probe::default());
        alloy_evm::Evm::set_inspector_enabled(&mut evm, inspect);
        let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
        assert_stopped(&result.result, LimitKind::DataSize, 180);
        result.result.gas().tx_gas_used()
    };
    let plain = gas_of(chain(), false);
    assert_eq!(gas_of(chain(), true), plain, "an observing inspector changes nothing");
    assert!(plain < GAS_LIMIT / 2, "the stop does not burn the gas: {plain}");

    // Callers whose code after the call is far more expensive spend exactly as much.
    let heavy = |next: Address| {
        let mut code = BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(next)
            .append(GAS)
            .append(CALL)
            .append(POP);
        for slot in 10..30u64 {
            code = code.sstore(U256::from(slot), U256::from(1));
        }
        code.stop().build()
    };
    let db = MemoryDatabase::default()
        .account_code(A, heavy(B))
        .account_code(B, heavy(C))
        .account_code(C, heavy(D))
        .account_code(D, writer());
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(cap(80)));
    let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    assert_stopped(&result.result, LimitKind::DataSize, 80);
    let light = |next: Address| {
        BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(next)
            .append(GAS)
            .append(CALL)
            .append(POP)
            .stop()
            .build()
    };
    let db = MemoryDatabase::default()
        .account_code(A, light(B))
        .account_code(B, light(C))
        .account_code(C, light(D))
        .account_code(D, writer());
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(cap(80)));
    let light_result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
    assert_stopped(&light_result.result, LimitKind::DataSize, 80);
    assert_eq!(
        result.result.gas().tx_gas_used(),
        light_result.result.gas().tx_gas_used(),
        "the code after the calls never ran"
    );
}

/// The stopped transaction gives the whole reservoir back, and spends the same gas whatever its
/// gas limit, so a limit above the execution cap changes nothing but the refund.
#[test]
fn test_stop_refills_the_reservoir_at_any_gas_limit() {
    let at = |gas_limit: u64| {
        let (result, _) = run(chain(), cap(180), call(CALLER, A, U256::ZERO, gas_limit));
        assert_stopped(&result.result, LimitKind::DataSize, 180);
        *result.result.gas()
    };
    let small = at(100_000_000);
    let large = at(1_000_000_000);
    assert_eq!(small.reservoir_remaining(), 0, "a limit under the cap has no reservoir");
    assert_eq!(large.reservoir_remaining(), 1_000_000_000 - TX_GAS_LIMIT_CAP, "all of it back");
    assert_eq!(small.tx_gas_used(), large.tx_gas_used());
    assert_eq!(small.total_gas_spent(), large.total_gas_spent());
}

/// An `SSTORE` that runs out of gas after its Host call records nothing, so a cap of zero does
/// not stop the transaction: it halts out of gas and burns its gas.
#[test]
fn test_true_out_of_gas_still_halts_and_burns() {
    let db = MemoryDatabase::default().account_code(A, writer());
    let gas_limit = 21_000 + 5_000;
    let (result, latched) = run(db, cap(0), call(CALLER, A, U256::ZERO, gas_limit));
    assert!(
        matches!(
            result.result,
            ExecutionResult::Halt {
                reason: MegaHaltReason::Base(revm::context::result::HaltReason::OutOfGas(_)),
                ..
            }
        ),
        "{:?}",
        result.result
    );
    assert_eq!(result.result.gas().tx_gas_used(), gas_limit, "the halt burns the gas");
    assert_eq!(latched, None);
}

/// A child that runs out of gas halts alone; its caller goes on and writes within the cap.
#[test]
fn test_child_out_of_gas_under_a_cap_does_not_latch() {
    let parent = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .push_number(5_000_u64)
        .append(CALL)
        .append(POP)
        .sstore(U256::from(9), U256::from(1))
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(A, parent).account_code(B, writer());
    let (result, latched) = run(db, cap(40), call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.state[&A].storage[&U256::from(9)].present_value(), U256::from(1));
    assert_eq!(latched, None);
}

/// A cap crossed by the writes the first frame's start makes (a value transfer's recipient)
/// reverts the transaction before the frame runs: the recipient's code never runs, its balance
/// is untouched, and the transaction spends what an empty call does.
#[test]
fn test_cap_crossed_before_the_first_frame_reverts_without_running() {
    let funded = || MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));
    let mut evm = evm_with(funded().account_code(B, writer()), cap(39), Probe::default());
    let result = evm.transact_raw(call(CALLER, B, U256::from(5), GAS_LIMIT)).unwrap();
    assert_stopped(&result.result, LimitKind::DataSize, 39);
    assert!(evm.inspector().steps.is_empty(), "the recipient's code never ran");
    assert_eq!(result.state.get(&B).map(|b| b.info.balance).unwrap_or_default(), U256::ZERO);

    assert_eq!(evm.ctx().additional_limit().usage(), mega_evm::LimitUsage::ZERO, "nothing kept");

    let (empty_call, _) = run(funded(), cap(u64::MAX), call(CALLER, B, U256::from(5), GAS_LIMIT));
    assert!(empty_call.result.is_success());
    assert_eq!(result.result.gas().tx_gas_used(), empty_call.result.gas().tx_gas_used());
}

/// A frame budget reverts the frame that crosses it and nothing else: the caller resumes and the
/// transaction is not latched.
#[test]
fn test_frame_budget_reverts_the_frame_without_a_latch() {
    let parent = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .sstore(U256::from(9), U256::from(1))
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(A, parent).account_code(B, writer());
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(100);
    let mut evm = evm_with(db, limits, Probe::default());
    let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();

    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(
        result.state[&A].storage[&U256::from(9)].present_value(),
        U256::from(1),
        "A resumed"
    );
    assert!(result.state[&B].storage.values().all(|slot| !slot.is_changed()), "B reverted");
    assert_eq!(evm.ctx().additional_limit().latched(), None);
    let (target, outcome, output) = evm.inspector().call_results[0].clone();
    assert_eq!((target, outcome), (B, InstructionResult::Revert));
    assert_eq!(output, limit_exceeded(LimitKind::DataSize, 100));
    assert!(evm.inspector().steps.contains(&SSTORE));
}

/// A frame budget crossed by the outermost frame reverts the transaction, still without a latch.
#[test]
fn test_frame_budget_of_the_outermost_frame_reverts_the_transaction() {
    let db = MemoryDatabase::default().account_code(A, writer());
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(100);
    let (result, latched) = run(db, limits, call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert_stopped(&result.result, LimitKind::DataSize, 100);
    assert_eq!(latched, None);
}

/// A later latch does not replace the first.
#[test]
fn test_first_latch_wins() {
    let mut layer = mega_evm::AdditionalLimit::default();
    let first = layer.latch(LimitKind::KVUpdate, 1, 2);
    let second = layer.latch(LimitKind::DataSize, 3, 4);
    assert_eq!(first, second);
    assert_eq!(layer.latched(), Some(&first));
    assert!(first.exceeded_limit() && !first.is_frame_local());
}

/// The layer's state belongs to one transaction: a latch and the usage counted do not reach the
/// next transaction or system call, through any entry point.
#[test]
fn test_the_latch_does_not_outlive_its_transaction() {
    let db = chain().account_code(B, writer());
    let mut evm =
        MegaEvm::new(context(db).with_tx_runtime_limits(cap(100))).with_inspector(Probe::default());
    let stopped =
        |evm: &MegaEvm<MemoryDatabase, Probe>| evm.ctx().additional_limit().latched().is_some();

    // B writes three slots: 120 bytes cross the cap.
    alloy_evm::Evm::set_inspector_enabled(&mut evm, false);
    assert!(!evm.transact_raw(call(CALLER, B, U256::ZERO, GAS_LIMIT)).unwrap().result.is_success());
    assert!(stopped(&evm));
    // An empty call runs clean, through the plain and the inspected path.
    assert!(evm
        .transact_raw(call(CALLER, CALLER, U256::ZERO, GAS_LIMIT))
        .unwrap()
        .result
        .is_success());
    assert!(!stopped(&evm));
    assert_eq!(evm.ctx().additional_limit().usage(), mega_evm::LimitUsage::ZERO);

    assert!(!evm.transact_raw(call(CALLER, B, U256::ZERO, GAS_LIMIT)).unwrap().result.is_success());
    alloy_evm::Evm::set_inspector_enabled(&mut evm, true);
    assert!(evm
        .transact_raw(call(CALLER, CALLER, U256::ZERO, GAS_LIMIT))
        .unwrap()
        .result
        .is_success());
    assert!(!stopped(&evm));

    // So does a system call.
    assert!(!evm.transact_raw(call(CALLER, B, U256::ZERO, GAS_LIMIT)).unwrap().result.is_success());
    let system = evm.transact_system_call(CALLER, CALLER, Bytes::new()).unwrap();
    assert!(system.result.is_success());
    assert!(!stopped(&evm));
}

/// A creation transaction stopped before its first frame still bumps the sender's nonce, like any
/// included creation transaction: it cannot be replayed.
#[test]
fn test_create_transaction_stopped_before_its_first_frame_bumps_the_nonce() {
    let tx = crate::common::create(CALLER, writer(), GAS_LIMIT);
    let (result, latched) = run(MemoryDatabase::default(), cap(39), tx);
    assert_stopped(&result.result, LimitKind::DataSize, 39);
    assert!(latched.is_some());
    assert_eq!(result.state[&CALLER].info.nonce, 1, "the sender's nonce is bumped");
    assert!(result.state.get(&CALLER.create(0)).is_none_or(|a| a.info.is_empty_code_hash()));
    assert!(result.result.gas().tx_gas_used() < GAS_LIMIT / 2, "the stop burns nothing");

    // The stopped creation's result carries the reservoir back.
    let tx = crate::common::create(CALLER, writer(), 1_000_000_000);
    let (result, _) = run(MemoryDatabase::default(), cap(39), tx);
    assert_eq!(result.result.gas().reservoir_remaining(), 1_000_000_000 - TX_GAS_LIMIT_CAP);
}

/// A limit is crossed by usage above it: usage equal to the limit does not stop a transaction or
/// a frame.
#[test]
fn test_usage_equal_to_the_limit_does_not_stop() {
    // A writes three slots: 120 bytes.
    let db = || MemoryDatabase::default().account_code(A, writer());
    let (result, latched) = run(db(), cap(120), call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(latched, None);
    let budget = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(120);
    let (result, _) = run(db(), budget, call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success(), "{:?}", result.result);
    let (result, _) = run(db(), cap(119), call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert_stopped(&result.result, LimitKind::DataSize, 119);
}
