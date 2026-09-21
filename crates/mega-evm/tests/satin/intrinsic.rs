//! The intrinsic phase: the EIP-7976 calldata floor, the EIP-7981 access-list data charge, and
//! what the execution cap admits.
//!
//! Satin keeps Amsterdam's floor: a base of 12,000 with the recipient and value charges on top,
//! and every calldata byte at 64 gas whether it is zero or not. EIP-7981 puts the access list's
//! own bytes in the floor at the same rate, while the per-item intrinsic charge stays at its
//! pre-EIP-8038 value. A transaction whose floor is above the 200M execution cap is rejected
//! before it runs, which is what bounds the calldata a transaction may carry.

use alloy_evm::{Evm, EvmError, InvalidTxError};
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants::{ACCOUNT_STATE_GAS, TX_GAS_LIMIT_CAP},
    test_utils::{op_transaction, MemoryDatabase},
    MegaEvm, MegaTransaction,
};
use revm::{
    context::{
        result::{ExecutionResult, InvalidTransaction},
        transaction::{AccessList, AccessListItem, TransactionType},
        TxEnv,
    },
    primitives::B256,
    Database,
};

use crate::common::{call, context, create, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000800000");
const CALLEE: Address = address!("0000000000000000000000000000000000800001");

/// The intrinsic gas of a plain call to an account that is not the sender: the EIP-2780 sender
/// base of 12,000 plus 3,000 for reaching the recipient.
///
/// The 3,000 is EIP-2780's own fixed charge, not the schedule's cold-account entry: inside
/// execution the schedule prices a cold account access at 2,600 — 100 for the read and 2,500 for
/// the cold surcharge — so repricing that entry would leave this number where it is.
const EMPTY_CALL: u64 = 15_000;

/// What one byte of anything a transaction carries costs in the EIP-7976 / EIP-7981 floor: four
/// floor tokens at 16 gas each.
const FLOOR_PER_BYTE: u64 = 64;

/// A database where the sender can pay and `CALLEE` exists.
fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(1))
}

/// Runs `tx` and returns the gas it used, or the validation error that rejected it.
fn gas_used(db: MemoryDatabase, tx: MegaTransaction) -> Result<u64, String> {
    let mut evm = MegaEvm::new(context(db));
    match evm.transact_raw(tx) {
        Ok(outcome) => {
            assert!(outcome.result.is_success(), "{:?}", outcome.result);
            Ok(outcome.result.gas().tx_gas_used())
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

/* ---------- the EIP-7976 calldata floor ---------- */

/// A call carrying `len` bytes of `byte`.
fn with_calldata(len: usize, byte: u8) -> MegaTransaction {
    crate::common::call_with_data(CALLER, CALLEE, Bytes::from(vec![byte; len]), 10_000_000)
}

/// Empty calldata pays the intrinsic gas alone: the floor is the same number, so neither is the
/// binding one.
#[test]
fn test_the_floor_of_empty_calldata_is_the_intrinsic_gas() {
    assert_eq!(gas_used(funded(), with_calldata(0, 0)), Ok(EMPTY_CALL));
}

/// One byte of calldata costs 64 gas in the floor, which is above what the intrinsic phase
/// charges for it, so the floor is what the transaction pays.
#[test]
fn test_one_calldata_byte_costs_sixty_four_gas() {
    assert_eq!(gas_used(funded(), with_calldata(1, 0x42)), Ok(EMPTY_CALL + FLOOR_PER_BYTE));
}

/// A hundred bytes, and a kilobyte: the floor is linear in the byte count at the same rate.
#[test]
fn test_the_floor_is_sixty_four_gas_for_every_calldata_byte() {
    for len in [100u64, 1_024, 4_096] {
        assert_eq!(
            gas_used(funded(), with_calldata(len as usize, 0x42)),
            Ok(EMPTY_CALL + FLOOR_PER_BYTE * len),
            "{len} bytes"
        );
    }
}

/// A zero byte costs the same as a non-zero one in the floor: EIP-7976 prices the space a byte
/// takes in a block, not its value.
#[test]
fn test_a_zero_calldata_byte_costs_the_same_as_a_non_zero_one() {
    let zeros = gas_used(funded(), with_calldata(1_024, 0x00));
    let non_zeros = gas_used(funded(), with_calldata(1_024, 0xff));
    assert_eq!(zeros, non_zeros);
    assert_eq!(zeros, Ok(EMPTY_CALL + FLOOR_PER_BYTE * 1_024));
}

/* ---------- the EIP-7981 access-list data charge ---------- */

/// A call whose access list names `CALLEE` with `keys` storage keys.
fn with_access_list(keys: usize) -> MegaTransaction {
    let access_list = AccessList(vec![AccessListItem {
        address: CALLEE,
        storage_keys: (0..keys).map(|i| B256::with_last_byte(i as u8)).collect(),
    }]);
    OpTx(op_transaction(TxEnv {
        // Legacy transactions carry no access list, so the type has to say EIP-2930.
        tx_type: TransactionType::Eip2930 as u8,
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 10_000_000,
        access_list,
        ..Default::default()
    }))
}

/// An access-list address costs 2,400 in the intrinsic phase — its pre-EIP-8038 price — and its
/// twenty bytes cost 64 each in the floor. With one address alone the intrinsic charge is the
/// larger of the two, so that is what the transaction pays.
#[test]
fn test_an_access_list_address_is_priced_at_the_pre_eip8038_rate() {
    assert_eq!(gas_used(funded(), with_access_list(0)), Ok(EMPTY_CALL + 2_400));
}

/// A storage key costs 1,900 in the intrinsic phase and its thirty-two bytes cost 2,048 in the
/// floor, so every key pushes the floor further above the intrinsic sum. Past the address's
/// 1,120-gas head start the floor takes over, and from there the transaction pays the bytes.
#[test]
fn test_access_list_bytes_are_counted_in_the_floor_at_sixty_four_each() {
    let address_bytes = 20 * FLOOR_PER_BYTE;
    let key_bytes = 32 * FLOOR_PER_BYTE;
    for keys in [0u64, 1, 7] {
        assert_eq!(
            gas_used(funded(), with_access_list(keys as usize)),
            Ok(EMPTY_CALL + 2_400 + 1_900 * keys),
            "{keys} keys: the intrinsic charge is still the larger"
        );
    }
    for keys in [8u64, 16, 64] {
        assert_eq!(
            gas_used(funded(), with_access_list(keys as usize)),
            Ok(EMPTY_CALL + address_bytes + key_bytes * keys),
            "{keys} keys: the floor is the larger"
        );
    }
}

/* ---------- the execution cap ---------- */

/// The largest calldata the execution cap admits: its floor is the cap's own number rounded down
/// to a whole byte.
const LARGEST_CALLDATA: u64 = 3_124_765;

/// A call carrying `len` calldata bytes with a gas limit above the execution cap, so the cap is
/// what the floor is measured against.
fn with_calldata_over_the_cap(len: u64) -> MegaTransaction {
    crate::common::call_with_data(
        CALLER,
        CALLEE,
        Bytes::from(vec![0u8; len as usize]),
        TX_GAS_LIMIT_CAP + 1_000_000,
    )
}

/// At the bound the transaction is admitted and pays its floor; one byte more and the floor is
/// above the execution cap, which rejects it before it runs.
#[test]
fn test_the_execution_cap_bounds_the_calldata_a_transaction_may_carry() {
    let floor = EMPTY_CALL + FLOOR_PER_BYTE * LARGEST_CALLDATA;
    assert!(floor <= TX_GAS_LIMIT_CAP, "{floor} is within the cap");
    assert!(
        floor + FLOOR_PER_BYTE > TX_GAS_LIMIT_CAP,
        "one byte more is over it, so the bound is the largest"
    );

    let db = funded().account_balance(CALLER, U256::from(10u128.pow(24)));
    assert_eq!(gas_used(db.clone(), with_calldata_over_the_cap(LARGEST_CALLDATA)), Ok(floor));

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
/// measured against whichever of the two is smaller.
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
    let expected = format!(
        "{:?}",
        InvalidTransaction::GasFloorMoreThanGasLimit {
            gas_floor: EMPTY_CALL + FLOOR_PER_BYTE * len,
            gas_limit,
        }
    );
    assert_eq!(gas_used(funded(), tx), Err(expected));
    assert_sender_untouched();
}

/// The sender's nonce and balance are untouched by a transaction validation rejected.
fn assert_sender_untouched() {
    let mut db = funded();
    let info = db.basic(CALLER).unwrap().unwrap();
    assert_eq!(info.nonce, 0);
    assert_eq!(info.balance, U256::from(10u64.pow(18)));
}

/* ---------- the state component is charged where it happens ---------- */

/// A transfer whose recipient must be created needs the account's state gas on top of the
/// intrinsic charge. The state gas is not part of the intrinsic phase, so a gas limit that covers
/// the intrinsic charge alone is accepted and then runs out of gas, charging the sender.
#[test]
fn test_a_transfer_that_cannot_pay_the_new_account_runs_out_of_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let mut evm = MegaEvm::new(context(db));
    let outcome = evm
        .transact_raw(call(CALLER, CALLEE, U256::from(1), 21_000 + ACCOUNT_STATE_GAS - 1))
        .expect("the transaction is admitted");

    assert!(matches!(outcome.result, ExecutionResult::Halt { .. }), "{:?}", outcome.result);
    assert_eq!(outcome.state[&CALLER].info.nonce, 1, "the sender paid for the attempt");
}

/// The same for a creation: the created account's state gas is charged as the frame starts, not
/// at validation.
#[test]
fn test_a_creation_that_cannot_pay_its_account_runs_out_of_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let mut evm = MegaEvm::new(context(db));
    let outcome = evm
        .transact_raw(create(CALLER, Bytes::new(), 24_000 + ACCOUNT_STATE_GAS - 1))
        .expect("the transaction is admitted");

    assert!(matches!(outcome.result, ExecutionResult::Halt { .. }), "{:?}", outcome.result);
    assert_eq!(outcome.state[&CALLER].info.nonce, 1, "the sender paid for the attempt");
}

/// A gas limit below the intrinsic charge itself is a validation rejection, which leaves the
/// sender untouched.
#[test]
fn test_a_gas_limit_below_the_intrinsic_charge_is_rejected() {
    let expected = format!(
        "{:?}",
        InvalidTransaction::CallGasCostMoreThanGasLimit {
            gas_limit: EMPTY_CALL - 1,
            initial_gas: EMPTY_CALL,
        }
    );
    assert_eq!(gas_used(funded(), call(CALLER, CALLEE, U256::ZERO, EMPTY_CALL - 1)), Err(expected));
    assert_sender_untouched();
}

/// A plain call within its limits still goes through, so the checks above are not too eager.
#[test]
fn test_a_valid_call_still_passes() {
    assert_eq!(gas_used(funded(), call(CALLER, CALLEE, U256::ZERO, 1_000_000)), Ok(EMPTY_CALL));
}
