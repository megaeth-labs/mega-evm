//! The `KeylessDeploy` dispatch: which calls are recognised, what they are charged, and which
//! run the deployed bytecode instead.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::system::keyless::{
    IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_OVERHEAD_GAS,
};
use revm::{
    bytecode::opcode::{CALL, CALLCODE, DELEGATECALL, STATICCALL},
    context::result::ExecutionResult,
};

use crate::common::{
    call_tx, calls_with, output, revert_data, run, split_outcome, system_db, with_contract,
    CONTRACT, GAS_LIMIT,
};

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

/// A dispatched `keylessDeploy` transaction pays the fixed overhead, and then runs the deployed
/// bytecode, because the rewrite that turns it into a deployment is not here yet.
#[test]
fn test_a_keyless_deploy_transaction_pays_the_overhead() {
    let data = keyless_deploy(b"a transaction");
    let dispatched = run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, data.clone(), U256::ZERO));
    let plain =
        run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, unknown_selector(&data), U256::ZERO));

    assert_eq!(
        revert_data(&dispatched),
        Bytes::from_static(&NOT_INTERCEPTED),
        "the deployed bytecode runs after the charge",
    );
    // What each transaction spent, not what its receipt reports: the calldata floor of
    // EIP-7623 lifts the receipt of the cheaper one above what it spent.
    let charged = dispatched.result.gas().total_gas_spent() - plain.result.gas().total_gas_spent();
    assert!(
        (KEYLESS_DEPLOY_OVERHEAD_GAS..KEYLESS_DEPLOY_OVERHEAD_GAS + 1_000).contains(&charged),
        "{charged} is not the fixed overhead plus what the two paths differ by in the bytecode",
    );
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
/// dispatched: the bytecode runs and reverts on its own.
#[test]
fn test_an_unknown_selector_is_not_dispatched() {
    let data = keyless_deploy(b"a transaction");
    for input in [unknown_selector(&data), Bytes::new(), Bytes::from(KEYLESS_DEPLOY[..3].to_vec())]
    {
        let result = run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, &input, U256::ZERO));
        assert!(!result.result.is_success());
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
    let charge_of = |payload: &[u8]| {
        let data = keyless_deploy(payload);
        let dispatched =
            run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, data.clone(), U256::ZERO));
        let plain =
            run(system_db(), call_tx(KEYLESS_DEPLOY_ADDRESS, unknown_selector(&data), U256::ZERO));
        assert!(dispatched.result.gas().total_gas_spent() < GAS_LIMIT);
        dispatched.result.gas().total_gas_spent() - plain.result.gas().total_gas_spent()
    };
    let (empty, payload) = (charge_of(b""), charge_of(&[0xab; 512]));
    assert!(empty >= KEYLESS_DEPLOY_OVERHEAD_GAS, "{empty} is below the fixed overhead");
    assert!(payload >= KEYLESS_DEPLOY_OVERHEAD_GAS, "{payload} is below the fixed overhead");
}
