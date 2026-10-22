//! The `MegaLimitControl` interceptor: `remainingComputeGas`, the value policy, the unknown
//! selector and the call schemes.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::system::{IMegaLimitControl, LIMIT_CONTROL_ADDRESS, NON_ZERO_TRANSFER_REVERT_DATA};
use revm::bytecode::opcode::{CALL, CALLCODE, DELEGATECALL};

use crate::common::{call_tx, output, revert_data, run, system_db, GAS_LIMIT};

const REMAINING_COMPUTE_GAS: [u8; 4] = IMegaLimitControl::remainingComputeGasCall::SELECTOR;
const NOT_INTERCEPTED: [u8; 4] = IMegaLimitControl::NotIntercepted::SELECTOR;

/// A call from a contract to the limit control contract, run as a transaction.
fn through_contract(scheme: u8, data: &[u8], value: u64) -> (bool, Bytes) {
    crate::common::through_contract(LIMIT_CONTROL_ADDRESS, scheme, data, value)
}

/// Decodes what `remainingComputeGas()` answered.
fn remaining(data: &Bytes) -> u64 {
    IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(data)
        .expect("the answer is a uint64")
}

/// `remainingComputeGas()` is intercepted and answers the caller's regular gas, which is below the
/// execution cap and below what the transaction had. What the figure is exactly is in
/// `remaining_compute_gas`.
#[test]
fn test_remaining_compute_gas_is_intercepted() {
    let (status, data) = through_contract(CALL, &REMAINING_COMPUTE_GAS, 0);
    assert!(status);
    let remaining = remaining(&data);
    assert!(remaining > 0, "the call was forwarded gas");
    assert!(remaining < GAS_LIMIT, "the caller spent gas before the call");
    assert!(
        remaining <= mega_evm::constants::TX_GAS_LIMIT_CAP,
        "regular gas never exceeds the execution cap",
    );
}

/// A call that carries value is refused with `NonZeroTransfer()`: the method reads and takes no
/// value.
#[test]
fn test_a_value_bearing_call_is_refused() {
    let (status, data) = through_contract(CALL, &REMAINING_COMPUTE_GAS, 1);
    assert!(!status);
    assert_eq!(data, Bytes::from_static(&NON_ZERO_TRANSFER_REVERT_DATA));
}

/// A selector the contract does not intercept, and an input too short to hold one, fall through
/// to the deployed bytecode, which reverts with `NotIntercepted()`.
#[test]
fn test_an_unknown_selector_falls_through_to_not_intercepted() {
    for data in [vec![0xde, 0xad, 0xbe, 0xef], vec![], REMAINING_COMPUTE_GAS[..3].to_vec()] {
        let result = run(system_db(), call_tx(LIMIT_CONTROL_ADDRESS, &data, U256::ZERO));
        assert_eq!(revert_data(&result), Bytes::from_static(&NOT_INTERCEPTED), "input {data:?}");
    }
}

/// A selector followed by trailing bytes is intercepted: admission is the four bytes alone.
#[test]
fn test_a_selector_with_trailing_bytes_is_intercepted() {
    for tail in [vec![0xff], vec![0x00; 32]] {
        let data: Vec<u8> = REMAINING_COMPUTE_GAS.iter().copied().chain(tail).collect();
        let result = run(system_db(), call_tx(LIMIT_CONTROL_ADDRESS, &data, U256::ZERO));
        assert!(remaining(&output(&result)) > 0);
    }
}

/// `CALLCODE` and `DELEGATECALL` never reach an interceptor: the scheme guard refuses them, and
/// the contract's own bytecode runs, which reverts with `NotIntercepted()`.
#[test]
fn test_callcode_and_delegatecall_are_not_intercepted() {
    for scheme in [CALLCODE, DELEGATECALL] {
        let (status, data) = through_contract(scheme, &REMAINING_COMPUTE_GAS, 0);
        assert!(!status, "scheme {scheme:#x} must not be intercepted");
        assert_eq!(data, Bytes::from_static(&NOT_INTERCEPTED));
    }
}
