//! History gas: what a Satin transaction pays for the bytes it appends to the chain's history.
//!
//! Every charge is a byte count at the cost per history byte, and every assertion here writes the
//! count out, so a repricing moves the numbers without rewriting the test.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE},
    test_utils::{BytecodeBuilder, MemoryDatabase},
};
use revm::bytecode::opcode::{PUSH0, RETURN, REVERT};

use crate::common::{create, execute, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000200000");

/// Room for the state gas of a new account and the code it deposits.
const GAS_LIMIT: u64 = 50_000_000;

/// The cost per history byte, spelled as the constant the tests price against.
const CPHB: u64 = COST_PER_HISTORY_BYTE;

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
    assert_eq!(deployed.gas.history, 32 * CPHB, "the deployed bytes");
    assert_eq!(reverted.gas.history, 0, "nothing was deployed");
}
