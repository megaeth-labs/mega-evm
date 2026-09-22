//! History gas: what a Satin transaction pays for the bytes it appends to the chain's history.
//!
//! Every charge is a byte count at the cost per history byte, and every assertion here writes the
//! count out, so a repricing moves the numbers without rewriting the test.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    LOG_BASE_SIZE, LOG_TOPIC_SIZE, TX_BASE_SIZE, TX_BODY_SIZE, TX_FIXED_WRITE_RECORDS,
    WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, GAS, LOG0, POP, PUSH0, PUSH1, RETURN, REVERT, STOP},
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

/* ---------- a frame-start charge the caller cannot pay ---------- */

/// `CALL(GAS, target, 1 wei, [], [])`, discarding the flag, then `STOP`: a value transfer that
/// forwards everything the caller may forward, so the caller keeps a sixty-fourth of what it held.
fn transfers_everything_to(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(PUSH1)
        .append(1u8)
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// `CREATE` of empty init code carrying one wei, discarding the address, then `STOP`.
fn creates_everything() -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0])
        .append(PUSH1)
        .append(1u8)
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build()
}

/// A funded caller holding `code`, with a balance to transfer.
fn caller_running(code: Bytes) -> MemoryDatabase {
    funded().account_balance(CALLEE, U256::from(10_000_000)).account_code(CALLEE, code)
}

/// The two records a value call's start makes cost more than a caller under roughly 450,000 gas
/// keeps after the sixty-fourth it may not forward. Such a caller pays for none of them by
/// halting: the frame it was starting does not run.
#[test]
fn test_a_call_whose_caller_cannot_pay_its_records_starts_no_frame() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = transfers_everything_to(CONTRACT);
    let short = execute(caller_running(code.clone()), call(CALLER, CALLEE, U256::ZERO, 600_000));
    let ample = execute(caller_running(code), call(CALLER, CALLEE, U256::ZERO, 1_000_000));

    assert!(!short.result.is_success(), "the caller halts rather than starting the frame");
    assert_eq!(short.usage.write_records, 0, "the frame that never ran wrote nothing");
    assert_eq!(short.gas.history, body(0), "and the transaction pays for its body alone");

    assert!(ample.result.is_success(), "{:?}", ample.result);
    assert_eq!(ample.usage.write_records, 2, "the caller's account and the recipient's");
    assert_eq!(ample.gas.history, body(0) + 2 * WRITE_RECORD_SIZE * CPHB);
}

/// The same for a creation, whose start records the created account and the creator's nonce.
#[test]
fn test_a_creation_whose_creator_cannot_pay_its_records_starts_no_frame() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = creates_everything();
    let short = execute(caller_running(code.clone()), call(CALLER, CALLEE, U256::ZERO, 600_000));
    let ample = execute(caller_running(code), call(CALLER, CALLEE, U256::ZERO, 1_000_000));

    assert!(!short.result.is_success(), "the creator halts rather than starting the creation");
    assert_eq!(short.usage.write_records, 0, "the creation that never ran wrote nothing");
    assert_eq!(short.gas.history, body(0), "and the transaction pays for its body alone");

    assert!(ample.result.is_success(), "{:?}", ample.result);
    assert_eq!(ample.usage.write_records, 2, "the created account and the creator's nonce");
    assert_eq!(ample.gas.history, body(0) + 2 * WRITE_RECORD_SIZE * CPHB);
}

/// Whether a write record is paid for may not depend on how much gas its frame's caller had
/// left. Across the gas limits that straddle the point where a caller can no longer pay for the
/// records of the frame it starts, the history a transaction pays beyond its body is exactly the
/// records it kept.
#[test]
fn test_no_gas_limit_buys_a_write_record_for_nothing() {
    if runs_at_measurement_prices() {
        return;
    }
    for code in [transfers_everything_to(CONTRACT), creates_everything()] {
        for limit in [300_000, 400_000, 600_000, 800_000, 1_000_000, 5_000_000] {
            let outcome =
                execute(caller_running(code.clone()), call(CALLER, CALLEE, U256::ZERO, limit));
            assert_eq!(
                outcome.gas.history,
                body(0) + outcome.usage.write_records * WRITE_RECORD_SIZE * CPHB,
                "at a {limit} gas limit: the records kept are the history paid",
            );
        }
    }
}

/* ---------- the writes every transaction makes ---------- */

/// The transaction body carries the writes every transaction makes whatever it runs: the sender's
/// account, and the four accounts its fees are credited to.
///
/// They are in the body rather than charged where they happen because the body is priced before
/// the transaction runs, when how many of the four are actually credited is not yet known — a
/// zero fee credits none, and a beneficiary that is one of the vaults is one account, not two.
/// The body carries the bound; nothing counts them again afterwards.
#[test]
fn test_the_body_carries_the_writes_every_transaction_makes() {
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(TX_BODY_SIZE, TX_BASE_SIZE + TX_FIXED_WRITE_RECORDS * WRITE_RECORD_SIZE);
    assert_eq!(TX_BODY_SIZE, 110 + 5 * 40);

    // A transaction that actually pays a fee credits the beneficiary and the fee vaults, and
    // records none of them: what it pays is what a fee-free transaction pays.
    let db = || funded().account_code(CALLEE, Bytes::new());
    let free = execute(db(), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    let paid = execute(db(), {
        let mut tx = call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT);
        tx.0.base.gas_price = 10;
        tx
    });

    assert!(free.result.is_success() && paid.result.is_success());
    assert_eq!(free.gas.history, body(0), "the body, and the body alone");
    assert_eq!(paid.gas.history, free.gas.history, "the fee recipients are already in the body");
    assert_eq!(paid.usage.write_records, 0, "and none of them is recorded again");
}

/// A deposit credits no fee recipient at all, and is exempt from history besides: it pays for
/// neither the body nor anything the body carries.
#[test]
fn test_a_deposit_pays_for_none_of_the_bodys_writes() {
    if runs_at_measurement_prices() {
        return;
    }
    let mut tx = call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT);
    tx.0.deposit.source_hash = alloy_primitives::B256::repeat_byte(0x11);
    let outcome = execute(funded().account_code(CALLEE, Bytes::new()), tx);

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.history, 0);
    assert_eq!(outcome.usage.write_records, 0);
}

/// The body carries a bound on the fee recipients, not a count of them, so a beneficiary that is
/// one of the fee vaults is not something anything has to notice: the two transactions pay the
/// same history, and neither reads a recipient to find out.
#[test]
fn test_a_beneficiary_that_is_a_fee_vault_changes_nothing() {
    if runs_at_measurement_prices() {
        return;
    }
    use op_revm::constants::BASE_FEE_RECIPIENT;
    let db = || funded().account_code(CALLEE, Bytes::new());
    let paying = |beneficiary| {
        let mut tx = call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT);
        tx.0.base.gas_price = 10;
        let ctx = crate::common::context(db())
            .with_block(revm::context::BlockEnv { beneficiary, ..crate::common::block() });
        mega_evm::MegaEvm::new(ctx).execute_transaction(tx).expect("the transaction is valid")
    };
    let distinct = paying(address!("00000000000000000000000000000000000c0ffe"));
    let a_vault = paying(BASE_FEE_RECIPIENT);

    assert!(distinct.result.is_success() && a_vault.result.is_success());
    assert_eq!(distinct.gas.history, body(0));
    assert_eq!(a_vault.gas.history, distinct.gas.history);
    assert_eq!(a_vault.usage.write_records, 0);
}
