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
use revm::bytecode::opcode::{CALL, CREATE, GAS, MSTORE, POP, PUSH0, PUSH1, RETURN, REVERT, STOP};

use crate::common::{body_history, call, execute, history_is_free, runs_at_measurement_prices};

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

/* ---------- the body, drawn from the reservoir first ---------- */

/// `GAS; PUSH0; MSTORE; PUSH1 32; PUSH0; RETURN`: answers with the regular gas the frame has
/// left after the `GAS` itself, and charges nothing else — no record, no log, no state.
fn answers_its_gas() -> Bytes {
    BytecodeBuilder::default()
        .append(GAS)
        .append(PUSH0)
        .append(MSTORE)
        .push_number(32_u8)
        .append(PUSH0)
        .append(RETURN)
        .build()
}

/// What [`answers_its_gas`] spends: `GAS` 2, `PUSH0` 2, `MSTORE` 3 and the first word of memory
/// 3, `PUSH1` 3, `PUSH0` 2, `RETURN` 0.
const ANSWERING: u64 = 2 + 2 + 3 + 3 + 3 + 2;

/// The intrinsic regular gas of a call carrying no data: `TX_BASE_COST` 12,000 and 3,000 for the
/// recipient's access.
const INTRINSIC: u64 = 12_000 + 3_000;

/// The body's history gas is taken from the reservoir first, and only its excess from the regular
/// budget [S4.4]; a reservoir short of the body is spent to the last gas, one of exactly the body
/// too, and one a gas larger keeps that gas [S4.7]. A body-only transaction shows it: the first
/// frame's `GAS` is the regular budget — the execution cap less the intrinsic gas, less the
/// excess the reservoir did not cover, less the `GAS` itself — and not the reservoir [S4.8].
///
/// Every figure is by hand (`constants`: the execution cap and the body's history at the price in
/// effect, which is 310 × 88 at the spec's price): the regular ledger is the intrinsic gas and
/// the program, whichever pool paid the body, because a charge that spilled stays on its own
/// ledger; the receipt adds the body.
#[test]
fn test_the_body_draws_the_reservoir_first_and_only_its_excess_from_regular_gas() {
    // A body that costs nothing leaves the reservoir nothing to draw.
    if history_is_free() {
        return;
    }
    let body = body_history(0);
    let db = || funded().account_code(CALLEE, answers_its_gas());

    for reservoir in [1, body - 1, body, body + 1] {
        let name = format!("a reservoir of {reservoir} against a body of {body}");
        let excess = body.saturating_sub(reservoir);
        let outcome = execute(db(), call(CALLER, CALLEE, U256::ZERO, TX_GAS_LIMIT_CAP + reservoir));
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);

        let gas_seen = U256::from_be_slice(outcome.result.output().expect("the answer"));
        assert_eq!(
            gas_seen,
            U256::from(TX_GAS_LIMIT_CAP - INTRINSIC - excess - 2),
            "{name}: GAS is the regular budget less the excess, not the reservoir",
        );
        assert_eq!(
            outcome.gas.reservoir_remaining,
            reservoir.saturating_sub(body),
            "{name}: the reservoir paid the body as far as it went",
        );
        assert_eq!(outcome.gas.history, body, "{name}: the history ledger is the body");
        assert_eq!(outcome.gas.state, 0, "{name}: no state");
        assert_eq!(
            outcome.gas.regular,
            INTRINSIC + ANSWERING,
            "{name}: the regular ledger is the intrinsic gas and the program, whoever paid the body",
        );
        assert_eq!(outcome.gas.gas_used, INTRINSIC + ANSWERING + body, "{name}: the receipt");
        assert_eq!(outcome.usage.write_records, 0, "{name}: body only");
    }

    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(body, 310 * 88, "the body at the spec's price");
    assert_eq!(INTRINSIC + ANSWERING + body, 42_295, "the empty call's 42,280 and the program");
}
