//! What a keyless deployment is charged, charge by charge, below and above the execution cap.
//!
//! Every figure is measured against the same calldata sent to an account with no code, which
//! pays the same intrinsic gas and nothing else: what the `keylessDeploy` transaction spends
//! beyond it is the overhead, the charges its call makes for the creation's start, and what the
//! creation itself spent — its `gasUsed`.

use mega_evm::{constants::MAX_INITCODE_SIZE, system::keyless::KEYLESS_DEPLOY_OVERHEAD_GAS};
use revm::bytecode::opcode::{PUSH0, REVERT};

use super::*;

/// The length of the runtime the tests deploy.
const RUNTIME_LEN: usize = 5;

/// Every charge of a deployment by a signer with no account, to an empty address, is accounted
/// for exactly, and the transaction spends the same below and above the execution cap:
///
/// - the overhead, and the regular gas the `CREATE` opcode charges its frame;
/// - the signer's account and the created account, as state gas, once each;
/// - the two write records the creation's start makes — the signer's nonce and the created account
///   — as history;
/// - what the creation spent, which is `gasUsed`: its regular gas, the state gas of the code it
///   deposited and the history of the same bytes.
#[test]
fn test_every_charge_of_a_deployment_is_accounted_for() {
    let init_code = deploying(&runtime(RUNTIME_LEN));
    let opcode = create_regular(init_code.len());
    let deployment = Deployment::new(init_code);
    let len = RUNTIME_LEN as u64;
    let deposit_state = satin_gas_params().code_deposit_state_gas(RUNTIME_LEN);
    let mut spent = None;
    for gas_limit in GAS_LIMITS {
        let outcome = deploy(system_db(), &deployment, gas_limit);
        let gas_used = returned(&outcome).gasUsed;
        let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
        let [total, regular, state, history_gas, history_bytes] = beyond(&outcome, &reference);

        let new_account = entry(GasId::new_account_state_gas());
        let created = entry(GasId::create_state_gas());
        assert_eq!(
            total,
            KEYLESS_DEPLOY_OVERHEAD_GAS + opcode + new_account + created + 2 * record() + gas_used,
            "at {gas_limit}",
        );
        assert_eq!(state, new_account + created + deposit_state, "at {gas_limit}");
        assert_eq!(history_gas, 2 * record() + history(len), "at {gas_limit}");
        assert_eq!(history_bytes, 2 * 40 + len, "at {gas_limit}");
        let creation_regular = gas_used - deposit_state - history(len);
        assert_eq!(
            regular,
            KEYLESS_DEPLOY_OVERHEAD_GAS + opcode + creation_regular,
            "at {gas_limit}"
        );
        assert_eq!(outcome.usage.write_records, 2, "the signer's nonce and the created account");

        let previous = *spent.get_or_insert((total, gas_used));
        assert_eq!((total, gas_used), previous, "the pools do not move what a deployment costs");
    }
}

/// A signer with no account is charged its account once, as state gas: the only difference
/// between a signer that holds a wei and one that holds nothing. EIP-2780 charges the transaction's
/// recipient, which is the `KeylessDeploy` contract and exists, so it adds nothing of its own.
#[test]
fn test_an_empty_signer_is_charged_its_account_once() {
    let deployment = Deployment::new(deploying(&runtime(RUNTIME_LEN)));
    for gas_limit in GAS_LIMITS {
        let empty = deploy(db_for(&deployment, U256::ZERO), &deployment, gas_limit);
        let funded = deploy(db_for(&deployment, U256::from(1)), &deployment, gas_limit);
        let [total, regular, state, history_gas, _] = beyond(&empty, &funded);
        let new_account = entry(GasId::new_account_state_gas());
        assert_eq!([total, regular, state, history_gas], [new_account, 0, new_account, 0]);
        assert_eq!(returned(&empty).gasUsed, returned(&funded).gasUsed);
        assert_eq!(nonce(&empty, deployment.signer), 1);
        assert_eq!(nonce(&funded, deployment.signer), 1);
    }
}

/// The created account is charged only when the deploy address is empty, as the `CREATE` opcode
/// charges it: an address that already holds a balance is an account already, the deployment
/// lands on it, and the balance stays.
#[test]
fn test_the_created_account_is_charged_only_when_the_address_is_empty() {
    let deployment = Deployment::new(deploying(&runtime(RUNTIME_LEN)));
    for gas_limit in GAS_LIMITS {
        let empty = deploy(system_db(), &deployment, gas_limit);
        let held = system_db().account_balance(deployment.address, U256::from(7_777));
        let held = deploy(held, &deployment, gas_limit);
        assert_eq!(returned(&held).deployedAddress, deployment.address);
        assert_eq!(
            held.state.get(&deployment.address).map(|account| account.info.balance),
            Some(U256::from(7_777)),
        );
        let [total, regular, state, history_gas, _] = beyond(&empty, &held);
        let created = entry(GasId::create_state_gas());
        assert_eq!([total, regular, state, history_gas], [created, 0, created, 0]);
    }
}

/// A refused call keeps the overhead and nothing else: every charge made after it is taken back
/// with the revert, whatever the pools.
#[test]
fn test_a_refused_call_keeps_the_overhead_alone() {
    // A value the signer cannot fund: refused at the last rule, after every read.
    let deployment = Deployment::with_value(deploying(&runtime(RUNTIME_LEN)), U256::from(1));
    for gas_limit in GAS_LIMITS {
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert_eq!(refusal(&outcome), KeylessDeployError::InsufficientBalance);
        let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
        let [total, regular, state, history_gas, history_bytes] = beyond(&outcome, &reference);
        assert_eq!(
            [total, regular, state, history_gas, history_bytes],
            [KEYLESS_DEPLOY_OVERHEAD_GAS, KEYLESS_DEPLOY_OVERHEAD_GAS, 0, 0, 0],
            "at {gas_limit}",
        );
        assert_eq!(outcome.usage.write_records, 0);
    }
}

/// A deployment whose init code reverts gives the created account's charge back, and the record
/// of the account it did not create; the signer's account and its nonce record stay, because the
/// nonce stays spent.
#[test]
fn test_a_failed_deployment_gives_the_created_account_back() {
    let deployment = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
    for gas_limit in GAS_LIMITS {
        let outcome = deploy(system_db(), &deployment, gas_limit);
        let error = failure(&outcome);
        let KeylessDeployError::ExecutionReverted { gas_used, .. } = error else {
            panic!("expected ExecutionReverted, got {error:?}");
        };
        let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
        let [total, regular, state, history_gas, history_bytes] = beyond(&outcome, &reference);
        let new_account = entry(GasId::new_account_state_gas());
        assert_eq!(state, new_account, "at {gas_limit}");
        assert_eq!(history_gas, record(), "at {gas_limit}");
        assert_eq!(history_bytes, 40, "at {gas_limit}");
        assert_eq!(
            regular,
            KEYLESS_DEPLOY_OVERHEAD_GAS + create_regular(3) + gas_used,
            "at {gas_limit}"
        );
        assert_eq!(total, regular + state + history_gas);
        assert_eq!(nonce(&outcome, deployment.signer), 1, "the nonce stays spent");
        assert_eq!(outcome.usage.write_records, 1, "the signer's nonce");
    }
}

/// The call pays the regular gas the `CREATE` opcode charges its frame on top of the overhead,
/// whatever the pools: the schedule's `create` entry, 32,000, and EIP-3860's 2 gas per word of
/// init code, 65,536 for init code of the maximum size. Init code of zero bytes stops at once and
/// deploys nothing, so the creation itself spends nothing, and every other charge of the call is
/// state and history gas.
#[test]
fn test_the_call_pays_the_create_opcodes_regular_gas() {
    assert_eq!(create_regular(1), 32_000 + 2);
    assert_eq!(create_regular(33), 32_000 + 4);
    assert_eq!(create_regular(MAX_INITCODE_SIZE), 32_000 + 65_536);
    let state = entry(GasId::new_account_state_gas()) + entry(GasId::create_state_gas());
    for len in [1, 33, MAX_INITCODE_SIZE] {
        let deployment = Deployment::new(vec![0; len].into());
        // Gas limits that cover the body of a transaction carrying a mebibyte of calldata, below
        // and above the execution cap.
        for gas_limit in [TX_GAS_LIMIT_CAP * 3 / 4, 10 * TX_GAS_LIMIT_CAP] {
            let outcome = deploy(system_db(), &deployment, gas_limit);
            let KeylessDeployError::EmptyCodeDeployed { gas_used } = failure(&outcome) else {
                panic!("expected EmptyCodeDeployed: {:?}", outcome.result);
            };
            assert_eq!(gas_used, 0);
            let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
            let [total, regular, state_gas, history_gas, _] = beyond(&outcome, &reference);
            let regular_expected = KEYLESS_DEPLOY_OVERHEAD_GAS + create_regular(len);
            assert_eq!(regular, regular_expected, "{len} bytes at {gas_limit}");
            assert_eq!(state_gas, state, "{len} bytes at {gas_limit}");
            assert_eq!(history_gas, 2 * record(), "{len} bytes at {gas_limit}");
            assert_eq!(
                total,
                regular_expected + state + 2 * record(),
                "{len} bytes at {gas_limit}"
            );
        }
    }
}

/// Both upfront charges are priced by the SALT bucket they land in — the signer's account in the
/// signer's, the created account in the deploy address's — and regular gas does not move with it.
#[test]
fn test_the_upfront_charges_scale_with_their_buckets() {
    let deployment = Deployment::new(deploying(&runtime(RUNTIME_LEN)));
    let new_account = entry(GasId::new_account_state_gas());
    let created = entry(GasId::create_state_gas());
    for gas_limit in GAS_LIMITS {
        let minimal = salt_run(system_db(), TestExternalEnvs::new(), &deployment, gas_limit);
        for (m_signer, m_address) in [(2, 1), (1, 2), (3, 2)] {
            let envs = crowded(
                crowded(TestExternalEnvs::new(), deployment.signer, m_signer),
                deployment.address,
                m_address,
            );
            let crowded_run = salt_run(system_db(), envs, &deployment, gas_limit);
            assert_eq!(crowded_run.gas.regular, minimal.gas.regular, "m does not touch regular");
            assert_eq!(
                crowded_run.gas.state - minimal.gas.state,
                (m_signer - 1) * new_account +
                    (m_address - 1) * (created + satin_gas_params().code_deposit_state_gas(5)),
                "signer at m = {m_signer}, address at m = {m_address}, gas limit {gas_limit}",
            );
        }
    }
}

/// A deployment that fails gives the created account back at the price it was charged, however
/// crowded the deploy address's bucket, and the bucket is read once for both; the signer's
/// account is kept at its own bucket's price.
#[test]
fn test_a_failed_deployment_gives_the_created_account_back_at_its_price() {
    let deployment = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
    let new_account = entry(GasId::new_account_state_gas());
    for gas_limit in GAS_LIMITS {
        for m in [1, 2, 8] {
            let envs = crowded(
                crowded(TestExternalEnvs::new(), deployment.address, m),
                deployment.signer,
                m,
            );
            let outcome = salt_run(system_db(), envs.clone(), &deployment, gas_limit);
            assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }));
            let reference = salt_run(
                system_db(),
                TestExternalEnvs::new(),
                &Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT])),
                gas_limit,
            );
            assert_eq!(outcome.gas.state, m * new_account, "m = {m}, gas limit {gas_limit}");
            assert_eq!(outcome.gas.regular, reference.gas.regular, "m = {m}");
            assert_eq!(
                envs.bucket_queries(bucket(deployment.address)),
                1,
                "one read priced the charge and its refund",
            );
        }
    }
}

/// A signer that sends its own deployment is the transaction's sender, whose account the body
/// counts: the creation's nonce bump makes no record of its own, only the created account does,
/// and the signer, an account already, is charged nothing for it.
#[test]
fn test_a_signer_that_sends_its_own_deployment_makes_no_record_of_its_own() {
    let deployment = Deployment::new(deploying(&runtime(RUNTIME_LEN)));
    for gas_limit in GAS_LIMITS {
        let mut tx =
            call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
        tx.0.base.gas_limit = gas_limit;
        tx.0.base.caller = deployment.signer;
        let outcome = MegaEvm::new(context(system_db()))
            .execute_transaction(tx)
            .expect("a valid transaction");
        assert_eq!(returned(&outcome).deployedAddress, deployment.address, "at {gas_limit}");
        // The transaction bumped its sender from 0 to 1, and the creation from 1 to 2.
        assert_eq!(nonce(&outcome, deployment.signer), 2);
        assert_eq!(outcome.usage.write_records, 1, "the created account alone");
        let deposit_state = satin_gas_params().code_deposit_state_gas(RUNTIME_LEN);
        assert_eq!(outcome.gas.state, entry(GasId::create_state_gas()) + deposit_state);
    }
}
