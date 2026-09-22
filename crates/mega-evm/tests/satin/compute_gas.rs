//! The compute figure: the regular gas a transaction spent, read off its `Gas` and nothing else.
//!
//! A transaction's raw spend splits into three ledgers — regular, state and history — and the
//! regular one is the compute every limit reads. It is `total − state − history`, from the result
//! revm's `Gas` settled into; there is no counter of its own beside it. The figure a block counts
//! is that ledger at least the EIP-7623 floor, with history taken out *before* the floor is
//! applied: the fork's own `block_regular_gas_used` is `max(total − state, floor)`, which carries
//! history, and taking history out of it afterwards could land below the floor.
//!
//! Every case runs below the execution cap, where the reservoir is empty and state and history
//! spill onto regular gas, and above it, where the reservoir pays them first.

use alloy_evm::{Evm as _, EvmError, InvalidTxError};
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants::{ACCOUNT_STATE_GAS, COST_PER_HISTORY_BYTE, SLOT_STATE_GAS, TX_GAS_LIMIT_CAP},
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase},
    BlockGasCounters, MegaContext, MegaEvm, MegaGasUsage, MegaHaltReason, MegaTransactionOutcome,
    TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        ADD, CALL, DUP1, GAS, JUMPDEST, JUMPI, LOG0, MLOAD, MSTORE, MUL, POP, PUSH0, PUSH1, PUSH3,
        SELFDESTRUCT, SLOAD, SUB, SWAP1,
    },
    context::{
        result::{ExecutionResult, HaltReason, InvalidTransaction, OutOfGasError},
        transaction::{AccessList, AccessListItem, TransactionType},
        TxEnv,
    },
    interpreter::{interpreter::EthInterpreter, Interpreter},
    precompile::{
        bn254::pair,
        hash::{RIPEMD160, SHA256},
        secp256k1::ECRECOVER,
    },
    Inspector,
};

use crate::{
    common::{call, call_with_data, context, execute, runs_at_measurement_prices},
    salt::{crowded_account, minimal_envs},
};

const CALLER: Address = address!("0000000000000000000000000000000000c00000");
const CALLEE: Address = address!("0000000000000000000000000000000000c00001");
const CHILD: Address = address!("0000000000000000000000000000000000c00002");
/// An account nothing has touched.
const FRESH: Address = address!("0000000000000000000000000000000000c00003");

/// The intrinsic gas of a plain call to an existing account: EIP-2780's sender base of 12,000 and
/// 3,000 for reaching the recipient.
const EMPTY_CALL: u64 = 15_000;

/// The two gas limits every case runs at: below the execution cap and above it.
const GAS_LIMITS: [u64; 2] = [10_000_000, TX_GAS_LIMIT_CAP + 100_000_000];

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(1))
}

/// `count` rounds of `PUSH1 1; PUSH1 2; ADD; POP`, eleven gas each, then `STOP`.
fn arithmetic(count: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..count {
        code = code.push_number(1_u8).push_number(2_u8).append(ADD).append(POP);
    }
    code.stop().build()
}

/// The three ledgers split the raw spend, the regular one is what is neither state nor history,
/// and the figure a block counts is that ledger at least the floor. Above the execution cap the
/// reservoir paid the state and history ledgers, and nothing else.
fn assert_ledgers(name: &str, gas_limit: u64, outcome: &MegaTransactionOutcome) {
    let gas = &outcome.gas;
    if gas_limit > TX_GAS_LIMIT_CAP {
        assert_eq!(
            gas.reservoir_remaining,
            gas_limit - TX_GAS_LIMIT_CAP - gas.state - gas.history,
            "{name}: the reservoir paid the state and history ledgers",
        );
    }
    let result = outcome.result.gas();
    assert_eq!(
        gas.regular + gas.state + gas.history,
        result.total_gas_spent(),
        "{name}: the three ledgers split the raw spend",
    );
    assert_eq!(gas.block_execution_gas(), gas.regular.max(gas.floor), "{name}");
    let mut block = BlockGasCounters::default();
    block.record(gas);
    assert_eq!(block.execution, gas.block_execution_gas(), "{name}: the block counts that figure");
}

/* ---------- the floor ---------- */

/// A transaction that carries a kilobyte of calldata and runs nothing is bound by its floor: its
/// regular ledger is the intrinsic gas, far below the floor's sixty-four gas a byte. The block
/// counts the floor.
///
/// Its history — the body and the kilobyte, at the cost per history byte — is larger still, so
/// the order the two are taken out in decides the figure. History comes out first and the floor
/// applies to what is left: the block counts the floor. The fork's own figure keeps the history
/// in and lands far above the floor; taking the history out of that afterwards lands far below
/// it. Neither is what a block counts.
#[test]
fn test_a_floor_bound_transaction_counts_its_floor_and_history_comes_out_first() {
    if runs_at_measurement_prices() {
        return;
    }
    const CALLDATA: u64 = 1_000;
    for gas_limit in GAS_LIMITS {
        let tx = call_with_data(CALLER, CALLEE, Bytes::from(vec![0; CALLDATA as usize]), gas_limit);
        let outcome = execute(funded(), tx);
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_ledgers("floor-bound", gas_limit, &outcome);

        let gas = outcome.gas;
        assert_eq!(gas.history, (TX_BODY_SIZE + CALLDATA) * COST_PER_HISTORY_BYTE);
        assert!(gas.floor > gas.regular, "{gas:?}: the floor binds the regular ledger");
        assert_eq!(gas.block_execution_gas(), gas.floor, "the block counts the floor");

        let fork_figure = outcome.result.gas().block_regular_gas_used();
        assert_eq!(fork_figure, gas.regular + gas.history, "the fork's figure carries history");
        assert!(fork_figure > gas.floor);
        assert!(
            fork_figure - gas.history < gas.floor,
            "taking history out after the floor would count less than the floor",
        );
        assert_eq!(gas.gas_used, gas.regular + gas.history, "the receipt is above its floor");
    }
}

/* ---------- an inspector that edits gas ---------- */

/// Charges `charge` and hands back `give_back` once, after the first instruction.
struct EditsGas {
    charge: u64,
    give_back: u64,
    done: bool,
}

impl EditsGas {
    const fn new(charge: u64, give_back: u64) -> Self {
        Self { charge, give_back, done: false }
    }
}

impl Inspector<MegaContext<MemoryDatabase>, EthInterpreter> for EditsGas {
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<MemoryDatabase>,
    ) {
        if !self.done {
            self.done = true;
            assert!(interp.gas.record_regular_cost(self.charge));
            interp.gas.erase_cost(self.give_back);
        }
    }
}

/// An inspector that charges gas between two instructions, or hands some back, moves the receipt;
/// the compute figure moves with it, by the same amount, because both are read off the same
/// `Gas`. A counter kept beside `Gas` would not have seen the edit at all.
#[test]
fn test_gas_an_inspector_edits_moves_the_compute_figure_with_the_receipt() {
    let run = |inspector: EditsGas, gas_limit: u64| -> MegaGasUsage {
        let outcome = MegaEvm::new(context(funded().account_code(CALLEE, arithmetic(20))))
            .with_inspector(inspector)
            .execute_transaction(call(CALLER, CALLEE, U256::ZERO, gas_limit))
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_ledgers("edited", gas_limit, &outcome);
        outcome.gas
    };

    for gas_limit in GAS_LIMITS {
        let baseline = run(EditsGas::new(0, 0), gas_limit);
        assert!(baseline.regular > baseline.floor + 86, "the floor is out of the way");

        for (charge, give_back) in [(126, 0), (0, 86)] {
            let edited = run(EditsGas::new(charge, give_back), gas_limit);
            let moved = |figure: fn(&MegaGasUsage) -> u64| {
                i128::from(figure(&edited)) - i128::from(figure(&baseline))
            };
            let expected = i128::from(charge) - i128::from(give_back);
            assert_eq!(moved(|gas| gas.gas_used), expected, "the receipt moves");
            assert_eq!(moved(|gas| gas.regular), expected, "the regular ledger moves with it");
            assert_eq!(moved(MegaGasUsage::block_execution_gas), expected, "and the block figure");
            assert_eq!((edited.state, edited.history), (baseline.state, baseline.history));
        }
    }
}

/* ---------- what the figure counts ---------- */

/// Runs `code` at [`CALLEE`] under a plain call at `gas_limit` and requires it to succeed.
fn computes(code: Bytes, gas_limit: u64) -> MegaGasUsage {
    let outcome =
        execute(funded().account_code(CALLEE, code), call(CALLER, CALLEE, U256::ZERO, gas_limit));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_ledgers("computes", gas_limit, &outcome);
    outcome.gas
}

/// What `code` computes beyond an empty contract, at both gas limits, which agree: state and
/// history that spilled onto regular gas below the cap are not computation.
fn computed_beyond_empty(code: Bytes) -> u64 {
    let [below, above] = GAS_LIMITS.map(|gas_limit| {
        computes(code.clone(), gas_limit).regular - computes(Bytes::new(), gas_limit).regular
    });
    assert_eq!(below, above, "below and above the execution cap");
    below
}

/// A call to a contract that does nothing computes its intrinsic gas: the regular ledger is the
/// receipt less the history the body cost.
#[test]
fn test_an_empty_call_computes_its_intrinsic_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    for gas_limit in GAS_LIMITS {
        let gas = computes(Bytes::from_static(&[PUSH0, 0x00]), gas_limit);
        assert_eq!(gas.regular, EMPTY_CALL + 2, "the intrinsic gas and one PUSH0");
        assert_eq!(gas.regular, gas.gas_used - gas.history);
        assert_eq!(gas.history, TX_BODY_SIZE * COST_PER_HISTORY_BYTE);
    }
}

/// Arithmetic is computed opcode by opcode: four pushes, an `ADD`, a `MUL` and two pops.
#[test]
fn test_arithmetic_is_computed_opcode_by_opcode() {
    let code = BytecodeBuilder::default()
        .push_number(1_u8)
        .push_number(2_u8)
        .append(ADD)
        .append(POP)
        .push_number(3_u8)
        .push_number(4_u8)
        .append(MUL)
        .append(POP)
        .stop()
        .build();
    assert_eq!(computed_beyond_empty(code), 4 * 3 + 3 + 5 + 2 * 2);
}

/// Memory expansion is computation: an `MSTORE` of a word at `0x40` grows memory to three words,
/// and an `MLOAD` of the same word grows it no further.
#[test]
fn test_memory_expansion_is_computed() {
    let code = BytecodeBuilder::default()
        .mstore(0x40, [0xff])
        .push_number(0x40_u8)
        .append(MLOAD)
        .append(POP)
        .stop()
        .build();
    // `PUSH32` the word, `PUSH1` the offset, `MSTORE` and three words of memory; `PUSH1`, `MLOAD`,
    // `POP`.
    assert_eq!(computed_beyond_empty(code), 3 + 3 + (3 + 3 * 3) + 3 + 3 + 2);
}

/// A log's own price is computation — its static cost, eight gas a byte and the memory it reads —
/// and its bytes are history, on their own ledger.
#[test]
fn test_a_log_is_computed_and_its_bytes_are_history() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = || BytecodeBuilder::default().push_number(32_u8).append(PUSH0).append(LOG0).stop();
    // `PUSH1`, `PUSH0`, then `LOG0`: 375, eight a byte, and one word of memory.
    assert_eq!(computed_beyond_empty(code().build()), 3 + 2 + 375 + 8 * 32 + 3);
    for gas_limit in GAS_LIMITS {
        let logged = computes(code().build(), gas_limit);
        let quiet = computes(Bytes::new(), gas_limit);
        assert_eq!(logged.history - quiet.history, (32 + 32) * COST_PER_HISTORY_BYTE);
    }
}

/// A fresh slot's state gas and its write record's history are not computation: what is left of
/// the `SSTORE` and the `SLOAD` after them is.
#[test]
fn test_storage_computes_what_is_neither_state_nor_history() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(0xff))
        .append(PUSH0)
        .append(SLOAD)
        .append(POP)
        .stop()
        .build();
    let computed = computed_beyond_empty(code.clone());
    for gas_limit in GAS_LIMITS {
        let gas = computes(code.clone(), gas_limit);
        assert_eq!(gas.state, SLOT_STATE_GAS, "the fresh slot");
        assert_eq!(gas.history, (TX_BODY_SIZE + WRITE_RECORD_SIZE) * COST_PER_HISTORY_BYTE);
        assert_eq!(gas.regular, gas.gas_used - gas.state - gas.history);
        assert_eq!(gas.regular, EMPTY_CALL + computed);
    }
    // Two pushes; a cold `SSTORE` of a fresh slot — the cold surcharge and Osaka's price for
    // setting a slot, which the schedule keeps beside the state gas; `PUSH0`; a warm `SLOAD`;
    // `POP`.
    assert_eq!(computed, 3 + 3 + (2_100 + 20_000) + 2 + 100 + 2);
}

/// A refund lowers the receipt and never the compute figure: clearing a slot and overwriting it
/// compute the same, and the receipt of the clearing one is lower by the refund.
#[test]
fn test_a_refund_does_not_lower_the_compute_figure() {
    /// The refund for clearing a storage slot (EIP-3529).
    const SSTORE_CLEARS_SCHEDULE: u64 = 4_800;
    let writes = |value: u64, gas_limit: u64| {
        let code = BytecodeBuilder::default().sstore(U256::from(7), U256::from(value)).stop();
        let db = funded().account_code(CALLEE, code.build()).account_storage(
            CALLEE,
            U256::from(7),
            U256::from(1),
        );
        let outcome = execute(db, call(CALLER, CALLEE, U256::ZERO, gas_limit));
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        outcome.gas
    };
    for gas_limit in GAS_LIMITS {
        let cleared = writes(0, gas_limit);
        let overwritten = writes(2, gas_limit);
        assert_eq!(overwritten.gas_used - cleared.gas_used, SSTORE_CLEARS_SCHEDULE);
        assert_eq!(cleared.regular, overwritten.regular, "the refund is not taken off compute");
        assert_eq!(cleared.block_execution_gas(), overwritten.block_execution_gas());
    }
}

/// State gas is not computation whichever pool paid it and whatever the bucket charged: a
/// `SELFDESTRUCT` that moves a balance to a new account computes the same with the account's
/// bucket at the minimum and crowded four times over, below the cap and above it.
#[test]
fn test_state_gas_stays_off_the_compute_figure_whatever_it_costs() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = BytecodeBuilder::default().push_address(FRESH).append(SELFDESTRUCT).build();
    let destructs = |m: u64, gas_limit: u64| {
        let db = funded().account_code(CALLEE, code.clone()).account_balance(CALLEE, U256::from(5));
        let envs = crowded_account(minimal_envs(), FRESH, m);
        crate::salt::run(db, envs, call(CALLER, CALLEE, U256::ZERO, gas_limit)).gas
    };
    let regular = destructs(1, GAS_LIMITS[0]).regular;
    for gas_limit in GAS_LIMITS {
        let minimal = destructs(1, gas_limit);
        let crowded = destructs(4, gas_limit);
        assert_eq!(minimal.state, ACCOUNT_STATE_GAS);
        assert_eq!(crowded.state, 4 * ACCOUNT_STATE_GAS);
        assert_eq!((minimal.regular, crowded.regular), (regular, regular));
    }
}

/// `CALL(gas, target, 0, 0, 0, 0, 0)`, discarding the flag.
fn calls(code: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(target)
        .push_number(gas)
        .append(CALL)
        .append(POP)
}

/// A callee running `rounds` rounds of arithmetic, then stopping or reverting.
fn child(rounds: usize, reverts: bool) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..rounds {
        code = code.push_number(1_u8).push_number(2_u8).append(ADD).append(POP);
    }
    if reverts { code.revert() } else { code.stop() }.build()
}

/// What a caller computes when its child runs `rounds` rounds and stops or reverts.
fn with_child(rounds: usize, reverts: bool, gas_limit: u64) -> MegaGasUsage {
    let caller = calls(BytecodeBuilder::default(), CHILD, 1_000_000).stop().build();
    let db = funded().account_code(CALLEE, caller).account_code(CHILD, child(rounds, reverts));
    let outcome = execute(db, call(CALLER, CALLEE, U256::ZERO, gas_limit));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    outcome.gas
}

/// A nested call's computation is the transaction's: a hundred rounds in the callee are a hundred
/// rounds on the compute figure.
#[test]
fn test_a_nested_call_s_computation_is_the_transaction_s() {
    for gas_limit in GAS_LIMITS {
        let delta =
            with_child(100, false, gas_limit).regular - with_child(0, false, gas_limit).regular;
        assert_eq!(delta, 100 * 11);
    }
}

/// A reverted frame gives its state and history back, never its computation: the hundred rounds a
/// callee ran before it reverted stay on the compute figure.
#[test]
fn test_a_reverted_frame_s_computation_stays_on_the_figure() {
    for gas_limit in GAS_LIMITS {
        let delta =
            with_child(100, true, gas_limit).regular - with_child(0, true, gas_limit).regular;
        assert_eq!(delta, 100 * 11);
    }
}

/// The figure belongs to its transaction: the same transaction twice on the same EVM computes the
/// same, whatever ran before it.
#[test]
fn test_each_transaction_computes_its_own() {
    let db = funded().account_code(CALLEE, arithmetic(100));
    let mut evm = MegaEvm::new(context(db));
    let first = evm.execute_transaction(call(CALLER, CALLEE, U256::ZERO, GAS_LIMITS[0])).unwrap();
    let second = evm.execute_transaction(call(CALLER, CALLEE, U256::ZERO, GAS_LIMITS[0])).unwrap();
    assert!(first.result.is_success() && second.result.is_success());
    assert_eq!(first.gas.regular, second.gas.regular);
}

/// A loop of `iterations` turns, twenty-six gas a turn: `PUSH3 n; JUMPDEST; PUSH1 1; SWAP1; SUB;
/// DUP1; PUSH1 4; JUMPI; POP; STOP`.
fn looping(iterations: u32) -> Bytes {
    let [_, a, b, c] = iterations.to_be_bytes();
    Bytes::from(vec![PUSH3, a, b, c, JUMPDEST, PUSH1, 1, SWAP1, SUB, DUP1, PUSH1, 4, JUMPI, POP, 0])
}

/// A transaction that computes for millions of gas reports every one of them.
#[test]
fn test_millions_of_gas_of_computation_are_all_on_the_figure() {
    for gas_limit in GAS_LIMITS {
        let long = computes(looping(100_000), gas_limit);
        let short = computes(looping(1), gas_limit);
        assert_eq!(long.regular - short.regular, 26 * 99_999);
        assert!(long.regular > 2_000_000);
    }
}

/* ---------- the execution cap ---------- */

/// Code that grows memory to a little over eleven megabytes, which costs some 240,000,000 gas
/// in one `MSTORE`: more than the execution cap, less than the gas limits above it.
fn past_the_cap() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1_u8)
        .push_number(11_199_968_u64)
        .append(MSTORE)
        .stop()
        .build()
}

/// Below the cap the gas limit is the compute budget, to the gas: a transaction whose limit is
/// exactly what it spends succeeds, and one gas less runs out.
#[test]
fn test_below_the_cap_the_gas_limit_is_the_compute_budget() {
    let run = |gas_limit: u64| {
        execute(
            funded().account_code(CALLEE, arithmetic(2_000)),
            call(CALLER, CALLEE, U256::ZERO, gas_limit),
        )
    };
    let spent = run(GAS_LIMITS[0]).gas.gas_used;
    let exact = run(spent);
    assert!(exact.result.is_success(), "{:?}", exact.result);
    assert_eq!(exact.gas.gas_used, spent);
    assert!(run(spent - 1).result.is_halt());
}

/// Above the gas limit's own reach, the execution cap is the compute budget: work that needs more
/// than 200,000,000 regular gas runs out of gas at the cap, even with a gas limit that could have
/// paid for it. It is an ordinary out-of-gas — a halt, not a limit stop — the regular ledger is the
/// cap exactly, and the reservoir above the cap comes back less the body's history it paid.
#[test]
fn test_computation_past_the_execution_cap_is_an_ordinary_out_of_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    for reservoir in [50_000_000, 100_000_000] {
        let gas_limit = TX_GAS_LIMIT_CAP + reservoir;
        let outcome = execute(
            funded().account_code(CALLEE, past_the_cap()),
            call(CALLER, CALLEE, U256::ZERO, gas_limit),
        );
        assert!(
            matches!(
                outcome.result,
                ExecutionResult::Halt {
                    reason: MegaHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Memory)),
                    ..
                }
            ),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.limit_exceeded, None, "no limit of this engine stopped it");
        assert_ledgers("past the cap", gas_limit, &outcome);
        assert_eq!(outcome.gas.regular, TX_GAS_LIMIT_CAP, "the whole execution budget, no more");
        assert_eq!(outcome.gas.history, TX_BODY_SIZE * COST_PER_HISTORY_BYTE);
        assert_eq!(outcome.gas.reservoir_remaining, reservoir - outcome.gas.history);
        assert_eq!(outcome.gas.gas_used, TX_GAS_LIMIT_CAP + outcome.gas.history);
    }
}

/// A nested call cannot compute past the cap either. Its callee is forwarded what the cap leaves,
/// runs out of gas and fails; its caller resumes on the sixty-fourth it kept, and the transaction
/// computes less than the cap.
#[test]
fn test_a_nested_call_cannot_compute_past_the_cap() {
    let caller = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CHILD)
        .append(GAS)
        .append(CALL)
        .append(PUSH0)
        .append(revm::bytecode::opcode::SSTORE)
        .stop()
        .build();
    let db = funded().account_code(CALLEE, caller).account_code(CHILD, past_the_cap());
    let outcome = execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMITS[1]));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.state[&CALLEE].storage[&U256::ZERO].present_value,
        U256::ZERO,
        "the call failed, and its flag says so",
    );
    assert!(outcome.gas.regular < TX_GAS_LIMIT_CAP, "{:?}", outcome.gas);
    assert!(outcome.gas.regular > TX_GAS_LIMIT_CAP / 64 * 63, "the callee burned its share");
}

/// A transaction whose intrinsic gas alone is more computation than the cap allows is rejected
/// before it runs, not included as a failure: an access list of 83,328 addresses is 2,400 gas each
/// on top of the call, just past the cap, and one address fewer is admitted.
///
/// Calldata reaches the cap through its floor first, sixty-four gas a byte against sixteen, and is
/// rejected the same way.
#[test]
fn test_intrinsic_gas_past_the_execution_cap_is_rejected_before_it_runs() {
    if runs_at_measurement_prices() {
        return;
    }
    let with_addresses = |addresses: usize| {
        let access_list =
            AccessList(vec![AccessListItem { address: CALLEE, storage_keys: vec![] }; addresses]);
        OpTx(op_transaction(TxEnv {
            tx_type: TransactionType::Eip2930 as u8,
            caller: CALLER,
            kind: TxKind::Call(CALLEE),
            gas_limit: TX_GAS_LIMIT_CAP + 200_000_000,
            access_list,
            ..Default::default()
        }))
    };
    let rejection = |tx| {
        let err = MegaEvm::new(context(funded())).transact_raw(tx).expect_err("rejected");
        err.as_invalid_tx_err().and_then(InvalidTxError::as_invalid_tx_err).cloned()
    };

    let admitted = execute(funded(), with_addresses(83_327));
    assert!(admitted.result.is_success(), "{:?}", admitted.result);
    assert_eq!(admitted.gas.regular, EMPTY_CALL + 2_400 * 83_327);
    assert_eq!(
        rejection(with_addresses(83_328)),
        Some(InvalidTransaction::GasFloorMoreThanGasLimit {
            gas_floor: EMPTY_CALL + 2_400 * 83_328,
            gas_limit: TX_GAS_LIMIT_CAP,
        }),
    );

    let calldata = (TX_GAS_LIMIT_CAP - EMPTY_CALL) / 64 + 1;
    let tx = call_with_data(
        CALLER,
        CALLEE,
        Bytes::from(vec![0; calldata as usize]),
        TX_GAS_LIMIT_CAP + 400_000_000,
    );
    assert_eq!(
        rejection(tx),
        Some(InvalidTransaction::GasFloorMoreThanGasLimit {
            gas_floor: EMPTY_CALL + 64 * calldata,
            gas_limit: TX_GAS_LIMIT_CAP,
        }),
    );
}

/* ---------- precompiles ---------- */

/// `CALL(gas, precompile, 0, 0, input_len, 0, 32)`, discarding the flag.
fn calls_precompile(
    code: BytecodeBuilder,
    precompile: Address,
    input_len: u8,
    gas: u64,
) -> BytecodeBuilder {
    code.push_number(32_u8)
        .append(PUSH0)
        .push_number(input_len)
        .append_many([PUSH0, PUSH0])
        .push_address(precompile)
        .push_number(gas)
        .append(CALL)
        .append(POP)
}

/// What a contract making `calls` computes beyond the same contract forwarding each call no gas at
/// all, which runs no precompile: exactly what the precompiles themselves cost.
fn precompiles_compute(calls: &[(Address, u8, u64)]) -> u64 {
    let program = |forward: bool| {
        let mut code = BytecodeBuilder::default();
        for &(precompile, input_len, gas) in calls {
            code = calls_precompile(code, precompile, input_len, if forward { gas } else { 0 });
        }
        code.stop().build()
    };
    let [below, above] = GAS_LIMITS.map(|gas_limit| {
        computes(program(true), gas_limit).regular - computes(program(false), gas_limit).regular
    });
    assert_eq!(below, above, "below and above the execution cap");
    below
}

/// SHA-256 of one word: 60 and 12 a word.
const SHA256_WORD: u64 = 72;
/// RIPEMD-160 of one word: 600 and 120 a word.
const RIPEMD160_WORD: u64 = 720;
/// ECRECOVER, whatever its input.
const ECRECOVER_COST: u64 = 3_000;
/// The BN254 pairing check of one pair: 45,000 for the call and 34,000 a pair.
const PAIRING_ONE_PAIR: u64 = 79_000;

/// A precompile is computed at its price, and precompiles one after another add up.
#[test]
fn test_precompiles_are_computed_at_their_price_and_add_up() {
    let sha = (*SHA256.address(), 32, 100_000);
    let ripemd = (*RIPEMD160.address(), 32, 100_000);
    let ecrecover = (*ECRECOVER.address(), 128, 100_000);
    assert_eq!(precompiles_compute(&[sha]), SHA256_WORD);
    assert_eq!(
        precompiles_compute(&[sha, ripemd, ecrecover]),
        SHA256_WORD + RIPEMD160_WORD + ECRECOVER_COST
    );
}

/// A precompile runs on exactly its price and not one gas less; forwarded less, it fails and
/// burns everything it was forwarded, which is computation too. The expensive pairing check
/// holds to the same rule as a hash.
#[test]
fn test_a_precompile_runs_on_exactly_its_price() {
    for (precompile, input_len, price) in
        [(*SHA256.address(), 32, SHA256_WORD), (pair::ADDRESS, 192, PAIRING_ONE_PAIR)]
    {
        assert_eq!(precompiles_compute(&[(precompile, input_len, price)]), price);
        assert_eq!(precompiles_compute(&[(precompile, input_len, price - 1)]), price - 1);
    }
}

/// A precompile a nested frame calls is computed where it runs, and the transaction counts it:
/// the same hash costs the same from the callee as from the transaction's own frame.
#[test]
fn test_a_precompile_in_a_nested_call_is_computed() {
    let child = |gas: u64| calls_precompile(BytecodeBuilder::default(), *SHA256.address(), 32, gas);
    let [below, above] = GAS_LIMITS.map(|gas_limit| {
        let run = |gas: u64| {
            let caller = calls(BytecodeBuilder::default(), CHILD, 1_000_000).stop().build();
            let db = funded()
                .account_code(CALLEE, caller)
                .account_code(CHILD, child(gas).stop().build());
            let outcome = execute(db, call(CALLER, CALLEE, U256::ZERO, gas_limit));
            assert!(outcome.result.is_success(), "{:?}", outcome.result);
            outcome.gas.regular
        };
        run(100_000) - run(0)
    });
    assert_eq!(below, SHA256_WORD, "below the execution cap");
    assert_eq!(above, SHA256_WORD, "above it");
}

/// Opcodes and precompiles in one frame add up: a hundred rounds of arithmetic, a hash and a
/// hundred more rounds compute the rounds and the hash.
#[test]
fn test_opcodes_and_precompiles_add_up() {
    let program = |gas: u64| {
        let mut code = BytecodeBuilder::default();
        for _ in 0..100 {
            code = code.push_number(1_u8).push_number(2_u8).append(ADD).append(POP);
        }
        code = calls_precompile(code, *SHA256.address(), 32, gas);
        for _ in 0..100 {
            code = code.push_number(3_u8).push_number(4_u8).append(MUL).append(POP);
        }
        code.stop().build()
    };
    for gas_limit in GAS_LIMITS {
        let mixed = computes(program(100_000), gas_limit).regular;
        let without_the_hash = computes(program(0), gas_limit).regular;
        let bare = computes(
            calls_precompile(BytecodeBuilder::default(), *SHA256.address(), 32, 0).stop().build(),
            gas_limit,
        )
        .regular;
        assert_eq!(mixed - without_the_hash, SHA256_WORD);
        assert_eq!(
            without_the_hash - bare,
            100 * 11 + 100 * 13,
            "the rounds, eleven and thirteen gas"
        );
    }
}
