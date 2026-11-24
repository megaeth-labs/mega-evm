//! A user's deposit, and a deposit that has already failed.
//!
//! The created caller of a user deposit is state growth before the first frame. A failed deposit
//! reports `FailedDeposit` and is not stopped.

use alloy_primitives::{address, Address, U256};
use mega_evm::{MegaHaltReason, MegaTransaction};
use revm::context::result::ExecutionResult;

use super::{
    block, execute, nonce_of, reverted_stop, slot_value, slots_gas, store_n_slots, world, CALLER,
    CONTRACT, TX_STATE_GROWTH_LIMIT,
};
use crate::common;

const FRESH: Address = address!("0000000000000000000000000000000000ab100a");
/// A second contract, so a deposit's slots are not the slots an earlier transaction filled.
const OTHER_CONTRACT: Address = address!("0000000000000000000000000000000000ab100c");

fn user_deposit(caller: Address, gas_limit: u64, system: bool) -> MegaTransaction {
    let mut tx = common::deposit(common::call(caller, CONTRACT, U256::ZERO, gas_limit));
    tx.0.deposit.is_system_transaction = system;
    tx
}

/// A user deposit whose sender had no account, then fresh slots. The created caller is +1 before
/// the first frame. The first frame's budget is 999. The 1,000th slot stops that frame, kind 3,
/// `limit` 999. The caller account stays.
#[test]
fn test_a_new_depositors_1000th_slot_stops_at_999() {
    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT) else { return };
    let db = world(&[(CONTRACT, store_n_slots(TX_STATE_GROWTH_LIMIT as u16))]);
    let outcome = execute(db, user_deposit(FRESH, gas, false));
    assert_eq!(
        reverted_stop(&outcome),
        mega_evm::MegaLimitExceeded { kind: 3, limit: TX_STATE_GROWTH_LIMIT - 1 }
    );
    assert!(outcome.limit_exceeded.is_some(), "the deposit's own frame stopped");
    assert_eq!(slot_value(&outcome, CONTRACT, 1), U256::ZERO, "the stopped frame keeps no slot");
    assert!(nonce_of(&outcome, FRESH) >= 1, "the created caller stays");
    assert_eq!(
        outcome.gas.state,
        common::account_state_gas(),
        "the created caller keeps its state gas; the stopped slots keep none"
    );
}

/// A user deposit whose sender already had an account. No pre-frame growth. The 1,001st fresh
/// slot stops the first frame, kind 3, `limit` 1,000.
#[test]
fn test_an_existing_depositors_1001st_slot_stops_at_1000() {
    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT + 1) else { return };
    let db = world(&[(CONTRACT, store_n_slots((TX_STATE_GROWTH_LIMIT + 1) as u16))])
        .account_balance(FRESH, U256::from(1));
    let outcome = execute(db, user_deposit(FRESH, gas, false));
    assert_eq!(
        reverted_stop(&outcome),
        mega_evm::MegaLimitExceeded { kind: 3, limit: TX_STATE_GROWTH_LIMIT }
    );
    assert_eq!(slot_value(&outcome, CONTRACT, 1), U256::ZERO);
}

/// The block is already at 1,000 state growth: the ordinary transaction after the deposit is
/// skipped. The deposit, a user's, is not skipped. It runs under the per-transaction limit: the
/// 1,001 slots it tries are stopped, and none is kept.
#[test]
fn test_a_user_deposit_after_1000_slots_is_not_skipped() {
    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT + 1) else { return };
    let mut db = block::database(store_n_slots(TX_STATE_GROWTH_LIMIT as u16));
    db.set_account_code(OTHER_CONTRACT, store_n_slots((TX_STATE_GROWTH_LIMIT + 1) as u16));
    let (results, mut state) = block::drive(
        db,
        vec![
            block::legacy(CALLER, 0, CONTRACT, gas),
            block::deposit(FRESH, OTHER_CONTRACT, gas, false),
            block::legacy(CALLER, 1, CONTRACT, gas),
        ],
    );
    assert!(results[0].is_ok(), "{:?}", results[0]);
    assert!(results[1].is_ok(), "a user deposit is not skipped: {:?}", results[1]);
    assert_eq!(
        block::slot(&mut state, OTHER_CONTRACT, 1),
        U256::ZERO,
        "the deposit is held to the per-transaction limit"
    );
    assert!(results[2].is_err(), "the block is at its limit: {:?}", results[2]);
}

/// A failed deposit whose sender had no account: no state gas for that account, and the limit
/// does not stop it. Its account counts one toward the block's state growth: after an ordinary
/// transaction of 999 slots, one such deposit takes the block to 1,000, so the next ordinary
/// transaction is skipped, and a second such deposit after it is not, taking the counter past
/// 1,000.
#[test]
fn test_a_failed_deposit_from_a_new_sender_is_not_stopped() {
    let db = world(&[(CONTRACT, store_n_slots(1))]);
    let outcome = execute(db, user_deposit(FRESH, 100_000, true));
    assert!(
        matches!(
            outcome.result,
            ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
        ),
        "{:?}",
        outcome.result
    );
    assert_eq!(outcome.gas.state, 0, "a failed deposit charges no state gas");
    assert!(outcome.limit_exceeded.is_none(), "the halt is not a limit stop");
    assert_eq!(nonce_of(&outcome, FRESH), 1, "the sender account is created");

    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT) else { return };
    let db = block::database(store_n_slots((TX_STATE_GROWTH_LIMIT - 1) as u16));
    let (results, _) = block::drive(
        db,
        vec![
            block::legacy(CALLER, 0, CONTRACT, gas),
            block::deposit(FRESH, CONTRACT, 100_000, true),
            block::legacy(CALLER, 1, CONTRACT, gas),
            block::deposit(
                address!("0000000000000000000000000000000000ab100b"),
                CONTRACT,
                100_000,
                true,
            ),
        ],
    );
    assert!(results[0].is_ok(), "{:?}", results[0]);
    assert!(results[1].is_ok(), "a failed deposit is not skipped: {:?}", results[1]);
    assert!(results[2].is_err(), "the failed deposit's account counted: {:?}", results[2]);
    assert!(results[3].is_ok(), "the counter may pass 1,000: {:?}", results[3]);
}
