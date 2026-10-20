//! The stop: a limit stops the frame that crosses it with a revert.
//!
//! The frame that crosses the limit reverts alone with `MegaLimitExceeded`, whether the limit is
//! the transaction's or the frame's own budget, and its caller resumes with what the frame counted
//! taken back. A caller that carries on is held to what the transaction has left, and a write that
//! crosses again stops it in turn. A transaction whose own frame is stopped settles like an
//! EIP-8037 revert: the sender gets the unspent regular gas and the whole reservoir back. A real
//! out-of-gas still halts and burns. The limit here is the configurable data-size cap of
//! `EvmTxRuntimeLimits`; a write record is 40 bytes.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    constants::{ACCOUNT_STATE_GAS, TX_GAS_LIMIT_CAP},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaContext, MegaEvm, MegaHaltReason,
    MegaLimitExceeded, MegaTransaction, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CALL, GAS, LOG0, POP, PUSH0, SSTORE},
    context::result::ExecutionResult,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        InstructionResult, Interpreter,
    },
    Database, Inspector,
};

use crate::common::{
    call, context, execute_under, history, runs_at_measurement_prices, slot_state_gas,
};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const A: Address = address!("00000000000000000000000000000000000000A0");
const B: Address = address!("00000000000000000000000000000000000000B0");
const C: Address = address!("00000000000000000000000000000000000000C0");
const D: Address = address!("00000000000000000000000000000000000000D0");
/// Room for the state gas the chain's nine new slots draw, all of it out of regular gas: the
/// limit is below the execution cap, so the transaction has no reservoir.
const GAS_LIMIT: u64 = 10_000_000;

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

/// Runs `tx` over `db` under `limits`.
fn run(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: MegaTransaction,
) -> MegaTransactionOutcome {
    execute_under(db, tx, limits)
}

/// The history gas an empty transaction body costs, which every transaction here pays before its
/// first frame and keeps whatever that frame does, at the price the engine runs.
fn body_history() -> u64 {
    mega_evm::history_gas(mega_evm::TX_BODY_SIZE).expect("a body has a price")
}

/// The same for a creation transaction, whose init code travels as the body's data.
fn create_body_history(init_code_len: u64) -> u64 {
    mega_evm::history_gas(mega_evm::TX_BODY_SIZE + init_code_len).expect("a body has a price")
}

/// A transaction limit that leaves `execution_bytes` above the body.
fn cap(execution_bytes: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::no_limits()
        .with_tx_data_size_limit(mega_evm::TX_BODY_SIZE.saturating_add(execution_bytes))
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

/// Crossing the cap at depth 3 reverts `D` alone, and every caller resumes, held to what the
/// transaction has left: `C`'s log after its second write crosses the cap again and stops `C`,
/// `B` writes and logs within it, and `A`'s second write crosses it once more and stops the
/// transaction's own frame, which takes back everything below it. Each stop names the cap, and
/// at no point does the transaction keep more than it.
///
/// What each crossing reached, above the body: `A`, `B`, `C` and `D`'s first writes are four
/// records (160 bytes), and `D`'s second makes 200; `C`'s second write makes 160 again, and its
/// log 192; `B`'s second write and log make 152; `A`'s second write 192.
#[test]
fn test_cap_crossed_at_depth_three_stops_each_frame_that_crosses_it() {
    let limit = mega_evm::TX_BODY_SIZE + 180;
    let stop = limit_exceeded(LimitKind::DataSize, limit);
    for inspect in [false, true] {
        let mut evm = evm_with(
            chain(),
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
            Probe::default(),
        );
        alloy_evm::Evm::set_inspector_enabled(&mut evm, inspect);
        let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();

        assert_stopped(&outcome.result, LimitKind::DataSize, limit);
        assert_eq!(
            outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit,
                used: mega_evm::TX_BODY_SIZE + 192,
                frame_local: false
            }),
            "A's own crossing produced its result"
        );
        assert!(outcome.result.logs().is_empty());
        for account in [A, B, C, D] {
            let storage = &outcome.state[&account].storage;
            assert!(storage.values().all(|slot| !slot.is_changed()), "{account} kept a write");
        }
        assert_eq!(outcome.usage.data_size, mega_evm::TX_BODY_SIZE, "the body alone stays");
        if inspect {
            let probe = evm.inspector();
            let sstores = probe.steps.iter().filter(|op| matches!(op, &&SSTORE)).count();
            assert_eq!(sstores, 8, "D's two writes and every frame's first and second");
            let logs = probe.steps.iter().filter(|op| matches!(op, &&LOG0)).count();
            assert_eq!(logs, 2, "C's and B's");
            assert_eq!(probe.steps.last(), Some(&SSTORE), "A's second write was the last step");
            assert_eq!(
                probe.call_results,
                vec![
                    (D, InstructionResult::Revert, stop.clone()),
                    (C, InstructionResult::Revert, stop.clone()),
                    (B, InstructionResult::Stop, Bytes::new()),
                    (A, InstructionResult::Revert, stop.clone()),
                ],
                "each frame that crossed returned the stop, the deepest first"
            );
        }
    }
}

/// The stopped transaction gives the whole reservoir back, and spends the same gas whatever its
/// gas limit, so a limit above the execution cap changes nothing but the refund.
#[test]
fn test_stop_refills_the_reservoir_at_any_gas_limit() {
    let at = |gas_limit: u64| {
        let result = run(chain(), cap(180), call(CALLER, A, U256::ZERO, gas_limit));
        assert_stopped(&result.result, LimitKind::DataSize, mega_evm::TX_BODY_SIZE + 180);
        result
    };
    let small = at(100_000_000);
    let large = at(1_000_000_000);
    assert_eq!(small.gas.reservoir_remaining, 0, "a limit under the cap has no reservoir");
    assert_eq!(
        large.gas.reservoir_remaining,
        1_000_000_000 - TX_GAS_LIMIT_CAP - body_history(),
        "all of it back but the body's history, which the stop does not take back",
    );
    assert_eq!(small.result.gas().tx_gas_used(), large.result.gas().tx_gas_used());
    assert_eq!(small.result.gas().total_gas_spent(), large.result.gas().total_gas_spent());
}

/// An `SSTORE` that runs out of gas after its Host call records nothing, so a cap of zero does
/// not stop the transaction: it halts out of gas and burns its gas.
#[test]
fn test_true_out_of_gas_still_halts_and_burns() {
    let db = MemoryDatabase::default().account_code(A, writer());
    let gas_limit = 21_000 + body_history() + 5_000;
    let result = run(db, cap(0), call(CALLER, A, U256::ZERO, gas_limit));
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
    assert_eq!(result.limit_exceeded, None);
}

/// A child that runs out of gas halts alone; its caller goes on and writes within the cap.
#[test]
fn test_child_out_of_gas_under_a_cap_is_no_stop() {
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
    let result = run(db, cap(40), call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.state[&A].storage[&U256::from(9)].present_value(), U256::from(1));
    assert_eq!(result.limit_exceeded, None);
    assert!(
        result.state[&B].storage.values().all(|slot| !slot.is_changed()),
        "B's writes went with its halt"
    );
    assert_eq!(
        result.usage,
        mega_evm::LimitUsage {
            data_size: mega_evm::TX_BODY_SIZE + mega_evm::WRITE_RECORD_SIZE,
            write_records: 1
        },
        "A's record alone, which meets the cap and does not cross it"
    );
}

/// A cap crossed by the writes the first frame's start makes (a value transfer's recipient)
/// reverts the transaction before the frame runs: the recipient's code never runs, its balance
/// is untouched, and the transaction spends the transfer's intrinsic gas alone — the recipient
/// is never created, so its state gas is never drawn.
#[test]
fn test_cap_crossed_before_the_first_frame_reverts_without_running() {
    if runs_at_measurement_prices() {
        return;
    }
    let funded = || MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));
    let mut evm = evm_with(funded().account_code(B, writer()), cap(39), Probe::default());
    let result = evm.transact_raw(call(CALLER, B, U256::from(5), GAS_LIMIT)).unwrap();
    assert_stopped(&result.result, LimitKind::DataSize, mega_evm::TX_BODY_SIZE + 39);
    assert!(evm.inspector().steps.is_empty(), "the recipient's code never ran");
    assert_eq!(result.state.get(&B).map(|b| b.info.balance).unwrap_or_default(), U256::ZERO);

    assert_eq!(
        evm.ctx().additional_limit().usage(),
        mega_evm::LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
        "the body stays; the recipient record does not"
    );

    let transfer = run(funded(), cap(u64::MAX), call(CALLER, B, U256::from(5), GAS_LIMIT));
    assert!(transfer.result.is_success());
    assert_eq!(
        result.result.gas().tx_gas_used(),
        21_000 + body_history(),
        "the transfer's intrinsic gas and its body",
    );
    assert_eq!(
        transfer.result.gas().tx_gas_used(),
        result.result.gas().tx_gas_used() +
            ACCOUNT_STATE_GAS +
            mega_evm::WRITE_RECORD_SIZE * mega_evm::constants::COST_PER_HISTORY_BYTE,
        "the transfer that goes through creates the recipient, and keeps its write record"
    );
}

/// A frame budget reverts the frame that crosses it and nothing else: the caller resumes and the
/// transaction reports no stop.
#[test]
fn test_frame_budget_reverts_the_frame_alone() {
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
    assert_eq!(evm.ctx().additional_limit().limit_exceeded(), None);
    let (target, outcome, output) = evm.inspector().call_results[0].clone();
    assert_eq!((target, outcome), (B, InstructionResult::Revert));
    // B's budget is 98% of the 100 bytes A was capped at, and B had used none of A's own.
    assert_eq!(output, limit_exceeded(LimitKind::DataSize, 98));
    assert!(evm.inspector().steps.contains(&SSTORE));
}

/// A frame budget crossed by the outermost frame reverts the transaction, and the outcome names
/// it: it produced the result of the transaction's own frame. The third write's record crosses
/// the 100 bytes.
#[test]
fn test_frame_budget_of_the_outermost_frame_reverts_the_transaction() {
    let db = MemoryDatabase::default().account_code(A, writer());
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(100);
    let result = run(db, limits, call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert_stopped(&result.result, LimitKind::DataSize, 100);
    assert_eq!(
        result.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 100,
            used: 3 * mega_evm::WRITE_RECORD_SIZE,
            frame_local: true,
        })
    );
    assert_eq!(
        result.usage,
        mega_evm::LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
        "the body is all the transaction keeps"
    );
}

/// The layer's state belongs to one transaction: the stop it reported and the usage counted do
/// not reach the next transaction or system call, through any entry point.
#[test]
fn test_the_stop_does_not_outlive_its_transaction() {
    let db = chain().account_code(B, writer());
    let mut evm =
        MegaEvm::new(context(db).with_tx_runtime_limits(cap(100))).with_inspector(Probe::default());
    let stopped = |evm: &MegaEvm<MemoryDatabase, Probe>| {
        evm.ctx().additional_limit().limit_exceeded().is_some()
    };

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
    assert_eq!(
        evm.ctx().additional_limit().usage(),
        mega_evm::LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
        "an empty call keeps its body"
    );

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

/// A creation transaction stopped before its first frame runs — its body fits the limit, the record
/// of the account it creates does not — still bumps the sender's nonce, like any included creation
/// transaction: it cannot be replayed.
#[test]
fn test_create_transaction_stopped_before_its_first_frame_bumps_the_nonce() {
    let init_code = writer();
    let room = init_code.len() as u64 + mega_evm::WRITE_RECORD_SIZE - 1;
    let tx = crate::common::create(CALLER, init_code.clone(), GAS_LIMIT);
    let result = run(MemoryDatabase::default(), cap(room), tx);
    assert_stopped(&result.result, LimitKind::DataSize, mega_evm::TX_BODY_SIZE + room);
    assert!(result.limit_exceeded.is_some());
    assert_eq!(result.state[&CALLER].info.nonce, 1, "the sender's nonce is bumped");
    assert!(result.state.get(&CALLER.create(0)).is_none_or(|a| a.info.is_empty_code_hash()));
    assert!(result.result.gas().tx_gas_used() < GAS_LIMIT / 2, "the stop burns nothing");

    // The stopped creation's result carries the reservoir back.
    let tx = crate::common::create(CALLER, init_code.clone(), 1_000_000_000);
    let result = run(MemoryDatabase::default(), cap(room), tx);
    assert_eq!(
        result.gas.reservoir_remaining,
        1_000_000_000 - TX_GAS_LIMIT_CAP - create_body_history(init_code.len() as u64),
    );
}

/// A limit is crossed by usage above it: usage equal to the limit does not stop a transaction or
/// a frame.
#[test]
fn test_usage_equal_to_the_limit_does_not_stop() {
    // A writes three slots: 120 bytes.
    let db = || MemoryDatabase::default().account_code(A, writer());
    let result = run(db(), cap(120), call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.limit_exceeded, None);
    let budget = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(120);
    let result = run(db(), budget, call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success(), "{:?}", result.result);
    let result = run(db(), cap(119), call(CALLER, A, U256::ZERO, GAS_LIMIT));
    assert_stopped(&result.result, LimitKind::DataSize, mega_evm::TX_BODY_SIZE + 119);
}

/// A tool's inspector that writes to the running frame's gas.
#[derive(Default)]
struct Charger {
    opcode: u8,
    /// State gas charged after the first `SSTORE` of the transaction.
    state_gas: u64,
    /// History gas charged after the first `SSTORE` of the transaction.
    history_gas: u64,
    charged: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Charger {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        self.opcode = interp.bytecode.opcode();
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<DB>,
    ) {
        if self.opcode == SSTORE && !self.charged {
            self.charged = true;
            assert!(interp.gas.record_state_cost(self.state_gas));
            assert!(interp.gas.record_history_cost(self.history_gas));
        }
    }
}

fn execute_with(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    charger: Charger,
    gas_limit: u64,
) -> mega_evm::MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .with_inspector(charger)
        .execute_transaction(call(CALLER, A, U256::ZERO, gas_limit))
        .unwrap()
}

/// State gas drawn from the reservoir before the stop is refilled by it: the stopped transaction
/// hands the whole reservoir back, where a transaction that kept the state keeps the charge.
#[test]
fn test_stop_refills_the_state_gas_drawn_from_the_reservoir() {
    let gas_limit = 1_000_000_000;
    // The body's history comes out of the reservoir too, and the stop does not take it back.
    let reservoir = gas_limit - TX_GAS_LIMIT_CAP - body_history();
    let charger = || Charger { state_gas: 50_000, ..Default::default() };
    // The chain's own new slots draw state gas from the same reservoir, so the charger's 50,000
    // is measured against a run that charges nothing extra.
    let plain =
        execute_with(chain(), EvmTxRuntimeLimits::no_limits(), Charger::default(), gas_limit);
    let kept = execute_with(chain(), EvmTxRuntimeLimits::no_limits(), charger(), gas_limit);
    assert!(kept.result.is_success());
    assert_eq!(
        kept.gas.reservoir_remaining,
        plain.gas.reservoir_remaining - 50_000,
        "the kept charge"
    );
    assert_eq!(kept.gas.state, plain.gas.state + 50_000);
    assert_eq!(plain.gas.state, 9 * slot_state_gas(), "the chain's nine slots are state gas");

    let stopped = execute_with(chain(), cap(180), charger(), gas_limit);
    assert_stopped(&stopped.result, LimitKind::DataSize, mega_evm::TX_BODY_SIZE + 180);
    assert_eq!(stopped.gas.reservoir_remaining, reservoir, "refilled by the stop");
    assert_eq!(stopped.gas.state, 0);
}

/// History gas a transaction spent is reported on its outcome, apart from regular gas; a stop
/// takes it back with the rest.
#[test]
fn test_outcome_reports_history_gas() {
    let charger = || Charger { history_gas: 700, ..Default::default() };
    let kept = execute_with(chain(), EvmTxRuntimeLimits::no_limits(), charger(), GAS_LIMIT);
    let plain =
        execute_with(chain(), EvmTxRuntimeLimits::no_limits(), Charger::default(), GAS_LIMIT);
    assert!(kept.result.is_success());
    assert_eq!(kept.gas.history, plain.gas.history + 700);
    assert_eq!(
        plain.gas.history,
        body_history() +
            9 * history(mega_evm::WRITE_RECORD_SIZE) +
            3 * history(mega_evm::LOG_BASE_SIZE),
        "the chain's own writes are history too: nine records and three empty logs",
    );
    assert_eq!(kept.gas.regular, plain.gas.regular, "history is not regular gas");
    assert_eq!(kept.gas.gas_used, plain.gas.gas_used + 700);

    // The body is charged before the first frame, so the stop keeps it and takes back every
    // charge the frames made, the inspector's included.
    let stopped = execute_with(chain(), cap(180), charger(), GAS_LIMIT);
    assert_stopped(&stopped.result, LimitKind::DataSize, mega_evm::TX_BODY_SIZE + 180);
    assert_eq!(stopped.gas.history, body_history());
}
