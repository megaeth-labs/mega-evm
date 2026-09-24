//! The rules a `keylessDeploy` call is held to before its creation starts, and what a refusal
//! leaves behind: nothing but the overhead.
//!
//! The rules and the error ABI are the legacy engine's. What moved is the balance rule, which
//! asks the signer for the transaction's value alone — a deployment pays no gas out of the
//! signer's balance — and rule 4, which now counts real nonces: the creation bumps the signer's
//! nonce whether it succeeds or not, so a signer gets two attempts that fail and not a third.

use alloy_primitives::{address, keccak256, Signature};
use mega_evm::{
    alloy_consensus::TxEip1559,
    constants::MAX_INITCODE_SIZE,
    system::keyless::{
        tests::{
            CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER, CREATE2_FACTORY_TX,
            NON_CONTRACT_CREATION_TX, POST_EIP155_CHAIN_1_TX,
        },
        KEYLESS_DEPLOY_OVERHEAD_GAS,
    },
    test_utils::{ErrorInjectingDatabase, InjectedDbError},
    MegaSpecId, MegaTransactionError, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{PUSH0, REVERT},
    context::{result::EVMError, CfgEnv},
    context_interface::cfg::GasId,
    state::{AccountInfo, Bytecode},
};

use super::*;

/// A `keylessDeploy` of `tx` with `gas_limit_override`, over `db`, at `gas_limit`.
fn submit(db: MemoryDatabase, tx: &[u8], gas_limit_override: u64, gas_limit: u64) -> Outcome {
    run_with(
        db,
        keyless_deploy_call(tx, U256::from(gas_limit_override)),
        gas_limit,
        EvmTxRuntimeLimits::no_limits(),
    )
}

type Outcome = MegaTransactionOutcome;

/// A refusal writes nothing: the signer's nonce is where it was and the deploy address holds no
/// code.
fn assert_nothing_written(outcome: &Outcome, deployment: &Deployment, signer_nonce: u64) {
    assert_eq!(nonce(outcome, deployment.signer), signer_nonce, "the signer's nonce");
    assert!(
        code_hash(outcome, deployment.address)
            .is_none_or(|hash| hash == revm::primitives::KECCAK_EMPTY),
        "the deploy address holds no code",
    );
}

/// A deployment of a one-byte runtime, which survives the empty-code check.
fn small() -> Deployment {
    Deployment::new(deploying(&runtime(1)))
}

/// Rule 1: the call carries no value. A deployment is made by its signer, and the contract holds
/// no balance.
#[test]
fn test_keyless_deploy_rejects_ether_transfer() {
    let deployment = small();
    for gas_limit in GAS_LIMITS {
        let mut tx =
            call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ONE);
        tx.0.base.gas_limit = gas_limit;
        let outcome = MegaEvm::new(context(system_db()))
            .execute_transaction(tx)
            .expect("a valid transaction");
        assert_eq!(refusal(&outcome), KeylessDeployError::NoEtherTransfer, "at {gas_limit}");
        assert_nothing_written(&outcome, &deployment, 0);
    }
}

/// Rule 2: `gasLimitOverride` covers the gas limit the transaction was signed with.
#[test]
fn test_keyless_deploy_gas_limit_too_low() {
    let outcome = submit(system_db(), CREATE2_FACTORY_TX, 99_999, GAS_LIMITS[0]);
    assert_eq!(
        refusal(&outcome),
        KeylessDeployError::GasLimitTooLow { tx_gas_limit: 100_000, provided_gas_limit: 99_999 },
    );
}

/// An override equal to the signed gas limit passes rule 2, and the creation runs with exactly
/// that much. The canonical `CREATE2` factory was signed for a chain that charges no state gas
/// for deployed code: without a reservoir its 100,000 gas does not cover the code it deposits,
/// and the creation runs out of gas; above the execution cap the reservoir pays that state gas,
/// and the same gas limit deploys it.
#[test]
fn test_keyless_deploy_gas_limit_exactly_equal() {
    let narrow = submit(system_db(), CREATE2_FACTORY_TX, 100_000, GAS_LIMITS[0]);
    let KeylessDeployError::ExecutionHalted { gas_used, .. } = failure(&narrow) else {
        panic!("expected the creation to run out of gas: {:?}", narrow.result);
    };
    assert_eq!(gas_used, 100_000, "the creation ran on the signed gas limit, and spent it");
    assert_eq!(nonce(&narrow, CREATE2_FACTORY_DEPLOYER), 1, "the nonce is spent all the same");

    let wide = submit(system_db(), CREATE2_FACTORY_TX, 100_000, GAS_LIMITS[1]);
    assert_eq!(returned(&wide).deployedAddress, CREATE2_FACTORY_CONTRACT);
}

/// The gas limit at which a `keylessDeploy` of `deployment` leaves the call `forward` to forward
/// to its creation, below the execution cap, for a signer that has an account and a deploy
/// address that is empty: the reference transaction's spend, the overhead, the created account
/// and the two records, and the forward.
fn gas_limit_forwarding(deployment: &Deployment, forward: u64) -> u64 {
    let intrinsic = reference(deployment.call_data(LARGE_OVERRIDE), GAS_LIMITS[0])
        .result
        .gas()
        .total_gas_spent();
    intrinsic +
        KEYLESS_DEPLOY_OVERHEAD_GAS +
        entry(GasId::create_state_gas()) +
        2 * record() +
        forward
}

/// Rule 2 again, once the call has paid for the creation's start: the override is capped to what
/// the call has left, and the capped gas must still cover the signed gas limit. A relayer that
/// sizes its transaction below that is refused, with the capped figure.
#[test]
fn test_a_forward_capped_below_the_signed_gas_limit_is_refused() {
    let deployment = Deployment::signed(0, 500_000, U256::ZERO, deploying(&runtime(1)));
    let gas_limit = gas_limit_forwarding(&deployment, 499_999);
    let outcome = submit(db_for(&deployment, U256::ONE), &deployment.tx, LARGE_OVERRIDE, gas_limit);
    assert_eq!(
        refusal(&outcome),
        KeylessDeployError::GasLimitTooLow { tx_gas_limit: 500_000, provided_gas_limit: 499_999 },
    );
    assert_nothing_written(&outcome, &deployment, 0);
}

/// A transaction that leaves the call exactly the signed gas limit deploys.
#[test]
fn test_a_forward_at_the_signed_gas_limit_deploys() {
    let deployment = Deployment::signed(0, 500_000, U256::ZERO, deploying(&runtime(1)));
    let gas_limit = gas_limit_forwarding(&deployment, 500_000);
    let outcome = submit(db_for(&deployment, U256::ONE), &deployment.tx, LARGE_OVERRIDE, gas_limit);
    assert_eq!(returned(&outcome).deployedAddress, deployment.address);
}

/// The charge for the signer's account comes out of the gas the creation would be forwarded when
/// the transaction has no reservoir, so it can take the forward below the signed gas limit: the
/// same transaction deploys for a signer that has an account and is refused for one that has
/// none. Above the execution cap the reservoir pays the charge, and both deploy.
#[test]
fn test_the_signers_account_can_take_the_forward_below_the_signed_gas_limit() {
    let deployment = Deployment::signed(0, 500_000, U256::ZERO, deploying(&runtime(1)));
    let new_account = entry(GasId::new_account_state_gas());
    let gas_limit = gas_limit_forwarding(&deployment, 500_000 + new_account / 2);

    let funded = submit(db_for(&deployment, U256::ONE), &deployment.tx, LARGE_OVERRIDE, gas_limit);
    assert_eq!(returned(&funded).deployedAddress, deployment.address, "the control deploys");
    let empty = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, gas_limit);
    assert_eq!(
        refusal(&empty),
        KeylessDeployError::GasLimitTooLow {
            tx_gas_limit: 500_000,
            provided_gas_limit: 500_000 + new_account / 2 - new_account,
        },
    );
    assert_nothing_written(&empty, &deployment, 0);

    let wide = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[1]);
    assert_eq!(returned(&wide).deployedAddress, deployment.address, "the reservoir pays");
}

/// Rule 3: the signer is recovered from the signature. `r` above the curve order recovers
/// nothing.
#[test]
fn test_keyless_deploy_invalid_signature() {
    let mut corrupted = CREATE2_FACTORY_TX.to_vec();
    corrupted[102..134].fill(0xff);
    let outcome = submit(system_db(), &corrupted, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::InvalidSignature);
}

/// So does `r = 0`, which no signature has.
#[test]
fn test_keyless_deploy_rejects_corrupted_signature() {
    let signed = mega_evm::system::keyless::decode_keyless_tx(CREATE2_FACTORY_TX).unwrap();
    let s = signed.signature().s();
    let corrupted = Signed::new_unchecked(
        signed.tx().clone(),
        Signature::new(U256::ZERO, s, false),
        B256::ZERO,
    );
    let mut encoded = Vec::new();
    corrupted.rlp_encode(&mut encoded);
    let outcome = submit(system_db(), &encoded, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::InvalidSignature);
}

/// A different `s` is a different signer, not an invalid signature: the deployment goes to that
/// signer's address, never to the canonical one.
#[test]
fn test_keyless_deploy_modified_s_changes_signer() {
    let mut modified = CREATE2_FACTORY_TX.to_vec();
    modified[135..167].fill(0x33);
    let outcome = submit(system_db(), &modified, LARGE_OVERRIDE, GAS_LIMITS[0]);
    let deployed = returned(&outcome).deployedAddress;
    assert_ne!(deployed, CREATE2_FACTORY_CONTRACT);
    assert_ne!(deployed, Address::ZERO);
    assert_eq!(nonce(&outcome, CREATE2_FACTORY_DEPLOYER), 0, "the canonical signer is untouched");
}

/// The transaction must be RLP a legacy transaction decodes from.
#[test]
fn test_keyless_deploy_malformed_encoding() {
    for garbage in [&[0xde, 0xad, 0xbe, 0xef][..], &[], &CREATE2_FACTORY_TX[..100]] {
        let outcome = submit(system_db(), garbage, LARGE_OVERRIDE, GAS_LIMITS[0]);
        assert_eq!(refusal(&outcome), KeylessDeployError::MalformedEncoding, "{garbage:02x?}");
    }
    let mut trailing = CREATE2_FACTORY_TX.to_vec();
    trailing.push(0x00);
    let outcome = submit(system_db(), &trailing, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::MalformedEncoding, "a trailing byte");
}

/// An EIP-2718 typed envelope is not a legacy transaction.
#[test]
fn test_keyless_deploy_rejects_eip2718_typed_envelope() {
    let tx = TxEip1559 {
        chain_id: 1,
        nonce: 0,
        gas_limit: 100_000,
        max_fee_per_gas: 100_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Create,
        value: U256::ZERO,
        input: Bytes::from_static(&[0x60, 0x80, 0x60, 0x40, 0x52]),
        access_list: Default::default(),
    };
    let signed = Signed::new_unchecked(
        tx,
        Signature::new(U256::from(0x1234), U256::from(0x5678), false),
        B256::ZERO,
    );
    let mut encoded = vec![0x02];
    signed.rlp_encode(&mut encoded);
    let outcome = submit(system_db(), &encoded, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::MalformedEncoding);
}

/// The transaction must be a creation.
#[test]
fn test_keyless_deploy_not_contract_creation() {
    let outcome = submit(system_db(), NON_CONTRACT_CREATION_TX, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::NotContractCreation);
}

/// The transaction must carry no chain id: that is what makes it valid on every chain.
#[test]
fn test_keyless_deploy_not_pre_eip155() {
    let outcome = submit(system_db(), POST_EIP155_CHAIN_1_TX, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::NotPreEIP155);
}

/// The transaction must be signed at nonce 0: the deploy address is the signer's first.
#[test]
fn test_keyless_deploy_non_zero_tx_nonce() {
    let deployment = Deployment::signed(1, SIGNED_GAS_LIMIT, U256::ZERO, deploying(&runtime(1)));
    let outcome = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::NonZeroTxNonce { tx_nonce: 1 });
    assert_nothing_written(&outcome, &deployment, 0);
}

/// Rule 8: the init code is within the initcode size limit the spec fixes.
#[test]
fn test_init_code_over_the_limit_is_refused() {
    let deployment = Deployment::new(vec![0; MAX_INITCODE_SIZE + 1].into());
    let outcome = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, 2_000_000_000);
    assert_eq!(
        refusal(&outcome),
        KeylessDeployError::InitCodeTooLarge {
            size: MAX_INITCODE_SIZE as u64 + 1,
            max: MAX_INITCODE_SIZE as u64,
        },
    );
    assert_nothing_written(&outcome, &deployment, 0);
}

/// Rule 4: the signer's nonce is at most 1.
#[test]
fn test_keyless_deploy_signer_nonce_too_high() {
    let deployment = small();
    let db = system_db().account_nonce(deployment.signer, 2);
    let outcome = submit(db, &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::SignerNonceTooHigh { signer_nonce: 2 });
    assert_nothing_written(&outcome, &deployment, 2);
}

/// Rule 4 counts the creation's own nonce bumps: a signer whose deployment fails twice — the
/// first bump takes it from 0 to 1, the second from 1 to 2 — is refused the third time. The
/// deploy address stays the same through all three.
#[test]
fn test_satin_caps_a_signer_at_two_failing_attempts() {
    let deployment = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
    let mut db = system_db();
    for attempt in 1..=2 {
        let outcome = run_nth(
            db.clone(),
            deployment.call_data(LARGE_OVERRIDE),
            GAS_LIMITS[0],
            EvmTxRuntimeLimits::no_limits(),
            attempt - 1,
        );
        assert!(
            matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }),
            "attempt {attempt}",
        );
        assert_eq!(nonce(&outcome, deployment.signer), attempt, "attempt {attempt}");
        revm::DatabaseCommit::commit(&mut db, outcome.result_and_state.state);
    }
    let third = run_nth(
        db,
        deployment.call_data(LARGE_OVERRIDE),
        GAS_LIMITS[0],
        EvmTxRuntimeLimits::no_limits(),
        2,
    );
    assert_eq!(refusal(&third), KeylessDeployError::SignerNonceTooHigh { signer_nonce: 2 });
}

/// Rule 9: the signer has no code — EIP-3607 for a sender that is not the transaction's.
#[test]
fn test_a_signer_with_code_is_refused() {
    let deployment = small();
    let db = system_db().account_code(deployment.signer, Bytes::from_static(&[0x00]));
    let outcome = submit(db, &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::SignerHasCode);
    assert_nothing_written(&outcome, &deployment, 0);
}

/// A chain that disables EIP-3607 disables rule 9 with it, as it does for its own transactions.
#[test]
fn test_disabling_eip3607_disables_the_signer_code_rule() {
    let deployment = small();
    let db = system_db().account_code(deployment.signer, Bytes::from_static(&[0x00]));
    let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
    cfg.disable_eip3607 = true;
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
    tx.0.base.gas_limit = GAS_LIMITS[0];
    let outcome = MegaEvm::new(context(db).with_cfg(cfg))
        .execute_transaction(tx)
        .expect("a valid transaction");
    assert_eq!(returned(&outcome).deployedAddress, deployment.address);
}

/// A chain that disables EIP-3607 does not read the signer's code at all: a node missing the
/// signer's bytecode deploys, where one that holds the rule fails on the read.
#[test]
fn test_a_disabled_eip3607_reads_no_signer_code() {
    let deployment = small();
    let code = Bytes::from_static(&[0x00]);
    let code_hash = keccak256(&code);
    let db = || {
        let mut db = ErrorInjectingDatabase::new(
            system_db().account_lazy_code(deployment.signer, code_hash),
        );
        db.fail_on_code_by_hash = Some(code_hash);
        db
    };
    let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
    cfg.disable_eip3607 = true;
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
    tx.0.base.gas_limit = GAS_LIMITS[0];
    let outcome = MegaEvm::new(context(db()).with_cfg(cfg))
        .execute_transaction(tx)
        .expect("the signer's code is never read");
    assert_eq!(returned(&outcome).deployedAddress, deployment.address);

    assert_db_error(run_on(db(), &deployment.tx), "injected code_by_hash() error");
}

/// An EIP-7702 delegation is not code: a signer that delegated deploys.
#[test]
fn test_a_delegated_signer_deploys() {
    let deployment = small();
    let delegation = Bytecode::new_eip7702(address!("0x00000000000000000000000000000000000d1e6a"));
    let mut db = system_db();
    db.insert_account_info(
        deployment.signer,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: delegation.hash_slow(),
            code: Some(delegation),
            ..Default::default()
        },
    );
    let outcome = submit(db, &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(returned(&outcome).deployedAddress, deployment.address);
    assert_eq!(nonce(&outcome, deployment.signer), 2);
}

/// Rule 5: the deploy address holds no code.
#[test]
fn test_keyless_deploy_contract_already_exists() {
    let db = system_db().account_code(CREATE2_FACTORY_CONTRACT, Bytes::from_static(&[0x60, 0x00]));
    for gas_limit in GAS_LIMITS {
        let outcome = submit(db.clone(), CREATE2_FACTORY_TX, LARGE_OVERRIDE, gas_limit);
        assert_eq!(refusal(&outcome), KeylessDeployError::ContractAlreadyExists, "at {gas_limit}");
        assert_eq!(nonce(&outcome, CREATE2_FACTORY_DEPLOYER), 0);
    }
}

/// The occupancy read goes through the journal, so the deploy address is in the transaction's
/// state — the witness a stateless client replays the refusal from — even though the call
/// reverts.
#[test]
fn test_keyless_deploy_contract_already_exists_deploy_address_in_witness_rex6() {
    let db = system_db().account_code(CREATE2_FACTORY_CONTRACT, Bytes::from_static(&[0x60, 0x00]));
    let outcome = submit(db, CREATE2_FACTORY_TX, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::ContractAlreadyExists);
    assert!(outcome.state.contains_key(&CREATE2_FACTORY_CONTRACT));
}

/// A second deployment of the same transaction finds its address taken.
#[test]
fn test_keyless_deploy_twice_fails_second_time() {
    let mut db = system_db();
    let first = submit(db.clone(), CREATE2_FACTORY_TX, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(returned(&first).deployedAddress, CREATE2_FACTORY_CONTRACT);
    revm::DatabaseCommit::commit(&mut db, first.result_and_state.state);
    let data = keyless_deploy_call(CREATE2_FACTORY_TX, U256::from(LARGE_OVERRIDE));
    let second = run_nth(db, data, GAS_LIMITS[0], EvmTxRuntimeLimits::no_limits(), 1);
    assert_eq!(refusal(&second), KeylessDeployError::ContractAlreadyExists);
}

/// Rule 6: the signer funds the transaction's value, and nothing else. A signer with nothing is
/// refused a deployment that carries a value.
#[test]
fn test_keyless_deploy_insufficient_balance_zero() {
    let deployment = Deployment::with_value(deploying(&runtime(1)), U256::from(1));
    let outcome = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::InsufficientBalance);
    assert_nothing_written(&outcome, &deployment, 0);
}

/// A signer holding part of the value is refused too.
#[test]
fn test_keyless_deploy_insufficient_balance_partial() {
    let deployment = Deployment::with_value(deploying(&runtime(1)), U256::from(10));
    let outcome =
        submit(db_for(&deployment, U256::from(9)), &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(refusal(&outcome), KeylessDeployError::InsufficientBalance);
}

/// A signer holding exactly the value deploys, and the value moves to the contract.
#[test]
fn test_keyless_deploy_balance_exactly_sufficient() {
    let deployment = Deployment::with_value(deploying(&runtime(1)), U256::from(10));
    let outcome =
        submit(db_for(&deployment, U256::from(10)), &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert_eq!(returned(&outcome).deployedAddress, deployment.address);
    let balance = |address| outcome.state.get(&address).map(|account| account.info.balance);
    assert_eq!(balance(deployment.signer), Some(U256::ZERO));
    assert_eq!(balance(deployment.address), Some(U256::from(10)));
}

/// A deployment pays no gas out of the signer's balance: a signer with nothing deploys a
/// deployment that carries no value, whatever gas price it was signed at.
#[test]
fn test_a_signer_with_nothing_deploys_what_carries_no_value() {
    let deployment = small();
    for gas_limit in GAS_LIMITS {
        let outcome = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address, "at {gas_limit}");
        assert_eq!(
            outcome.state.get(&deployment.signer).map(|account| account.info.balance),
            Some(U256::ZERO),
        );
    }
}

/// The value is real, so it must be funded.
#[test]
fn test_the_value_must_be_funded() {
    let deployment = Deployment::with_value(deploying(&runtime(1)), U256::from(1_000));
    for gas_limit in GAS_LIMITS {
        let db = db_for(&deployment, U256::from(999));
        let outcome = submit(db, &deployment.tx, LARGE_OVERRIDE, gas_limit);
        assert_eq!(refusal(&outcome), KeylessDeployError::InsufficientBalance, "at {gas_limit}");
        assert_nothing_written(&outcome, &deployment, 0);
    }
}

/// Rule 7: a call that cannot pay the overhead runs out of gas at once, before any rule reads the
/// state: the signer is untouched.
#[test]
fn test_a_call_below_the_overhead_reads_nothing() {
    let deployment = small();
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), GAS_LIMITS[0]);
    let intrinsic = reference.result.gas().total_gas_spent();
    let gas_limit = intrinsic + KEYLESS_DEPLOY_OVERHEAD_GAS - 1;
    let outcome = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, gas_limit);
    assert!(matches!(outcome.result, ExecutionResult::Halt { .. }), "{:?}", outcome.result);
    assert_eq!(outcome.result.gas().tx_gas_used(), gas_limit);
    assert!(!outcome.state.contains_key(&deployment.signer), "no rule read the signer");
}

/// A call that cannot pay for the signer's account runs out of gas before the creation starts:
/// no nonce is bumped and nothing is deployed.
#[test]
fn test_a_call_that_cannot_pay_the_signers_account_runs_out_of_gas() {
    let deployment = small();
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), GAS_LIMITS[0]);
    let intrinsic = reference.result.gas().total_gas_spent();
    let gas_limit = intrinsic + KEYLESS_DEPLOY_OVERHEAD_GAS + 1_000;
    let outcome = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, gas_limit);
    assert!(matches!(outcome.result, ExecutionResult::Halt { .. }), "{:?}", outcome.result);
    assert_nothing_written(&outcome, &deployment, 0);
    assert_eq!(outcome.gas.state, 0, "the halt takes the charges back");
}

/// A call refused after it paid for the creation's start — the forward capped below the signed
/// gas limit — keeps none of it: no account was added, so the signer's account is not charged,
/// and the refusal writes nothing.
#[test]
fn test_a_call_refused_after_its_charges_keeps_none_of_them() {
    let deployment = Deployment::signed(0, 500_000, U256::ZERO, deploying(&runtime(1)));
    // Enough for everything but the signer's account, which the empty signer adds.
    let gas_limit = gas_limit_forwarding(&deployment, 500_000);
    let outcome = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, gas_limit);
    assert!(matches!(refusal(&outcome), KeylessDeployError::GasLimitTooLow { .. }));
    let [total, regular, state, history_gas, _] =
        beyond(&outcome, &reference(deployment.call_data(LARGE_OVERRIDE), gas_limit));
    assert_eq!(
        [total, regular, state, history_gas],
        [KEYLESS_DEPLOY_OVERHEAD_GAS, KEYLESS_DEPLOY_OVERHEAD_GAS, 0, 0],
    );
    assert_nothing_written(&outcome, &deployment, 0);
}

/// The signer's account is charged exactly when its nonce is spent: a refused call does neither,
/// a deployment that fails at run time does both.
#[test]
fn test_the_signers_account_is_charged_exactly_when_its_nonce_is_spent() {
    let deployment =
        Deployment::signed(0, 500_000, U256::ZERO, Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
    let refused = submit(
        system_db(),
        &deployment.tx,
        LARGE_OVERRIDE,
        gas_limit_forwarding(&deployment, 500_000),
    );
    assert!(matches!(refusal(&refused), KeylessDeployError::GasLimitTooLow { .. }));
    assert_eq!(nonce(&refused, deployment.signer), 0);
    assert_eq!(refused.gas.state, 0);

    let failed = submit(system_db(), &deployment.tx, LARGE_OVERRIDE, GAS_LIMITS[0]);
    assert!(matches!(failure(&failed), KeylessDeployError::ExecutionReverted { .. }));
    assert_eq!(nonce(&failed, deployment.signer), 1);
    assert_eq!(failed.gas.state, entry(GasId::new_account_state_gas()));
}

/// A database read a rule needs fails the transaction with the database's error: the deploy
/// address's, whose occupancy rule 5 reads.
#[test]
fn test_a_failed_read_of_the_deploy_address_fails_the_transaction() {
    let mut db = ErrorInjectingDatabase::new(system_db());
    db.fail_on_account = Some(CREATE2_FACTORY_CONTRACT);
    assert_db_error(run_on(db, CREATE2_FACTORY_TX), "injected basic() error");
}

/// The occupancy read never loads the deploy address's code, so a node that has the account but
/// not its bytecode — a stateless client's witness — still refuses an occupied address with
/// `ContractAlreadyExists`, rather than failing.
#[test]
fn test_the_occupancy_read_needs_no_code() {
    let code = Bytes::from_static(&[0x60, 0x00]);
    let code_hash = keccak256(&code);
    let mut db = ErrorInjectingDatabase::new(
        system_db().account_lazy_code(CREATE2_FACTORY_CONTRACT, code_hash),
    );
    db.fail_on_code_by_hash = Some(code_hash);
    let outcome = run_on(db, CREATE2_FACTORY_TX).expect("the occupancy read needs no code");
    assert_eq!(refusal(&outcome), KeylessDeployError::ContractAlreadyExists);
}

/// The signer's read, which rules 4 and 9 need, fails the transaction the same way.
#[test]
fn test_a_failed_read_of_the_signer_fails_the_transaction() {
    let mut db = ErrorInjectingDatabase::new(system_db());
    db.fail_on_account = Some(CREATE2_FACTORY_DEPLOYER);
    assert_db_error(run_on(db, CREATE2_FACTORY_TX), "injected basic() error");
}

/// A `keylessDeploy` of `tx` over `db`, at the narrow gas limit.
fn run_on(
    db: ErrorInjectingDatabase,
    tx: &[u8],
) -> Result<MegaTransactionOutcome, EVMError<InjectedDbError, MegaTransactionError>> {
    let mut call = call_tx(
        KEYLESS_DEPLOY_ADDRESS,
        keyless_deploy_call(tx, U256::from(LARGE_OVERRIDE)),
        U256::ZERO,
    );
    call.0.base.gas_limit = GAS_LIMITS[0];
    MegaEvm::new(context(db)).execute_transaction(call)
}

/// Requires `result` to be the database error `expected` names.
fn assert_db_error(
    result: Result<MegaTransactionOutcome, EVMError<InjectedDbError, MegaTransactionError>>,
    expected: &str,
) {
    match result {
        Err(EVMError::Database(error)) => {
            assert!(error.to_string().contains(expected), "got: {error}");
        }
        other => {
            panic!("expected a database error, got: {:?}", other.map(|o| o.result_and_state.result))
        }
    }
}

/// A SALT lookup a charge needs fails the transaction with the cause the environment reported,
/// for either of the two accounts the call charges: the price is never guessed.
#[test]
fn test_a_failed_salt_lookup_fails_the_transaction() {
    let deployment = small();
    for failing in [deployment.signer, deployment.address] {
        let bucket =
            <TestExternalEnvs<String> as mega_evm::SaltEnv>::bucket_id_for_account(failing);
        let envs =
            TestExternalEnvs::<String>::new().with_failing_bucket(bucket, "salt down".into());
        let context = mega_evm::MegaContext::<_, TestExternalEnvs<String>>::new_with_external_envs(
            system_db(),
            MegaSpecId::SATIN,
            mega_evm::ExternalEnvs { salt_env: envs.clone(), oracle_env: envs },
        )
        .with_block(crate::common::block())
        .with_chain(mega_evm::test_utils::zero_fee_l1_block_info());
        let mut tx =
            call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
        tx.0.base.gas_limit = GAS_LIMITS[0];
        match MegaEvm::new(context).execute_transaction(tx) {
            Err(EVMError::Custom(message)) => assert!(message.contains("salt down"), "{message}"),
            other => panic!(
                "expected the SALT failure, got {:?}",
                other.map(|o| o.result_and_state.result)
            ),
        }
    }
}
