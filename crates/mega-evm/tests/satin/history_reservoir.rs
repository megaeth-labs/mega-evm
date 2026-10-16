//! The ledgers above the EIP-8037 execution cap, where the reservoir is what pays.
//!
//! A transaction whose gas limit exceeds the 200,000,000 execution cap carries the excess as an
//! EIP-8037 reservoir, and every state and history charge draws on it before it touches regular
//! gas. Below the cap the reservoir is empty, every charge spills straight onto regular gas, and
//! a charge that is silently given back looks exactly like a charge that was made and kept.
//!
//! So the rule this lane carries, and the next mechanism inherits: **every ledger invariant gets
//! at least one case above the cap**. Each holds its transaction to
//!
//! ```text
//! reservoir_remaining == gas_limit - TX_GAS_LIMIT_CAP - state - history
//! ```
//!
//! and to the same regular ledger as the same program run below the cap. A charge undone behind
//! the ledger's back is reservoir the transaction never paid for, and this identity is the only
//! place it shows.
//!
//! The four cases here are the ones the frame-start charge needs, because it is the one site that
//! charges history from outside the frame the charge belongs to: a value call, a nested creation,
//! a child that reverts, and a call an interceptor answers without a frame.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::{ACCOUNT_STATE_GAS, COST_PER_HISTORY_BYTE, TX_GAS_LIMIT_CAP},
    system::{IMegaAccessControl, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    LimitUsage, MegaTransactionOutcome, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::bytecode::opcode::{CALL, CREATE, GAS, POP, PUSH0, PUSH1, REVERT, STOP};

use crate::common::{call, execute, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000700000");
const CALLEE: Address = address!("0000000000000000000000000000000000700001");
const CONTRACT: Address = address!("0000000000000000000000000000000000700002");

/// The gas limit above the execution cap, and the reservoir it leaves.
const RESERVOIR: u64 = 100_000_000;
const ABOVE_CAP: u64 = TX_GAS_LIMIT_CAP + RESERVOIR;

/// A gas limit below the execution cap, where the reservoir is empty and every charge spills onto
/// regular gas.
const BELOW_CAP: u64 = 50_000_000;

const CPHB: u64 = COST_PER_HISTORY_BYTE;

/// The history a transaction's body costs.
const BODY: u64 = TX_BODY_SIZE * CPHB;

/// The history one write record costs.
const RECORD: u64 = WRITE_RECORD_SIZE * CPHB;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(10_000_000))
}

/// `CALL(GAS, target, 1 wei, [], [])`, discarding the flag, then `STOP`.
fn transfers_to(target: Address) -> Bytes {
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
fn creates() -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0])
        .append(PUSH1)
        .append(1u8)
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build()
}

/// A one-wei call to `MegaAccessControl`, which its interceptor refuses before any frame runs.
fn calls_a_refusing_interceptor() -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR)
        .append_many([PUSH0, PUSH0])
        .push_number(4u64)
        .append(PUSH0)
        .append(PUSH1)
        .append(1u8)
        .push_address(ACCESS_CONTROL_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// Runs the same transaction above and below the execution cap and holds the two to the same
/// ledgers: above the cap the reservoir pays what regular gas pays below it, and the reservoir
/// left over is the gas limit above the cap less the state and history the ledgers report.
///
/// Returns the above-the-cap outcome, for a case that has more to say about it.
#[track_caller]
fn assert_the_reservoir_pays_what_regular_gas_pays(
    name: &str,
    db: impl Fn() -> MemoryDatabase,
) -> MegaTransactionOutcome {
    let above = execute(db(), call(CALLER, CALLEE, U256::ZERO, ABOVE_CAP));
    let below = execute(db(), call(CALLER, CALLEE, U256::ZERO, BELOW_CAP));

    assert_eq!(above.result.is_success(), below.result.is_success(), "{name}: success");
    assert_eq!(above.usage, below.usage, "{name}: what the transaction kept");
    assert_eq!(above.gas.history, below.gas.history, "{name}: the history ledger");
    assert_eq!(above.gas.state, below.gas.state, "{name}: the state ledger");
    assert_eq!(above.gas.regular, below.gas.regular, "{name}: the regular ledger");
    assert_eq!(below.gas.reservoir_remaining, 0, "{name}: below the cap there is no reservoir");
    assert_eq!(
        above.gas.reservoir_remaining,
        ABOVE_CAP - TX_GAS_LIMIT_CAP - above.gas.state - above.gas.history,
        "{name}: the reservoir paid exactly the state and the history the ledgers report",
    );
    above
}

/// A value `CALL` charges its caller for two write records. Above the cap the reservoir pays
/// them, and the frame it starts inherits the reservoir the charge left — not the one the caller
/// held before it, which the frame would hand back on return.
#[test]
fn test_a_value_call_pays_its_records_out_of_the_reservoir() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = transfers_to(CONTRACT);
    let outcome = assert_the_reservoir_pays_what_regular_gas_pays("a value call", || {
        funded().account_code(CALLEE, code.clone())
    });

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 2, "the caller's account and the recipient's");
    assert_eq!(outcome.gas.history, BODY + 2 * RECORD);
    assert_eq!(outcome.gas.state, ACCOUNT_STATE_GAS, "the recipient is a new account");
}

/// A nested creation charges its creator for the created account and for its own nonce, and the
/// creation frame inherits the reservoir that charge left.
#[test]
fn test_a_nested_creation_pays_its_records_out_of_the_reservoir() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = creates();
    let outcome = assert_the_reservoir_pays_what_regular_gas_pays("a nested creation", || {
        funded().account_code(CALLEE, code.clone())
    });

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 2, "the created account and the creator's nonce");
    assert_eq!(outcome.gas.history, BODY + 2 * RECORD);
}

/// A child that reverts keeps none of the records its caller paid for, so the charge goes back to
/// the reservoir it came from — and goes back once. The net history is the body alone.
#[test]
fn test_a_reverting_child_gives_its_records_back_to_the_reservoir() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = transfers_to(CONTRACT);
    let outcome =
        assert_the_reservoir_pays_what_regular_gas_pays("a value call whose child reverts", || {
            funded()
                .account_code(CALLEE, code.clone())
                .account_code(CONTRACT, Bytes::from_static(&[PUSH0, PUSH0, REVERT]))
        });

    assert!(outcome.result.is_success(), "the caller survives its child's revert");
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
        "the reverted transfer kept nothing but the body"
    );
    assert_eq!(outcome.gas.history, BODY, "the body alone");
    assert_eq!(
        outcome.gas.reservoir_remaining,
        RESERVOIR - BODY,
        "the transfer's records cost the reservoir nothing in the end",
    );
}

/// A value call a system contract's interceptor refuses never starts a frame: its lane is empty
/// and the whole charge comes back. It is the path where the caller is paid back twice if the
/// frame inherited the caller's reservoir from before the charge, because the empty lane refills
/// what the adoption already restored.
#[test]
fn test_an_intercepted_value_call_gives_its_records_back_once() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = calls_a_refusing_interceptor();
    let outcome = assert_the_reservoir_pays_what_regular_gas_pays(
        "a value call an interceptor refuses",
        || {
            funded()
                .account_code(CALLEE, code.clone())
                .account_balance(ACCESS_CONTROL_ADDRESS, U256::from(1))
                .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
        },
    );

    assert!(outcome.result.is_success(), "the caller survives the refusal");
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
        "the refused call wrote nothing but the body"
    );
    assert_eq!(outcome.gas.history, BODY, "the body alone");
    assert_eq!(
        outcome.gas.reservoir_remaining,
        RESERVOIR - BODY,
        "the refused call's records cost the reservoir nothing in the end",
    );
}
