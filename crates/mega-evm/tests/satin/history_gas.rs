//! History gas: what a Satin transaction pays for the bytes it appends to the chain's history.
//!
//! Every charge is a byte count at the cost per history byte, and every assertion here writes the
//! count out, so a repricing moves the numbers without rewriting the test.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    TX_BODY_SIZE,
};
use revm::bytecode::opcode::{PUSH0, RETURN, REVERT};

use crate::common::{call, call_with_data, create, execute, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const CALLEE: Address = address!("0000000000000000000000000000000000200001");

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
        body(deploying(32).len() as u64) + 32 * CPHB,
        "the body the init code travels in, and the deployed bytes",
    );
    assert_eq!(
        reverted.gas.history,
        body(reverting().len() as u64),
        "the body alone: nothing was deployed",
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
