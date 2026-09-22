//! The history bytes a transaction appended, reported beside the history gas it paid.
//!
//! Every history charge is a byte count at the cost per history byte, and the transaction reports
//! the count as well as the gas. Without a history allowance the two are a price apart. A value
//! transfer's allowance pays for its callee's first event before the callee's gas does, and what
//! it pays for is on no gas ledger, so the byte count is larger than the gas says — by exactly
//! what the allowances paid.
//!
//! Every case runs twice: below the execution cap, where the reservoir is empty and every charge
//! spills onto regular gas, and above it, where the reservoir pays first.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{COST_PER_HISTORY_BYTE, TX_GAS_LIMIT_CAP},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaTransaction, MegaTransactionOutcome, LOG_BASE_SIZE, LOG_TOPIC_SIZE,
    STORAGE_CALL_STIPEND_BYTES, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, LOG0, POP, PUSH0, PUSH1, RETURN, REVERT},
    interpreter::{
        interpreter::EthInterpreter, CreateInputs, CreateOutcome, Gas, InstructionResult,
        InterpreterResult,
    },
    Inspector,
};

use crate::common::{call, call_with_data, context, create, execute, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000a00000");
const CALLEE: Address = address!("0000000000000000000000000000000000a00001");
const CHILD: Address = address!("0000000000000000000000000000000000a00002");
const RECEIVER: Address = address!("0000000000000000000000000000000000a00003");
const OTHER_RECEIVER: Address = address!("0000000000000000000000000000000000a00004");
/// An account nothing has touched.
const FRESH: Address = address!("0000000000000000000000000000000000a00005");
/// An account that exists and has no code.
const PAYEE: Address = address!("0000000000000000000000000000000000a00006");

const CPHB: u64 = COST_PER_HISTORY_BYTE;

/// The reservoir the runs above the execution cap carry.
const RESERVOIR: u64 = 100_000_000;

/// The two gas limits every case runs at: below the execution cap and above it.
const GAS_LIMITS: [u64; 2] = [50_000_000, TX_GAS_LIMIT_CAP + RESERVOIR];

/// Bytes the deployments here leave behind.
const DEPLOYED: u64 = 32;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(10u64.pow(9)))
        .account_balance(PAYEE, U256::from(1))
}

/// A log with `topics` topics over the first `len` bytes of memory.
fn log(code: BytecodeBuilder, topics: u8, len: u64) -> BytecodeBuilder {
    let mut code = code;
    for topic in 0..topics {
        code = code.push_number(u64::from(topic) + 1);
    }
    code.push_number(len).push_number(0u64).append(LOG0 + topics)
}

/// The bytes a log with `topics` topics and `len` bytes of data appends.
const fn log_bytes(topics: u64, len: u64) -> u64 {
    LOG_BASE_SIZE + topics * LOG_TOPIC_SIZE + len
}

/// Init code that deploys [`DEPLOYED`] zero bytes: `PUSH1 32; PUSH0; RETURN`.
fn deploying() -> Bytes {
    BytecodeBuilder::default().push_number(DEPLOYED).append_many([PUSH0, RETURN]).build()
}

/// `CREATE` over `init_code` placed in memory, discarding the address.
fn creating(code: BytecodeBuilder, init_code: &[u8]) -> BytecodeBuilder {
    code.mstore(0, init_code)
        .push_number(init_code.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .append(POP)
}

/// `CALL(gas, target, value, 0, 0, 0, 0)`, discarding the flag.
fn calling(code: BytecodeBuilder, target: Address, value: u64, gas: u64) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(value)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
        .append(POP)
}

/// `funded()` with `code` at [`CALLEE`].
fn callee_running(code: BytecodeBuilder) -> MemoryDatabase {
    funded().account_code(CALLEE, code.stop().build())
}

/// Asserts what the reservoir of a run above the execution cap says about its two ledgers: what
/// the transaction spent on state and history came out of it, and nothing else did.
fn assert_reservoir_paid(name: &str, gas_limit: u64, outcome: &MegaTransactionOutcome) {
    if gas_limit <= TX_GAS_LIMIT_CAP {
        return;
    }
    let reservoir = gas_limit - TX_GAS_LIMIT_CAP;
    assert!(outcome.gas.reservoir_remaining > 0, "{name}: the reservoir is not exhausted");
    assert_eq!(
        outcome.gas.reservoir_remaining,
        reservoir - outcome.gas.state - outcome.gas.history,
        "{name}: the reservoir paid the state and history ledgers and nothing else",
    );
}

/// With no allowance anywhere, the bytes a transaction reports are its history gas at the price,
/// at every site that appends history and on every path that takes it back.
#[test]
fn test_without_an_allowance_the_bytes_are_the_history_gas_at_the_price() {
    if runs_at_measurement_prices() {
        return;
    }
    type Case = (&'static str, fn() -> MemoryDatabase, fn(u64) -> MegaTransaction, u64);
    let cases: [Case; 12] = [
        (
            "a body alone",
            || callee_running(BytecodeBuilder::default()),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            TX_BODY_SIZE,
        ),
        (
            "calldata, a byte a byte",
            || callee_running(BytecodeBuilder::default()),
            |gas| call_with_data(CALLER, CALLEE, Bytes::from(vec![0xab; 100]), gas),
            TX_BODY_SIZE + 100,
        ),
        (
            "a log",
            || callee_running(log(BytecodeBuilder::default(), 2, 50)),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            TX_BODY_SIZE + log_bytes(2, 50),
        ),
        (
            "a storage write",
            || callee_running(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1))),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
        ),
        (
            "a storage write written back",
            || {
                callee_running(
                    BytecodeBuilder::default()
                        .sstore(U256::from(1), U256::from(1))
                        .sstore(U256::from(1), U256::ZERO),
                )
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            TX_BODY_SIZE,
        ),
        (
            "a transfer that creates its recipient",
            funded,
            |gas| call(CALLER, FRESH, U256::from(1), gas),
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
        ),
        (
            "a creation transaction",
            funded,
            |gas| create(CALLER, deploying(), gas),
            TX_BODY_SIZE + deploying().len() as u64 + WRITE_RECORD_SIZE + DEPLOYED,
        ),
        (
            "a nested creation: the created account, the creator's nonce and the code",
            || callee_running(creating(BytecodeBuilder::default(), &deploying())),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE + DEPLOYED,
        ),
        (
            "a nested creation that reverts with a word of data: the creator's nonce outlives \
             it, and the data is no code",
            || callee_running(creating(BytecodeBuilder::default(), &[PUSH1, 32, PUSH0, REVERT])),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
        ),
        (
            "a child that logs and reverts",
            || {
                callee_running(calling(BytecodeBuilder::default(), CHILD, 0, 1_000_000))
                    .account_code(CHILD, log(BytecodeBuilder::default(), 0, 32).revert().build())
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            TX_BODY_SIZE,
        ),
        (
            "a transaction whose own frame writes, logs, deploys and reverts",
            || {
                let code = creating(
                    log(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), 1, 32),
                    &deploying(),
                );
                funded().account_code(CALLEE, code.revert().build())
            },
            |gas| call(CALLER, CALLEE, U256::from(1), gas),
            TX_BODY_SIZE,
        ),
        (
            "every site at once",
            || {
                let code = creating(
                    calling(
                        log(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), 3, 64),
                        FRESH,
                        1,
                        1_000_000,
                    ),
                    &deploying(),
                );
                callee_running(code)
            },
            |gas| call_with_data(CALLER, CALLEE, Bytes::from(vec![1; 10]), gas),
            // The body and its calldata; the slot; the log; the transfer's two records (the
            // callee's account and the recipient's), which leave the creation none of its own
            // for the callee's nonce; the created account and its code.
            TX_BODY_SIZE +
                10 +
                WRITE_RECORD_SIZE +
                log_bytes(3, 64) +
                2 * WRITE_RECORD_SIZE +
                WRITE_RECORD_SIZE +
                DEPLOYED,
        ),
    ];

    for (name, db, tx, bytes) in cases {
        for gas_limit in GAS_LIMITS {
            let outcome = execute(db(), tx(gas_limit));
            assert_eq!(outcome.gas.history_bytes, bytes, "{name} at {gas_limit}: the bytes");
            assert_eq!(
                outcome.gas.history,
                bytes * CPHB,
                "{name} at {gas_limit}: the history gas is the bytes at the price",
            );
            assert_reservoir_paid(name, gas_limit, &outcome);
        }
    }
}

/// A transaction whose gas cannot pay for the record its own frame makes runs out of gas before
/// that frame, and the record is not made: it reports its body alone, in bytes and in gas.
#[test]
fn test_a_transaction_that_cannot_pay_its_first_record_reports_its_body_alone() {
    if runs_at_measurement_prices() {
        return;
    }
    let transfer = |gas| call(CALLER, PAYEE, U256::from(1), gas);
    // What the transfer spends is its intrinsic gas and the one record, nothing else: that is the
    // least gas limit that pays for all of it.
    let ample = execute(funded(), transfer(GAS_LIMITS[0]));
    assert!(ample.result.is_success(), "{:?}", ample.result);
    let fits = ample.gas.gas_used;

    let exact = execute(funded(), transfer(fits));
    assert!(exact.result.is_success(), "{:?}", exact.result);
    assert_eq!(exact.gas.history_bytes, TX_BODY_SIZE + WRITE_RECORD_SIZE);
    assert_eq!(exact.gas.history, exact.gas.history_bytes * CPHB);

    let short = execute(funded(), transfer(fits - 1));
    assert!(short.result.is_halt(), "{:?}", short.result);
    assert_eq!(short.usage.write_records, 0, "no frame ran, so no record was made");
    assert_eq!(short.gas.history_bytes, TX_BODY_SIZE, "the body alone");
    assert_eq!(short.gas.history, TX_BODY_SIZE * CPHB);
}

/// Answers every creation itself with a success whose output is a word of bytes, so no frame runs
/// and nothing is deposited.
struct AnswersCreations;

impl Inspector<MegaContext<MemoryDatabase>, EthInterpreter> for AnswersCreations {
    fn create(
        &mut self,
        _context: &mut MegaContext<MemoryDatabase>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        Some(CreateOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                Bytes::from(vec![0xfe; 32]),
                Gas::new(inputs.gas_limit()),
            ),
            Some(CHILD),
        ))
    }
}

/// A creation answered without running deposits nothing, whatever the answer's output: the bytes
/// the transaction reports are what the history ledger charged for, and the charge its caller made
/// for the creation's records comes back with the creation that never made them.
#[test]
fn test_a_creation_answered_without_running_appends_no_code() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = callee_running(creating(BytecodeBuilder::default(), &deploying()));
    let outcome = MegaEvm::new(context(db))
        .with_inspector(AnswersCreations)
        .execute_transaction(call(CALLER, CALLEE, U256::ZERO, GAS_LIMITS[0]))
        .expect("the transaction is valid");

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 0, "the creation that never ran wrote nothing");
    assert_eq!(outcome.gas.history_bytes, TX_BODY_SIZE, "and appended nothing: its body alone");
    assert_eq!(outcome.gas.history, TX_BODY_SIZE * CPHB);
}

/* ---------- where an allowance pays ---------- */

/// One three-topic event over one word: the event the allowance is sized for.
fn event() -> BytecodeBuilder {
    log(BytecodeBuilder::default(), 3, 32)
}

/// A value call's history allowance pays for bytes its callee appends, and those bytes are on no
/// gas ledger: the byte count exceeds the history gas by exactly what the allowances paid, at
/// most one allowance per value call, and nothing for an event its frame did not keep.
#[test]
fn test_the_bytes_exceed_the_history_gas_by_what_the_allowances_paid() {
    if runs_at_measurement_prices() {
        return;
    }
    // The two records every transfer below writes: the sender's account and the receiver's.
    let transfer = 2 * WRITE_RECORD_SIZE;

    type Case = (&'static str, fn() -> MemoryDatabase, u64, u64);
    let cases: [Case; 5] = [
        (
            "a transfer's receiver emits the event, and the allowance pays all of it",
            || {
                callee_running(calling(BytecodeBuilder::default(), RECEIVER, 1, 2_300))
                    .account_code(RECEIVER, event().stop().build())
            },
            TX_BODY_SIZE + transfer + log_bytes(3, 32),
            STORAGE_CALL_STIPEND_BYTES,
        ),
        (
            "an event larger than the allowance: the receiver's gas pays the rest",
            || {
                callee_running(calling(BytecodeBuilder::default(), RECEIVER, 1, 100_000))
                    .account_code(RECEIVER, log(BytecodeBuilder::default(), 3, 64).stop().build())
            },
            TX_BODY_SIZE + transfer + log_bytes(3, 64),
            STORAGE_CALL_STIPEND_BYTES,
        ),
        (
            "two events: the allowance pays the first",
            || {
                let code = log(event(), 3, 32).stop().build();
                callee_running(calling(BytecodeBuilder::default(), RECEIVER, 1, 100_000))
                    .account_code(RECEIVER, code)
            },
            TX_BODY_SIZE + transfer + 2 * log_bytes(3, 32),
            STORAGE_CALL_STIPEND_BYTES,
        ),
        (
            "the receiver reverts: the event is not appended, and the transfer's records neither",
            || {
                callee_running(calling(BytecodeBuilder::default(), RECEIVER, 1, 100_000))
                    .account_code(RECEIVER, event().revert().build())
            },
            TX_BODY_SIZE,
            0,
        ),
        (
            "two receivers, two allowances",
            || {
                let code = calling(
                    calling(BytecodeBuilder::default(), RECEIVER, 1, 2_300),
                    OTHER_RECEIVER,
                    1,
                    2_300,
                );
                callee_running(code)
                    .account_code(RECEIVER, event().stop().build())
                    .account_code(OTHER_RECEIVER, event().stop().build())
            },
            // The sender's account is written once; each receiver's once.
            TX_BODY_SIZE + 3 * WRITE_RECORD_SIZE + 2 * log_bytes(3, 32),
            2 * STORAGE_CALL_STIPEND_BYTES,
        ),
    ];

    for (name, db, bytes, paid_by_allowances) in cases {
        for gas_limit in GAS_LIMITS {
            let outcome = execute(db(), call(CALLER, CALLEE, U256::ZERO, gas_limit));
            assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
            assert_eq!(outcome.gas.history_bytes, bytes, "{name} at {gas_limit}: the bytes");
            assert_eq!(
                outcome.gas.history_bytes * CPHB - outcome.gas.history,
                paid_by_allowances * CPHB,
                "{name} at {gas_limit}: the gap is what the allowances paid",
            );
            assert_reservoir_paid(name, gas_limit, &outcome);
        }
    }
}
