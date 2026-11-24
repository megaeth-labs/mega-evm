//! The intrinsic phase: the EIP-7976 calldata floor, the history gas of the transaction's body, and
//! what the execution cap admits.
//!
//! Satin keeps Amsterdam's floor: a base of 12,000 with the recipient and value charges on top,
//! and every calldata byte at 64 gas whether it is zero or not. A transaction whose floor is above
//! the 200M execution cap is rejected before it runs, which is what bounds the calldata a
//! transaction may carry; one whose floor or intrinsic charge is above its own gas limit is
//! rejected too, and leaves its sender untouched.
//!
//! The floor is still computed and still validated, and it no longer decides what anyone pays:
//! history gas charges the same bytes at a higher rate, so a Satin transaction is above its own
//! floor. The tests below read the floor off the result rather than off the bill.

use alloy_evm::{Evm, EvmError, InvalidTxError};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP, test_utils::MemoryDatabase, MegaEvm, MegaTransaction, TX_BODY_SIZE,
};
use op_revm::OpTransactionError;
use revm::{
    context::result::{EVMError, InvalidTransaction},
    Database, ExecuteCommitEvm, ExecuteEvm,
};

use crate::common::{call, context, history_is_free, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000800000");
const CALLEE: Address = address!("0000000000000000000000000000000000800001");

/// The intrinsic gas of a plain call to an account that is not the sender: the EIP-2780 sender
/// base of 12,000 plus 3,000 for reaching the recipient.
///
/// The 3,000 is EIP-2780's own fixed charge, EIP-8038's cold account access, not the schedule's
/// cold-account entry: inside execution the schedule prices the same access as 100 for the read
/// and 2,900 for the cold surcharge, so repricing that entry would leave this number where it is.
const EMPTY_CALL: u64 = 15_000;

/// What one byte of anything a transaction carries costs in the EIP-7976 / EIP-7981 floor: four
/// floor tokens at 16 gas each.
const FLOOR_PER_BYTE: u64 = 64;

/// The history gas a transaction carrying `bytes` bytes beside its envelope pays for its body, at
/// the price the engine runs.
fn body_history(bytes: u64) -> u64 {
    mega_evm::history_gas(TX_BODY_SIZE + bytes).expect("a body has a price")
}

/// A database where the sender can pay and `CALLEE` exists.
fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(1))
}

/// Runs `tx` and returns the gas it used, or the validation error that rejected it.
fn gas_used(db: MemoryDatabase, tx: MegaTransaction) -> Result<u64, String> {
    charged(db, tx).map(|charged| charged.gas_used)
}

/// What a transaction was charged: the receipt's figure and the floor it was measured against.
struct Charged {
    gas_used: u64,
    floor: u64,
}

/// Runs `tx` and reports what it was charged, or the validation error that rejected it.
fn charged(db: MemoryDatabase, tx: MegaTransaction) -> Result<Charged, String> {
    let mut evm = MegaEvm::new(context(db));
    match evm.transact_raw(tx) {
        Ok(outcome) => {
            assert!(outcome.result.is_success(), "{:?}", outcome.result);
            let gas = outcome.result.gas();
            Ok(Charged { gas_used: gas.tx_gas_used(), floor: gas.floor_gas() })
        }
        Err(err) => {
            let invalid = err
                .as_invalid_tx_err()
                .and_then(InvalidTxError::as_invalid_tx_err)
                .unwrap_or_else(|| panic!("unexpected error {err:?}"));
            Err(format!("{invalid:?}"))
        }
    }
}

/* ---------- the execution cap ---------- */

/// The largest calldata the execution cap admits: its floor is the cap's own number rounded down
/// to a whole byte.
const LARGEST_CALLDATA: u64 = 3_124_765;

/// A call carrying `len` calldata bytes with a gas limit that covers everything it has to pay,
/// above the execution cap, so the cap is what the floor is measured against.
///
/// The body's history is not held to the cap — it comes out of the reservoir — so the limit has
/// to be above the cap by more than the body costs.
fn with_calldata_over_the_cap(len: u64) -> MegaTransaction {
    crate::common::call_with_data(
        CALLER,
        CALLEE,
        Bytes::from(vec![0u8; len as usize]),
        TX_GAS_LIMIT_CAP + body_history(len) + 1_000_000,
    )
}

/// At the bound the transaction is admitted; one byte more and the floor is above the execution
/// cap, which rejects it before it runs.
///
/// The bound is the floor's, not the bill's: history gas is charged on top of it and neither the
/// floor nor the cap counts it, so what the transaction pays is far above the cap.
#[test]
fn test_the_execution_cap_bounds_the_calldata_a_transaction_may_carry() {
    let floor = EMPTY_CALL + FLOOR_PER_BYTE * LARGEST_CALLDATA;
    assert!(floor <= TX_GAS_LIMIT_CAP, "{floor} is within the cap");
    assert!(
        floor + FLOOR_PER_BYTE > TX_GAS_LIMIT_CAP,
        "one byte more is over it, so the bound is the largest"
    );

    let db = funded().account_balance(CALLER, U256::from(10u128.pow(24)));
    let charged = charged(db.clone(), with_calldata_over_the_cap(LARGEST_CALLDATA)).expect("valid");
    assert_eq!(charged.floor, floor);
    // The floor binds where a history byte is cheaper than the floor's rate less the intrinsic
    // one: the bill is the larger of the two.
    assert_eq!(
        charged.gas_used,
        (EMPTY_CALL + 4 * LARGEST_CALLDATA + body_history(LARGEST_CALLDATA)).max(floor)
    );

    let expected = format!(
        "{:?}",
        InvalidTransaction::GasFloorMoreThanGasLimit {
            gas_floor: floor + FLOOR_PER_BYTE,
            gas_limit: TX_GAS_LIMIT_CAP,
        }
    );
    assert_eq!(gas_used(db, with_calldata_over_the_cap(LARGEST_CALLDATA + 1)), Err(expected));
}

/// A floor above the transaction's own gas limit is rejected too, below the cap: the floor is
/// measured against whichever of the two is smaller. The rejection leaves the sender as it was.
///
/// Independence: independent — the base, the floor's rate and the
/// sender's funding are written out.
#[test]
fn test_a_floor_above_the_gas_limit_is_rejected() {
    let gas_limit = 100_000;
    let len = (gas_limit - EMPTY_CALL) / FLOOR_PER_BYTE + 1;
    let tx = crate::common::call_with_data(
        CALLER,
        CALLEE,
        Bytes::from(vec![0u8; len as usize]),
        gas_limit,
    );
    assert_rejected_and_sender_untouched(
        tx,
        InvalidTransaction::GasFloorMoreThanGasLimit {
            gas_floor: EMPTY_CALL + FLOOR_PER_BYTE * len,
            gas_limit,
        },
    );
}

/// Validation rejects `tx` with `expected` on an EVM over [`funded`], through alloy-evm and through
/// revm's own entry point, and the sender is untouched in the database the second EVM ran on.
///
/// That EVM runs `tx` as a block builder runs one transaction after another, without finalizing
/// it, and then commits what its journal holds: whatever the rejection left behind for the next
/// transaction reaches the database, where [`assert_sender_untouched`] reads the sender.
fn assert_rejected_and_sender_untouched(tx: MegaTransaction, expected: InvalidTransaction) {
    assert_eq!(gas_used(funded(), tx.clone()), Err(format!("{expected:?}")), "through alloy-evm");
    let mut evm = MegaEvm::new(context(funded()));
    match ExecuteEvm::transact_one(&mut evm, tx) {
        Err(EVMError::Transaction(invalid)) => assert_eq!(
            format!("{invalid:?}"),
            format!("{:?}", OpTransactionError::Base(expected)),
            "the rejection"
        ),
        other => panic!("expected the rejection {expected:?}, got {other:?}"),
    }
    ExecuteCommitEvm::commit_inner(&mut evm);
    assert_sender_untouched(evm.db_mut());
}

/// The sender's nonce and balance in `db`, the database a rejected transaction ran on, are what
/// [`funded`] gave it.
fn assert_sender_untouched(db: &mut MemoryDatabase) {
    let info = db.basic(CALLER).expect("the database reads").expect("the sender exists");
    assert_eq!(info.nonce, 0, "the sender's nonce");
    assert_eq!(info.balance, U256::from(10u64.pow(18)), "the sender's balance");
}

/* ---------- the gas limit against the intrinsic charge and the body ---------- */

/// A gas limit below the intrinsic charge itself is a validation rejection, which leaves the
/// sender untouched.
///
/// The figure the error carries is one reading of a point the spec leaves open. The spec fixes
/// the rejection and its error, `CallGasCostMoreThanGasLimit`, for a gas limit below the intrinsic
/// regular gas plus the body's history, but not which figure the error names. Here, below the
/// intrinsic regular gas alone, it names that charge without the body's history; a gas limit that
/// covers the intrinsic charge but not the body is named with the whole minimum (the next test).
/// Whether the error should name one figure in both cases is undecided, so a change to it is not
/// a change of the rules this test holds: the rejection, and a sender left untouched.
///
/// Independence: independent — the intrinsic charge and the sender's
/// funding are written out.
#[test]
fn test_a_gas_limit_below_the_intrinsic_charge_is_rejected() {
    assert_rejected_and_sender_untouched(
        call(CALLER, CALLEE, U256::ZERO, EMPTY_CALL - 1),
        InvalidTransaction::CallGasCostMoreThanGasLimit {
            gas_limit: EMPTY_CALL - 1,
            initial_gas: EMPTY_CALL,
        },
    );
}

/// A gas limit that covers the intrinsic charge but not the body's history is a validation
/// rejection too, naming the whole figure: the bytes a transaction carries are part of what makes
/// it valid, so a transaction that cannot pay for them is never included, and leaves the sender
/// untouched.
///
/// The figure the error carries — the whole minimum, the intrinsic charge and the body's history
/// together — is one reading of a point the spec leaves open: the spec fixes the rejection and its
/// error, not which figure the error names, and below the intrinsic charge alone the error names
/// that charge without the body (the test above). Whether it should name one figure in both cases
/// is undecided, so a change to it is not a change of the rules this test holds: the rejection, a
/// sender left untouched, and a gas limit of the whole minimum admitted.
///
/// Independence: independent for the intrinsic charge and the
/// sender's funding, which are written out; the body's history is priced by the production
/// helper.
#[test]
fn test_a_gas_limit_short_of_the_body_is_rejected() {
    if runs_at_measurement_prices() {
        return;
    }
    let minimum = EMPTY_CALL + body_history(0);
    for gas_limit in [EMPTY_CALL, EMPTY_CALL + 1, minimum - 1] {
        assert_rejected_and_sender_untouched(
            call(CALLER, CALLEE, U256::ZERO, gas_limit),
            InvalidTransaction::CallGasCostMoreThanGasLimit { gas_limit, initial_gas: minimum },
        );
    }
    assert_eq!(gas_used(funded(), call(CALLER, CALLEE, U256::ZERO, minimum)), Ok(minimum));
}

/// The history of a transaction's body is its intrinsic history gas once validation admitted it,
/// and nothing before: a transaction validation refuses — its gas limit below the intrinsic charge
/// alone, or short of the body's history — reports none, neither the figure it would have paid nor
/// the one the transaction before it paid.
#[test]
fn test_the_bodys_history_is_reported_once_validation_admits_it() {
    if history_is_free() {
        return;
    }
    let mut evm = MegaEvm::new(context(funded()));
    let minimum = EMPTY_CALL + body_history(0);
    evm.transact_raw(call(CALLER, CALLEE, U256::ZERO, minimum)).expect("the minimum is admitted");
    assert_eq!(evm.ctx().intrinsic_history_gas(), body_history(0), "admitted");
    // At a byte price so small that the body's history rounds to nothing, the minimum is the
    // intrinsic charge alone, and a gas limit equal to it is admitted: only a limit below the
    // charge it is compared with is refused.
    for (gas_limit, initial_gas) in
        [(EMPTY_CALL - 1, EMPTY_CALL), (EMPTY_CALL, minimum), (minimum - 1, minimum)]
            .into_iter()
            .filter(|(gas_limit, initial_gas)| gas_limit < initial_gas)
    {
        let refused = evm
            .transact_raw(call(CALLER, CALLEE, U256::ZERO, gas_limit))
            .expect_err("a gas limit below the minimum is refused");
        assert_eq!(
            refused.as_invalid_tx_err().and_then(InvalidTxError::as_invalid_tx_err),
            Some(&InvalidTransaction::CallGasCostMoreThanGasLimit { gas_limit, initial_gas }),
            "at {gas_limit}: the intrinsic charge refused it",
        );
        assert_eq!(evm.ctx().intrinsic_history_gas(), 0, "at {gas_limit}: refused");
    }
}
