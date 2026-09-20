//! The `MegaAccessControl` interceptor: the three intercepted methods, the value policy, the
//! unknown selector and the call schemes.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    system::{IMegaAccessControl, ACCESS_CONTROL_ADDRESS, NON_ZERO_TRANSFER_REVERT_DATA},
    test_utils::MemoryDatabase,
};
use revm::bytecode::opcode::{CALL, CALLCODE, DELEGATECALL, STATICCALL};

use crate::common::{
    call_tx, calls_with, output, revert_data, run, split_outcome, system_db, with_contract,
    CONTRACT,
};

const DISABLE: [u8; 4] = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
const ENABLE: [u8; 4] = IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR;
const IS_DISABLED: [u8; 4] = IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR;
const NOT_INTERCEPTED: [u8; 4] = IMegaAccessControl::NotIntercepted::SELECTOR;

/// A call from a contract to the access control contract, run as a transaction.
fn through_contract(scheme: u8, data: &[u8], value: u64) -> (bool, Bytes) {
    let code = calls_with(scheme, ACCESS_CONTROL_ADDRESS, data, value);
    let result = run(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    let outcome = output(&result);
    let (status, data) = split_outcome(&outcome);
    (status, Bytes::copy_from_slice(data))
}

/// The three methods are intercepted: the two switches return nothing and the query answers
/// `false`, which is what the common execution layer knows while detention is not here.
#[test]
fn test_the_three_methods_are_intercepted() {
    let disabled = IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(&false);
    for (selector, expected) in
        [(DISABLE, Bytes::new()), (ENABLE, Bytes::new()), (IS_DISABLED, Bytes::from(disabled))]
    {
        let (status, data) = through_contract(CALL, &selector, 0);
        assert!(status, "the call to the intercepted method failed");
        assert_eq!(data, expected);
    }
}

/// A transaction that calls the contract directly is intercepted too: no opcode has to start
/// the call.
#[test]
fn test_a_direct_transaction_is_intercepted() {
    let result = run(system_db(), call_tx(ACCESS_CONTROL_ADDRESS, IS_DISABLED, U256::ZERO));
    assert_eq!(
        output(&result),
        Bytes::from(IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(
            &false
        )),
    );
}

/// A `STATICCALL` reaches the interceptor: the three methods write nothing.
#[test]
fn test_a_static_call_is_intercepted() {
    let (status, data) = through_contract(STATICCALL, &IS_DISABLED, 0);
    assert!(status);
    assert_eq!(
        data,
        Bytes::from(IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(
            &false
        )),
    );
    let (status, data) = through_contract(STATICCALL, &DISABLE, 0);
    assert!(status, "the switch does not write state, so a static caller may set it");
    assert!(data.is_empty());
}

/// A call that carries value to one of the three methods is refused with `NonZeroTransfer()`:
/// they read or steer execution and take no value.
#[test]
fn test_a_value_bearing_call_is_refused() {
    for selector in [DISABLE, ENABLE, IS_DISABLED] {
        let (status, data) = through_contract(CALL, &selector, 1);
        assert!(!status, "a value-bearing call must not succeed");
        assert_eq!(data, Bytes::from_static(&NON_ZERO_TRANSFER_REVERT_DATA));
    }
}

/// The value policy is per method, after the selector matched: a value-bearing call to a
/// selector the contract does not intercept still falls through to the bytecode.
#[test]
fn test_a_value_bearing_call_to_an_unknown_selector_falls_through() {
    let (status, data) = through_contract(CALL, &[0xde, 0xad, 0xbe, 0xef], 1);
    assert!(!status);
    assert_eq!(data, Bytes::from_static(&NOT_INTERCEPTED));
}

/// A selector the contract does not intercept, and an input too short to hold one, fall through
/// to the deployed bytecode, which reverts with `NotIntercepted()`.
#[test]
fn test_an_unknown_selector_falls_through_to_not_intercepted() {
    for data in [vec![0xde, 0xad, 0xbe, 0xef], vec![], vec![0x00; 3], DISABLE[..3].to_vec()] {
        let result = run(system_db(), call_tx(ACCESS_CONTROL_ADDRESS, &data, U256::ZERO));
        assert_eq!(
            revert_data(&result),
            Bytes::from_static(&NOT_INTERCEPTED),
            "input {data:?} must run the bytecode",
        );
    }
}

/// A selector followed by trailing bytes is intercepted: admission is the four bytes alone.
#[test]
fn test_a_selector_with_trailing_bytes_is_intercepted() {
    for tail in [vec![0xff], vec![0x00; 32], vec![0xab; 100]] {
        let data: Vec<u8> = IS_DISABLED.iter().copied().chain(tail).collect();
        let result = run(system_db(), call_tx(ACCESS_CONTROL_ADDRESS, &data, U256::ZERO));
        assert_eq!(
            output(&result),
            Bytes::from(IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(
                &false
            )),
        );
    }
}

/// `CALLCODE` and `DELEGATECALL` never reach an interceptor: the scheme guard refuses them, and
/// the contract's own bytecode runs, which reverts with `NotIntercepted()`.
#[test]
fn test_callcode_and_delegatecall_are_not_intercepted() {
    for scheme in [CALLCODE, DELEGATECALL] {
        for selector in [DISABLE, ENABLE, IS_DISABLED] {
            let (status, data) = through_contract(scheme, &selector, 0);
            assert!(!status, "scheme {scheme:#x} must not be intercepted");
            assert_eq!(data, Bytes::from_static(&NOT_INTERCEPTED));
        }
    }
}

/// An account without the contract's code at the address is not the access control contract:
/// the interceptor answers whatever the account holds, because the address is what it matches.
#[test]
fn test_the_address_is_what_the_interceptor_matches() {
    let db = MemoryDatabase::default().account_balance(crate::common::CALLER, U256::from(1_000));
    let result = run(db, call_tx(ACCESS_CONTROL_ADDRESS, IS_DISABLED, U256::ZERO));
    assert_eq!(
        output(&result),
        Bytes::from(IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(
            &false
        )),
        "the interceptor answers before the code of the account is looked at",
    );
}
