//! The limits a keyless deployment is held to, and what a stop leaves behind.
//!
//! The creation is an ordinary frame below its `keylessDeploy` call, so it is held to everything
//! an ordinary creation is: its share of the data-size and KV budgets, the transaction's limits,
//! the state-gas limit. A frame budget reverts the creation alone, and the call answers
//! `ExecutionReverted` with the stop's revert data, the signer's nonce spent. A transaction limit
//! stops the transaction: the call reverts with the stop and takes back everything the
//! deployment wrote, the signer's nonce included, and the sender pays for what ran and gets the
//! rest of its gas and its reservoir back.
//!
//! The last tests pin the three places gas a mechanism grants a frame could leak back to a
//! caller or to the sender: an answer built without a frame, the settlement of a stopped
//! transaction, and the return of the frame's unspent gas.

use alloy_sol_types::SolError;
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP, system::keyless::KEYLESS_DEPLOY_OVERHEAD_GAS, LimitCheck,
    LimitKind, MegaLimitExceeded,
};
use revm::bytecode::opcode::{LOG0, PUSH0, REVERT};

use super::*;
use crate::common::context;

/// Init code that logs `len` bytes of data and deploys a one-byte runtime.
fn logging(len: u64) -> Bytes {
    let prefix = mega_evm::test_utils::BytecodeBuilder::default()
        .push_number(len)
        .push_number(0_u64)
        .append(LOG0)
        .build_vec();
    constructor(&prefix, &runtime(1))
}

/// Init code that fills a fresh slot and deploys a one-byte runtime.
fn filling_a_slot() -> Bytes {
    let prefix = mega_evm::test_utils::BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .build_vec();
    constructor(&prefix, &runtime(1))
}

/// The data size a transaction carrying `deployment`'s calldata counts before any frame: its
/// body.
fn body_bytes(deployment: &Deployment) -> u64 {
    reference(deployment.call_data(LARGE_OVERRIDE), GAS_LIMITS[0]).usage.data_size
}

/// Runs a `keylessDeploy` of `deployment` at `gas_limit` under `limits`.
fn limited(deployment: &Deployment, gas_limit: u64, limits: EvmTxRuntimeLimits) -> Outcome {
    run_with(system_db(), deployment.call_data(LARGE_OVERRIDE), gas_limit, limits)
}

type Outcome = MegaTransactionOutcome;

/// The stop a transaction reverted with.
fn stop(outcome: &Outcome) -> MegaLimitExceeded {
    let ExecutionResult::Revert { output, .. } = &outcome.result else {
        panic!("the transaction did not revert: {:?}", outcome.result);
    };
    MegaLimitExceeded::abi_decode(output).expect("the revert data is a limit stop")
}

/// The stop a deployment's creation reverted with, reported as its `ExecutionReverted` output.
fn creation_stop(outcome: &Outcome) -> MegaLimitExceeded {
    let KeylessDeployError::ExecutionReverted { output, .. } = failure(outcome) else {
        panic!("the creation did not revert: {:?}", outcome.result);
    };
    MegaLimitExceeded::abi_decode(&output).expect("the creation's revert data is a limit stop")
}

/// A transaction stopped inside its keyless deployment keeps nothing of it: the signer's nonce
/// is where it was, no code is deployed, no state or history gas beyond the body is spent, no
/// record is kept, and above the execution cap the whole reservoir but the body's history comes
/// back. The sender pays the overhead and what ran.
fn assert_stopped_whole(outcome: &Outcome, deployment: &Deployment, gas_limit: u64) {
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
    assert_eq!(nonce(outcome, deployment.signer), 0, "the signer's nonce is taken back");
    assert!(code_hash(outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
    let [_, regular, state, history_gas, history_bytes] = beyond(outcome, &reference);
    assert_eq!([state, history_gas, history_bytes], [0; 3], "at {gas_limit}");
    assert!(regular >= KEYLESS_DEPLOY_OVERHEAD_GAS, "the overhead stays spent");
    assert_eq!(outcome.usage.write_records, 0, "no record is kept");
    if gas_limit > TX_GAS_LIMIT_CAP {
        assert_eq!(
            outcome.result.gas().reservoir_remaining(),
            reference.result.gas().reservoir_remaining(),
            "the reservoir comes back",
        );
    }
}

/// A transaction whose body alone crosses its data-size limit is stopped before its first frame,
/// and that frame is not a deployment: the call is answered with the stop, and no rule reads the
/// signer.
#[test]
fn test_a_transaction_latched_by_its_body_deploys_nothing() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(100),
        );
        assert_eq!(stop(&outcome), MegaLimitExceeded { kind: 0, limit: 100 }, "at {gas_limit}");
        assert!(!outcome.state.contains_key(&deployment.signer), "no rule read the signer");
        let [total, ..] =
            beyond(&outcome, &reference(deployment.call_data(LARGE_OVERRIDE), gas_limit));
        assert_eq!(total, 0, "not even the overhead is charged");
    }
}

/// A log that crosses the transaction's data-size limit stops the transaction: the call reverts
/// with the stop, and the deployment is taken back whole.
#[test]
fn test_a_deployment_crossing_the_transaction_data_size_limit_stops_it() {
    let deployment = Deployment::new(logging(2_000));
    let limit = body_bytes(&deployment) + 2 * 40 + 100;
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
        );
        assert_eq!(stop(&outcome), MegaLimitExceeded { kind: 0, limit }, "at {gas_limit}");
        assert!(matches!(
            outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, frame_local: false, .. })
        ));
        assert_stopped_whole(&outcome, &deployment, gas_limit);
    }
}

/// A log that crosses the creation's own data-size budget reverts the creation alone: the call
/// reports `ExecutionReverted` with the stop, the transaction is not stopped, and the signer's
/// nonce is spent.
#[test]
fn test_a_deployment_crossing_its_data_size_budget_reverts_alone() {
    let deployment = Deployment::new(logging(2_000));
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(1_000),
        );
        // The call's own lane gets the frame cap, and its creation 98% of that.
        assert_eq!(creation_stop(&outcome), MegaLimitExceeded { kind: 0, limit: 980 });
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(nonce(&outcome, deployment.signer), 1);
        assert_eq!(outcome.usage.write_records, 1, "the signer's nonce record");
    }
}

/// The records the creation's start makes are held before the creation is built: a transaction
/// limit that holds one record but not two stops the deployment before its init code runs.
#[test]
fn test_the_creations_start_crossing_the_limit_stops_the_transaction() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let limit = body_bytes(&deployment) + 40;
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
        );
        assert_eq!(stop(&outcome), MegaLimitExceeded { kind: 0, limit });
        assert_stopped_whole(&outcome, &deployment, gas_limit);
    }
}

/// A failed creation leaves its creator's nonce record on the call's lane, which no check held it
/// to: a call whose budget cannot hold that record reverts alone with its own stop, and takes the
/// nonce back with it. The transaction is not latched.
#[test]
fn test_the_signers_record_can_put_the_call_over_its_budget() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(39),
        );
        assert_eq!(stop(&outcome), MegaLimitExceeded { kind: 0, limit: 39 });
        assert_eq!(outcome.limit_exceeded, None, "a budget, not the transaction's limit");
        assert_stopped_whole(&outcome, &deployment, gas_limit);
    }
}

/// A failed creation by a signer at nonce 1 leaves no record on the call: its nonce bump is taken
/// back, and the record with it. The same budget then holds the call, which answers with the
/// creation's own stop, and the signer stays at 1.
#[test]
fn test_a_failure_from_nonce_one_leaves_the_call_within_its_budget() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let outcome = run_with(
            system_db().account_nonce(deployment.signer, 1),
            deployment.call_data(LARGE_OVERRIDE),
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(39),
        );
        // The call's lane gets the frame cap, and its creation 98% of that.
        assert_eq!(creation_stop(&outcome), MegaLimitExceeded { kind: 0, limit: 38 });
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(nonce(&outcome, deployment.signer), 1, "at {gas_limit}");
        assert_eq!(outcome.usage.write_records, 0, "at {gas_limit}");
    }
}

/// A transaction KV limit reaches the creation as its share: 98% of what the call has left,
/// rounded down. A limit of two leaves the creation one, and the two records its start makes cross
/// it: the creation is stopped before its init code runs, alone, and the signer's nonce is spent.
/// Records come one at a time, so inside the creation its share always binds before the
/// transaction's limit does.
#[test]
fn test_a_kv_limit_below_the_creations_start_stops_the_creation_at_its_start() {
    let deployment = Deployment::new(filling_a_slot());
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(2),
        );
        assert_eq!(creation_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 1 });
        let KeylessDeployError::ExecutionReverted { gas_used, .. } = failure(&outcome) else {
            unreachable!()
        };
        assert_eq!(gas_used, 0, "the init code never ran");
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(nonce(&outcome, deployment.signer), 1, "a stopped creation spends the nonce");
        assert_eq!(outcome.usage.write_records, 1);
    }
}

/// And a KV budget of three gives the creation two records — its start's — so the slot it fills
/// reverts it alone.
#[test]
fn test_a_deployment_crossing_its_kv_budget_reverts_alone() {
    let deployment = Deployment::new(filling_a_slot());
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_frame_kv_update_limit(3),
        );
        assert_eq!(creation_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 2 });
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(nonce(&outcome, deployment.signer), 1);
    }
}

/// The state gas of the creation's start — the signer's account and the created account — is held
/// once the creation is built, as a `CREATE`'s upfront charge is: a limit below it stops the
/// transaction before the init code runs.
#[test]
fn test_the_upfront_state_gas_is_held_to_the_state_gas_limit() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let upfront = entry(GasId::new_account_state_gas()) + entry(GasId::create_state_gas());
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(upfront - 1),
        );
        assert_eq!(stop(&outcome), MegaLimitExceeded { kind: 3, limit: upfront - 1 });
        assert_stopped_whole(&outcome, &deployment, gas_limit);
    }
}

/// A slot the constructor fills counts on top of the upfront state gas.
#[test]
fn test_a_deployment_crossing_the_state_gas_limit_stops_it() {
    let deployment = Deployment::new(filling_a_slot());
    let upfront = entry(GasId::new_account_state_gas()) + entry(GasId::create_state_gas());
    let limit = upfront + entry(GasId::sstore_set_state_gas()) - 1;
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(limit),
        );
        assert_eq!(stop(&outcome), MegaLimitExceeded { kind: 3, limit });
        assert_stopped_whole(&outcome, &deployment, gas_limit);
    }
}

/// And so does the code it deposits: a limit one short of it stops the transaction before the
/// code is written.
#[test]
fn test_the_deposited_code_is_held_to_the_state_gas_limit() {
    let deployment = Deployment::new(deploying(&runtime(5)));
    let limit = entry(GasId::new_account_state_gas()) +
        entry(GasId::create_state_gas()) +
        satin_gas_params().code_deposit_state_gas(5) -
        1;
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(limit),
        );
        assert_eq!(stop(&outcome), MegaLimitExceeded { kind: 3, limit });
        assert_stopped_whole(&outcome, &deployment, gas_limit);
    }
}

/// A limit that holds the whole deployment lets it through.
#[test]
fn test_a_deployment_within_every_limit_deploys() {
    let deployment = Deployment::new(filling_a_slot());
    let upfront = entry(GasId::new_account_state_gas()) + entry(GasId::create_state_gas());
    // The creation gets 98% of what the call has left, rounded down: room for its three records
    // and its byte of code, and for its three records.
    let limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_data_size_limit(body_bytes(&deployment) + 3 * 40 + 1 + 10)
        .with_tx_kv_update_limit(4)
        .with_tx_state_gas_limit(
            upfront +
                entry(GasId::sstore_set_state_gas()) +
                satin_gas_params().code_deposit_state_gas(1),
        );
    for gas_limit in GAS_LIMITS {
        let outcome = limited(&deployment, gas_limit, limits);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address, "at {gas_limit}");
    }
}

/// An answer built without a frame carries the reservoir the call inherited: a call refused after
/// it paid for the signer's account — the forward capped below the signed gas limit, which only a
/// signed gas limit near the execution cap reaches above it — gives the charge back to the
/// reservoir and the sender gets all of it but the body's history.
#[test]
fn test_a_refusal_after_the_charges_gives_the_reservoir_back() {
    let deployment =
        Deployment::signed(0, TX_GAS_LIMIT_CAP - 50_000, U256::ZERO, deploying(&runtime(1)));
    let gas_limit = GAS_LIMITS[1];
    let outcome = limited(&deployment, gas_limit, EvmTxRuntimeLimits::no_limits());
    assert!(matches!(refusal(&outcome), KeylessDeployError::GasLimitTooLow { .. }));
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
    let [total, regular, state, history_gas, _] = beyond(&outcome, &reference);
    assert_eq!(
        [total, regular, state, history_gas],
        [KEYLESS_DEPLOY_OVERHEAD_GAS, KEYLESS_DEPLOY_OVERHEAD_GAS, 0, 0]
    );
    assert_eq!(
        outcome.result.gas().reservoir_remaining(),
        reference.result.gas().reservoir_remaining(),
    );
}

/// And a call refused after the charges of the creation's start — the forward the `CREATE`
/// opcode's regular gas takes below the signed gas limit, which above the execution cap only a
/// signed gas limit just under what the call has left reaches — gives their state and history gas
/// back to the reservoir, and keeps their regular gas, as it keeps the overhead.
#[test]
fn test_a_refusal_after_the_creations_charges_gives_the_reservoir_back() {
    let gas_limit = GAS_LIMITS[1];
    let init_code = deploying(&runtime(1));
    // Above the cap the call has what the execution cap leaves once the body's regular gas — all
    // the reference spends — and the overhead are paid. The signed gas limit is one less, and is
    // part of the calldata the body is priced by, so the two are settled together.
    let mut signed = TX_GAS_LIMIT_CAP - 200_000;
    let (deployment, left) = (0..10)
        .find_map(|_| {
            let deployment = Deployment::signed(0, signed, U256::ZERO, init_code.clone());
            let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
            let left = TX_GAS_LIMIT_CAP - reference.gas.regular - KEYLESS_DEPLOY_OVERHEAD_GAS;
            let settled = left - 1 == signed;
            signed = left - 1;
            settled.then_some((deployment, left))
        })
        .expect("the signed gas limit settles");
    let outcome = limited(&deployment, gas_limit, EvmTxRuntimeLimits::no_limits());
    let opcode = create_regular(init_code.len());
    assert_eq!(
        refusal(&outcome),
        KeylessDeployError::GasLimitTooLow {
            tx_gas_limit: signed,
            provided_gas_limit: left - opcode
        },
    );
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
    let [total, regular, state, history_gas, _] = beyond(&outcome, &reference);
    let kept = KEYLESS_DEPLOY_OVERHEAD_GAS + opcode;
    assert_eq!([total, regular, state, history_gas], [kept, kept, 0, 0]);
    assert_eq!(
        outcome.result.gas().reservoir_remaining(),
        reference.result.gas().reservoir_remaining(),
    );
}

/// The call gets back what its creation did not spend, the same way whether the creation
/// succeeds or reverts: the transaction spends the overhead, the `CREATE` opcode's regular gas,
/// what the call kept of the creation's start, and the creation's `gasUsed`, and nothing else,
/// below and above the execution cap.
#[test]
fn test_the_unspent_forward_comes_back_on_success_and_revert_alike() {
    let succeeding = Deployment::new(deploying(&runtime(1)));
    let reverting = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
    let new_account = entry(GasId::new_account_state_gas());
    for gas_limit in GAS_LIMITS {
        let success = limited(&succeeding, gas_limit, EvmTxRuntimeLimits::no_limits());
        let [total, ..] =
            beyond(&success, &reference(succeeding.call_data(LARGE_OVERRIDE), gas_limit));
        assert_eq!(
            total,
            KEYLESS_DEPLOY_OVERHEAD_GAS +
                create_regular(deploying(&runtime(1)).len()) +
                new_account +
                entry(GasId::create_state_gas()) +
                2 * record() +
                returned(&success).gasUsed,
            "success at {gas_limit}",
        );

        let revert = limited(&reverting, gas_limit, EvmTxRuntimeLimits::no_limits());
        let [total, ..] =
            beyond(&revert, &reference(reverting.call_data(LARGE_OVERRIDE), gas_limit));
        assert_eq!(
            total,
            KEYLESS_DEPLOY_OVERHEAD_GAS +
                create_regular(3) +
                new_account +
                record() +
                returned(&revert).gasUsed,
            "revert at {gas_limit}",
        );
    }
}

/// A transaction the protocol sends is held to no limit, keyless deployments included: the
/// deployment that a transaction limit stops for a user deploys for a system call.
#[test]
fn test_a_deployment_is_counted_but_not_stopped_under_the_exemption() {
    let deployment = Deployment::new(logging(2_000));
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(1);
    let mut evm = MegaEvm::new(context(system_db()).with_tx_runtime_limits(limits));
    let outcome = alloy_evm::Evm::transact_system_call(
        &mut evm,
        mega_evm::system::MEGA_SYSTEM_ADDRESS,
        KEYLESS_DEPLOY_ADDRESS,
        deployment.call_data(LARGE_OVERRIDE),
    )
    .expect("a system call runs");
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("the system call did not succeed: {:?}", outcome.result);
    };
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data()).unwrap();
    assert_eq!(ret.deployedAddress, deployment.address);
}

use revm::primitives::KECCAK_EMPTY;

/// Runs a `keylessDeploy` of `deployment` over `db` at `gas_limit` under `limits`, with the
/// signer's SALT bucket at `m_signer` times the minimum and the deploy address's at `m_address`.
fn crowded_run(
    db: MemoryDatabase,
    deployment: &Deployment,
    gas_limit: u64,
    limits: EvmTxRuntimeLimits,
    (m_signer, m_address): (u64, u64),
) -> Outcome {
    let envs = crowded(TestExternalEnvs::new(), deployment.signer, m_signer);
    let envs = crowded(envs, deployment.address, m_address);
    let context = MegaContext::<_, TestExternalEnvs<String>>::new_with_external_envs(
        db,
        MegaSpecId::SATIN,
        ExternalEnvs { salt_env: envs.clone(), oracle_env: envs },
    )
    .with_block(block())
    .with_chain(mega_evm::test_utils::zero_fee_l1_block_info())
    .with_tx_runtime_limits(limits);
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    MegaEvm::new(context).execute_transaction(tx).expect("the transaction is valid")
}

/// The state-gas limit holds each upfront charge of a deployment once, at the price its bucket
/// sets: the signer's account the call pays for, and the created account the creation's start
/// holds, as the frame-start hold holds any `CREATE`'s. With the signer's bucket at twice the
/// minimum and the deploy address's at four times, a deployment holds `2 ×` the new account, `4 ×`
/// the created account and `4 ×` its deposited byte. A limit of exactly that deploys, as without a
/// limit; one gas less stops the transaction. A signer that has an account pays for none, and the
/// limit that holds the rest deploys it.
#[test]
fn test_the_upfront_charges_are_held_once_at_their_bucket_prices() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    assert_ne!(bucket(deployment.signer), bucket(deployment.address));
    let created = entry(GasId::create_state_gas()) + satin_gas_params().code_deposit_state_gas(1);
    let held = 2 * entry(GasId::new_account_state_gas()) + 4 * created;
    let limited = |db: MemoryDatabase, gas_limit, limit| {
        let limits = EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(limit);
        crowded_run(db, &deployment, gas_limit, limits, (2, 4))
    };
    for gas_limit in GAS_LIMITS {
        let unlimited = crowded_run(
            system_db(),
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits(),
            (2, 4),
        );
        assert_eq!(unlimited.gas.state, held, "at {gas_limit}");

        let exact = limited(system_db(), gas_limit, held);
        assert_eq!(returned(&exact).deployedAddress, deployment.address, "at {gas_limit}");
        assert_eq!(exact.gas, unlimited.gas);
        let short = limited(system_db(), gas_limit, held - 1);
        assert_eq!(stop(&short), MegaLimitExceeded { kind: 3, limit: held - 1 });
        assert_stopped_whole(&short, &deployment, gas_limit);

        let funded = db_for(&deployment, U256::from(1));
        let exact = limited(funded.clone(), gas_limit, 4 * created);
        assert_eq!(returned(&exact).deployedAddress, deployment.address, "at {gas_limit}");
        assert_eq!(exact.gas.state, 4 * created);
        let short = limited(funded, gas_limit, 4 * created - 1);
        assert_eq!(stop(&short), MegaLimitExceeded { kind: 3, limit: 4 * created - 1 });
    }
}
