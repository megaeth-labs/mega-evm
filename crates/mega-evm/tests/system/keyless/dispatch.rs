//! The `KeylessDeploy` dispatch: which calls are recognised, what they are charged, and which
//! run the deployed bytecode instead.
//!
//! A recognised call pays the fixed overhead before anything else, then is either rewritten into
//! its deployment or answered. The tests here send transactions a rule refuses, so that what is
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
    call_tx, calls_with, output, revert_data, run, split_outcome, system_db, with_contract,
    CONTRACT, GAS_LIMIT,
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
        assert!(
            result.result.gas().total_gas_spent() < KEYLESS_DEPLOY_OVERHEAD_GAS,
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
        assert!(
            result.result.gas().total_gas_spent() < KEYLESS_DEPLOY_OVERHEAD_GAS,
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
    let gas_limit = 60_000;
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

/// Admission is the selector alone here too: a payload too short to hold the arguments the ABI
/// names is still dispatched and charged, and the deployment rejects what it cannot decode with
/// `MalformedEncoding()`.
#[test]
fn test_a_truncated_payload_is_still_dispatched() {
    let truncated: Bytes =
        KEYLESS_DEPLOY.iter().copied().chain([0_u8; 16]).collect::<Vec<_>>().into();
    let dispatched = run_with(system_db(), truncated.clone(), GAS_LIMIT, no_limits());
    assert_eq!(refusal(&dispatched), KeylessDeployError::MalformedEncoding);
    let [total, ..] = beyond(&dispatched, &reference(truncated, GAS_LIMIT));
    assert_eq!(total, KEYLESS_DEPLOY_OVERHEAD_GAS);
}
