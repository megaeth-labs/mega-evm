//! The `KeylessDeploy` dispatch: which calls are recognised, what they are charged, and which
//! run the deployed bytecode instead.
//!
//! A recognised call pays the fixed overhead before anything else, then either starts its
//! deployment or is refused. The tests here send transactions a rule refuses, so that what is
//! left beyond the reference transaction — the same calldata to an account with no code — is the
//! overhead alone.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    system::keyless::{
        IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_OVERHEAD_GAS,
    },
    EvmTxRuntimeLimits,
};
use revm::{
    bytecode::opcode::{CALL, CALLCODE, DELEGATECALL, STATICCALL},
    context::result::ExecutionResult,
};

use super::{beyond, reference, refusal, run_with, GAS_LIMITS};
use crate::common::{
    body_history, call_tx, calls_with, output, revert_data, run, split_outcome, system_db,
    with_contract, CONTRACT, GAS_LIMIT,
};

/// No runtime limit.
fn no_limits() -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::no_limits()
}

const KEYLESS_DEPLOY: [u8; 4] = IKeylessDeploy::keylessDeployCall::SELECTOR;
const NOT_INTERCEPTED: [u8; 4] = IKeylessDeploy::NotIntercepted::SELECTOR;
const NO_ETHER_TRANSFER: [u8; 4] = IKeylessDeploy::NoEtherTransfer::SELECTOR;

/// The calldata of a `keylessDeploy` call carrying `transaction` bytes.
fn keyless_deploy(transaction: &[u8]) -> Bytes {
    Bytes::from(
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: Bytes::copy_from_slice(transaction),
            gasLimitOverride: U256::from(1_000_000),
        }
        .abi_encode(),
    )
}

/// The same calldata with a selector the contract does not know, so the call is not dispatched
/// and pays no overhead.
fn unknown_selector(data: &Bytes) -> Bytes {
    let mut data = data.to_vec();
    data[0] ^= 0xff;
    Bytes::from(data)
}

/// A dispatched `keylessDeploy` transaction pays the fixed overhead, then is refused by the
/// rules: bytes that do not decode as a signed transaction are `MalformedEncoding()`. The
/// deployed bytecode does not run, and the overhead is all the refusal keeps.
#[test]
fn test_a_keyless_deploy_transaction_pays_the_overhead() {
    let data = keyless_deploy(b"a transaction");
    for gas_limit in GAS_LIMITS {
        let dispatched = run_with(system_db(), data.clone(), gas_limit, no_limits());
        assert_eq!(refusal(&dispatched), KeylessDeployError::MalformedEncoding);
        let [total, regular, ..] = beyond(&dispatched, &reference(data.clone(), gas_limit));
        assert_eq!([total, regular], [KEYLESS_DEPLOY_OVERHEAD_GAS; 2], "at {gas_limit}");
    }
}

/// A call that carries value is refused with the ABI's own `NoEtherTransfer()`, and the
/// overhead is charged all the same.
#[test]
fn test_a_value_bearing_call_is_refused() {
    let data = keyless_deploy(b"a transaction");
    let code = calls_with(CALL, KEYLESS_DEPLOY_ADDRESS, &data, 1);
    let result = run(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    let outcome = output(&result);
    let (status, returned) = split_outcome(&outcome);

    // A call from a contract is at depth 1, where nothing is dispatched: the bytecode runs and
    // refuses the value itself, with the empty revert data of a non-payable method.
    assert!(!status);
    assert!(returned.is_empty(), "the bytecode refuses the value before it reaches the method");

    // The transaction itself is at depth 0, where the dispatch refuses it.
    let result = run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, data, U256::from(1)));
    assert_eq!(revert_data(&result), Bytes::from_static(&NO_ETHER_TRANSFER));
    assert!(
        result.result.gas().total_gas_spent() > KEYLESS_DEPLOY_OVERHEAD_GAS,
        "the refusal keeps the overhead",
    );
}

/// A call a contract makes is not dispatched, whatever its selector: only a transaction
/// deploys. The deployed bytecode runs and reverts with `NotIntercepted()`, and no overhead is
/// charged.
#[test]
fn test_a_call_from_a_contract_is_not_dispatched() {
    let data = keyless_deploy(b"a transaction");
    for scheme in [CALL, STATICCALL, CALLCODE, DELEGATECALL] {
        let code = calls_with(scheme, KEYLESS_DEPLOY_ADDRESS, &data, 0);
        let result = run(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
        let outcome = output(&result);
        let (status, returned) = split_outcome(&outcome);
        assert!(!status, "scheme {scheme:#x}");
        assert_eq!(returned, NOT_INTERCEPTED, "scheme {scheme:#x}");
        // The transaction's body is its only history: what it spent beside it is less than the
        // overhead.
        assert!(
            result.result.gas().total_gas_spent() - body_history(0) < KEYLESS_DEPLOY_OVERHEAD_GAS,
            "scheme {scheme:#x} paid the overhead of a dispatch that did not happen",
        );
    }
}

/// A selector the contract does not know, and an input too short to hold one, are not
/// dispatched: the bytecode runs and reverts on its own, with empty data.
///
/// `KeylessDeploy` carries no fallback, so a selector it does not declare finds no function to
/// run; `NotIntercepted()` is what its `keylessDeploy` body reverts with, which is a call the
/// contract does declare.
#[test]
fn test_an_unknown_selector_is_not_dispatched() {
    let data = keyless_deploy(b"a transaction");
    for input in [unknown_selector(&data), Bytes::new(), Bytes::from(KEYLESS_DEPLOY[..3].to_vec())]
    {
        let result = run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, &input, U256::ZERO));
        assert!(!result.result.is_success());
        assert!(
            revert_data(&result).is_empty(),
            "an input of {} bytes reverted with data the contract has no code to return",
            input.len(),
        );
        // The transaction's body is its only history: what it spent beside it is less than the
        // overhead.
        let spent = result.result.gas().total_gas_spent() - body_history(input.len() as u64);
        assert!(
            spent < KEYLESS_DEPLOY_OVERHEAD_GAS,
            "an input of {} bytes paid an overhead it does not owe",
            input.len(),
        );
    }
}

/// A `keylessDeploy` call the caller did not forward the overhead to is answered out of gas,
/// as a frame that ran out of gas is: the gas it was forwarded is spent.
#[test]
fn test_a_call_that_cannot_pay_the_overhead_runs_out_of_gas() {
    let data = keyless_deploy(b"a transaction");
    // 60,000 on top of the body's history: the intrinsic gas and less than the overhead beside it.
    let gas_limit = 60_000 + body_history(data.len() as u64);
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, data, U256::ZERO);
    tx.0.base.gas_limit = gas_limit;

    let result = run(system_db(), tx);
    assert!(
        matches!(result.result, ExecutionResult::Halt { .. }),
        "{:?} is not an out-of-gas halt",
        result.result,
    );
    assert_eq!(result.result.tx_gas_used(), gas_limit, "an out-of-gas frame spends what it had",);
}

/// The overhead is fixed: an empty deployment transaction is charged the same as one with a
/// payload.
#[test]
fn test_the_overhead_does_not_depend_on_the_payload() {
    for payload in [&b""[..], &[0xab; 512]] {
        let data = keyless_deploy(payload);
        let dispatched = run_with(system_db(), data.clone(), GAS_LIMIT, no_limits());
        assert_eq!(refusal(&dispatched), KeylessDeployError::MalformedEncoding);
        let [total, ..] = beyond(&dispatched, &reference(data, GAS_LIMIT));
        assert_eq!(total, KEYLESS_DEPLOY_OVERHEAD_GAS, "a payload of {} bytes", payload.len());
    }
}

/// A call is recognised only once its arguments decode as `(bytes, uint256)`: a payload too short
/// to hold them, an offset that points past the data, and a length that runs past it are not
/// dispatched. The deployed bytecode runs and its decoder reverts with empty data, and the
/// overhead is not charged.
#[test]
fn test_arguments_that_do_not_decode_are_not_dispatched() {
    let selector = KEYLESS_DEPLOY.to_vec();
    let word = |value: u64| U256::from(value).to_be_bytes::<32>().to_vec();
    let well_formed = keyless_deploy(b"a transaction").to_vec();

    let mut offset_past_the_data = selector.clone();
    offset_past_the_data.extend(word(0x1000));
    offset_past_the_data.extend(word(1_000_000));
    let mut length_past_the_data = selector.clone();
    length_past_the_data.extend(word(0x40));
    length_past_the_data.extend(word(1_000_000));
    length_past_the_data.extend(word(32));
    let cases = [
        ("short", [selector.clone(), vec![0_u8; 16]].concat()),
        ("selector alone", selector),
        ("offset past the data", offset_past_the_data),
        ("length past the data", length_past_the_data),
        ("cut inside the bytes", well_formed[..well_formed.len() - 40].to_vec()),
    ];
    for (name, input) in cases {
        let data = Bytes::from(input);
        for gas_limit in GAS_LIMITS {
            let outcome = run_with(system_db(), data.clone(), gas_limit, no_limits());
            let ExecutionResult::Revert { output, .. } = &outcome.result else {
                panic!("{name}: the bytecode did not revert: {:?}", outcome.result);
            };
            assert!(output.is_empty(), "{name}: the bytecode's decoder reverts with empty data");
            let [total, regular, ..] = beyond(&outcome, &reference(data.clone(), gas_limit));
            assert!(
                total < KEYLESS_DEPLOY_OVERHEAD_GAS && regular < KEYLESS_DEPLOY_OVERHEAD_GAS,
                "{name} at {gas_limit}: paid {total} beyond a plain call, an overhead it does not \
                 owe",
            );
        }
    }
}

/// Bytes after a valid encoding do not prevent the recognition: the call is dispatched and pays
/// the overhead, and the deployment rejects what the first argument carries.
#[test]
fn test_bytes_after_a_valid_encoding_are_still_dispatched() {
    let mut data = keyless_deploy(b"a transaction").to_vec();
    data.extend([0xab; 5]);
    let data = Bytes::from(data);
    let dispatched = run_with(system_db(), data.clone(), GAS_LIMIT, no_limits());
    assert_eq!(refusal(&dispatched), KeylessDeployError::MalformedEncoding);
    let [total, ..] = beyond(&dispatched, &reference(data, GAS_LIMIT));
    assert_eq!(total, KEYLESS_DEPLOY_OVERHEAD_GAS);
}

/// A call that carries value and whose arguments do not decode is not dispatched either: the
/// bytecode runs and refuses the value itself, with the empty revert data of a non-payable
/// method, and no overhead is charged.
#[test]
fn test_a_value_call_whose_arguments_do_not_decode_runs_the_bytecode() {
    let truncated: Bytes = KEYLESS_DEPLOY.iter().copied().chain([0_u8; 16]).collect();
    let result =
        run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, truncated.clone(), U256::from(1)));
    assert!(!result.result.is_success());
    assert!(revert_data(&result).is_empty());
    // The same input without value is not dispatched either, so it pays no overhead; the value
    // call must spend within an overhead of it, whatever the calldata costs at the byte prices in
    // effect.
    let without_value = run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, truncated, U256::ZERO));
    let (with, without) =
        (result.result.gas().total_gas_spent(), without_value.result.gas().total_gas_spent());
    assert!(
        with.abs_diff(without) < KEYLESS_DEPLOY_OVERHEAD_GAS,
        "the value call spent {with} against {without} without value: an overhead it does not owe",
    );
}
