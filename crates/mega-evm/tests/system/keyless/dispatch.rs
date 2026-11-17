//! The `KeylessDeploy` dispatch: which calls are recognised, what they are charged, and which
//! run the deployed bytecode instead.
//!
//! A recognised call pays the fixed overhead before anything else, then either starts its
//! deployment or is refused. The tests here send transactions a rule refuses, so that what is
//! left beyond the reference transaction — the same calldata to an account with no code — is the
//! overhead alone.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::system::keyless::{
    IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_OVERHEAD_GAS,
};
use revm::context::result::ExecutionResult;

use crate::common::{body_history, call_tx, revert_data, run, system_db};

const KEYLESS_DEPLOY: [u8; 4] = IKeylessDeploy::keylessDeployCall::SELECTOR;

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
