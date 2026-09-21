//! History gas: what a Satin transaction pays for the bytes it appends to the chain's history.
//!
//! Every charge is a byte count at the cost per history byte, and every assertion here writes the
//! count out, so a repricing moves the numbers without rewriting the test.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    LOG_BASE_SIZE, LOG_TOPIC_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, GAS, LOG0, POP, PUSH0, RETURN, REVERT},
    context_interface::cfg::GasId,
};

use crate::common::{call, call_with_data, create, execute, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const CALLEE: Address = address!("0000000000000000000000000000000000200001");
const CONTRACT: Address = address!("0000000000000000000000000000000000200002");

/// Room for the state gas of a new account and the code it deposits.
const GAS_LIMIT: u64 = 50_000_000;

/// The cost per history byte, spelled as the constant the tests price against.
const CPHB: u64 = COST_PER_HISTORY_BYTE;

/// The history a transaction's body costs when it carries `bytes` bytes beside its envelope.
const fn body(bytes: u64) -> u64 {
    (TX_BODY_SIZE + bytes) * CPHB
}

fn funded() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)))
}

/// Init code that deploys `len` zero bytes: `PUSH len; PUSH0; RETURN`.
fn deploying(len: u64) -> Bytes {
    BytecodeBuilder::default().push_number(len).append_many([PUSH0, RETURN]).build()
}

/// Init code that reverts without deploying anything.
fn reverting() -> Bytes {
    BytecodeBuilder::default().append_many([PUSH0, PUSH0, REVERT]).build()
}

/// Deployed code is both state and history: its bytes enter the world state and the chain's
/// history, so a deployment pays the per-byte state rate and the per-byte history rate on the
/// same length.
#[test]
fn test_deployed_code_pays_history_for_every_byte() {
    if runs_at_measurement_prices() {
        return;
    }
    let short = execute(funded(), create(CALLER, deploying(32), GAS_LIMIT));
    let long = execute(funded(), create(CALLER, deploying(64), GAS_LIMIT));

    assert!(short.result.is_success(), "{:?}", short.result);
    assert!(long.result.is_success(), "{:?}", long.result);
    assert_eq!(long.gas.history - short.gas.history, 32 * CPHB, "32 more bytes of history");
    assert_eq!(long.gas.state - short.gas.state, 32 * COST_PER_STATE_BYTE, "and of state");
}

/// A deployment that reverts appends no code, so it pays no code-deposit history at all.
#[test]
fn test_a_reverted_deployment_pays_no_code_deposit_history() {
    if runs_at_measurement_prices() {
        return;
    }
    let deployed = execute(funded(), create(CALLER, deploying(32), GAS_LIMIT));
    let reverted = execute(funded(), create(CALLER, reverting(), GAS_LIMIT));

    assert!(deployed.result.is_success(), "{:?}", deployed.result);
    assert!(!reverted.result.is_success(), "the creation reverts");
    assert_eq!(
        deployed.gas.history,
        body(deploying(32).len() as u64) + WRITE_RECORD_SIZE * CPHB + 32 * CPHB,
        "the body the init code travels in, the created account's record, the deployed bytes",
    );
    assert_eq!(
        reverted.gas.history,
        body(reverting().len() as u64),
        "the body alone: nothing was deployed, and the account's record went with the failure",
    );
}

/* ---------- the transaction body ---------- */

/// Every transaction pays for its own body before it runs: the envelope and the records of the
/// writes its inclusion makes, whatever else it does.
#[test]
fn test_every_transaction_pays_for_its_body() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = || funded().account_code(CALLEE, Bytes::new());
    let empty = execute(db(), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));

    assert!(empty.result.is_success(), "{:?}", empty.result);
    assert_eq!(empty.gas.history, body(0));
    assert_eq!(empty.gas.history, 310 * 88, "310 bytes at the cost per history byte");
    assert_eq!(empty.gas.state, 0, "the body is history, not state");
}

/// Calldata is part of the body, one history byte per byte, whatever the bytes are: a zero byte
/// takes the same space in a block as a non-zero one.
#[test]
fn test_calldata_costs_one_history_byte_per_byte() {
    if runs_at_measurement_prices() {
        return;
    }
    let run = |len: usize, byte: u8| {
        let db = funded().account_code(CALLEE, Bytes::new());
        let data = Bytes::from(vec![byte; len]);
        execute(db, call_with_data(CALLER, CALLEE, data, GAS_LIMIT))
    };
    let zeros = run(100, 0x00);
    let non_zeros = run(100, 0xff);
    let longer = run(200, 0x00);

    assert_eq!(zeros.gas.history, body(100));
    assert_eq!(non_zeros.gas.history, zeros.gas.history, "the value of a byte is not its size");
    assert_eq!(longer.gas.history - zeros.gas.history, 100 * CPHB, "a hundred bytes more");
}

/* ---------- the sites the Host stages ---------- */

/// The body of a `LOG` with `topics` topics over `data_len` bytes of memory, without the `STOP`
/// that ends a program.
fn log_body(topics: u8, data_len: u64) -> BytecodeBuilder {
    let mut code = BytecodeBuilder::default();
    for topic in 0..topics {
        code = code.push_number(u64::from(topic) + 1);
    }
    code.push_number(data_len).push_number(0u64).append(LOG0 + topics)
}

/// A program that emits one such log and stops.
fn logging(topics: u8, data_len: u64) -> Bytes {
    log_body(topics, data_len).stop().build()
}

/// A log appends its own record for the address that emitted it, one record per topic and its
/// data, and pays history for every one of those bytes.
#[test]
fn test_a_log_pays_for_its_address_its_topics_and_its_data() {
    if runs_at_measurement_prices() {
        return;
    }
    let run = |code: Bytes| {
        let db = funded().account_code(CALLEE, code);
        execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
    };
    let bare = run(logging(0, 0));
    let one_topic = run(logging(1, 32));
    let three_topics = run(logging(3, 32));

    assert!(bare.result.is_success(), "{:?}", bare.result);
    assert_eq!(bare.gas.history, body(0) + LOG_BASE_SIZE * CPHB, "the address alone");
    assert_eq!(
        one_topic.gas.history,
        body(0) + (LOG_BASE_SIZE + LOG_TOPIC_SIZE + 32) * CPHB,
        "the address, one topic and 32 bytes of data",
    );
    assert_eq!(
        three_topics.gas.history,
        body(0) + 160 * CPHB,
        "a three-topic event carrying one word is 160 bytes",
    );
    assert_eq!(
        three_topics.gas.history - one_topic.gas.history,
        2 * LOG_TOPIC_SIZE * CPHB,
        "two topics more",
    );
}

/// A log costs history on top of what the EVM itself charges for the opcode: the schedule's own
/// price is untouched, and the history is the bytes.
#[test]
fn test_a_log_pays_history_on_top_of_the_schedules_price() {
    if runs_at_measurement_prices() {
        return;
    }
    let run = |code: Bytes| {
        let db = funded().account_code(CALLEE, code);
        execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
    };
    let quiet = run(BytecodeBuilder::default().stop().build());
    let logging = run(logging(1, 32));

    // The EVM's own price for the opcode: two pushes for the offset and length, one for the
    // topic, the `LOG` base, the per-topic and per-byte rates, and one word of memory.
    let params = mega_evm::satin_gas_params();
    let native =
        3 * 3 + 375 + params.get(GasId::logtopic()) + 32 * params.get(GasId::logdata()) + 3;
    assert_eq!(
        logging.gas.regular - quiet.gas.regular,
        native,
        "the schedule charges what it always did",
    );
    assert_eq!(
        logging.gas.history - quiet.gas.history,
        (LOG_BASE_SIZE + LOG_TOPIC_SIZE + 32) * CPHB
    );
}

/// A storage write leaves one write record, and the record is history: the first change of a slot
/// in the transaction pays for forty bytes, and writing the slot back to its original value takes
/// that charge back.
#[test]
fn test_a_storage_write_pays_for_its_record_and_a_write_back_takes_it_back() {
    if runs_at_measurement_prices() {
        return;
    }
    let run = |code: BytecodeBuilder| {
        let db = funded().account_code(CALLEE, code.stop().build());
        execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
    };
    let one = run(BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)));
    let three = run(BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(1))
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(1)));
    let rewritten = run(BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(1))
        .sstore(U256::ZERO, U256::from(2)));
    let restored = run(BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(1))
        .sstore(U256::ZERO, U256::ZERO));

    assert_eq!(one.gas.history, body(0) + WRITE_RECORD_SIZE * CPHB, "one record");
    assert_eq!(three.gas.history, body(0) + 3 * WRITE_RECORD_SIZE * CPHB, "three slots, three");
    assert_eq!(rewritten.gas.history, one.gas.history, "a slot already changed records once");
    assert_eq!(restored.gas.history, body(0), "the write-back took the record's charge back");
}

/// A frame that fails pays for nothing it appended: its logs and its write records go back with
/// it, and its caller keeps only what it did itself.
#[test]
fn test_a_failing_frame_gives_its_history_back() {
    if runs_at_measurement_prices() {
        return;
    }
    // `CALLEE` calls `CONTRACT` and carries on whatever it answers.
    let caller_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let run = |callee: Bytes| {
        let db = funded().account_code(CALLEE, caller_code.clone()).account_code(CONTRACT, callee);
        execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
    };
    let quiet = run(BytecodeBuilder::default().stop().build());
    let kept = run(logging(1, 32));
    let reverted = run(log_body(1, 32).append_many([PUSH0, PUSH0, REVERT]).build());

    assert!(kept.result.is_success() && reverted.result.is_success(), "the caller survives");
    assert_eq!(kept.gas.history - quiet.gas.history, (LOG_BASE_SIZE + LOG_TOPIC_SIZE + 32) * CPHB);
    assert_eq!(reverted.gas.history, quiet.gas.history, "the reverted frame's log is not paid for");
}
