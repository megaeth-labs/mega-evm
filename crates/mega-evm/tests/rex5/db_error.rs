//! REX5 DB-error paths added by the system-tx validation and the keyless deploy
//! sandbox rework.
//!
//! Two failure modes need pinning:
//!
//! - **System-tx validate** (`evm/execution.rs`): the new pre-deposit `inspect_account` on
//!   `MEGA_SYSTEM_ADDRESS` can surface a database error. The `map_err` must wrap it as
//!   `EVMError::Custom` so OP handler returns the canonical fatal-external shape rather than
//!   silently dropping the system tx.
//! - **Keyless deploy step 7** (`sandbox/execution.rs`): the `basic(deploy_address)` read before
//!   sandbox setup can surface a database error. The `map_err` must wrap it as the selector-only
//!   `KeylessDeployError::InternalError` and surface as `Revert` (validation-style — no sandbox
//!   state to merge).

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    revm::context::result::ExecutionResult,
    sandbox::{
        decode_error_result,
        tests::{CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER, CREATE2_FACTORY_TX},
        KeylessDeployError,
    },
    test_utils::{ErrorInjectingDatabase, MemoryDatabase},
    EVMError, IKeylessDeploy, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, TestExternalEnvs,
    KEYLESS_DEPLOY_ADDRESS, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use revm::{context::TxEnv, inspector::NoOpInspector};

const RELAYER: Address = address!("0000000000000000000000000000000000990000");

/// REX5+ keyless deploy must reject with `InternalError` (selector-only revert) when the
/// pre-sandbox `basic(deploy_address)` DB read fails. The signer MUST NOT be charged
/// and no replay barrier may be installed, because the sandbox never ran.
#[test]
fn test_keyless_deploy_address_db_error_maps_to_internal_error_revert() {
    let mut inner = MemoryDatabase::default();
    inner.set_account_balance(
        CREATE2_FACTORY_DEPLOYER,
        U256::from(1_000_000_000_000_000_000_000u128),
    );
    inner.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let mut db = ErrorInjectingDatabase::new(inner);
    // Step 7 of `execute_keyless_deploy_call` does `journal.database.basic(deploy_address)`
    // to ensure the deploy address has no code. Fail that specific read.
    db.fail_on_account = Some(CREATE2_FACTORY_CONTRACT);

    let external_envs = TestExternalEnvs::<Infallible>::new();
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(CREATE2_FACTORY_TX),
        gasLimitOverride: U256::from(10_000_000u64),
    }
    .abi_encode();

    let mut context =
        MegaContext::new(db, MegaSpecId::REX5).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let tx = TxEnv {
        caller: RELAYER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: 30_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let res = alloy_evm::Evm::transact(&mut evm, tx).expect("outer EVM error not expected");

    let revert_output = match res.result {
        ExecutionResult::Revert { output, .. } => output,
        other => panic!("expected outer Revert, got {other:?}"),
    };
    let decoded = decode_error_result(&revert_output)
        .expect("revert payload must decode to a known KeylessDeployError");
    assert!(
        matches!(decoded, KeylessDeployError::InternalError),
        "DB read failure on deploy address must surface as selector-only InternalError, got {decoded:?}",
    );

    // Signer untouched: the pre-sandbox failure path is validation-shaped, so the signer
    // must not be fee-debited and the nonce must remain 0.
    let signer_after =
        res.state.get(&CREATE2_FACTORY_DEPLOYER).cloned().map(|acc| acc.info).unwrap_or_default();
    assert_eq!(
        signer_after.nonce, 0,
        "DB error before sandbox must not bump signer nonce (no replay barrier)",
    );
}

/// REX5+ system-tx validate must surface a DB failure as `EVMError::Custom`. The
/// `inspect_account` call on `MEGA_SYSTEM_ADDRESS` happens *before* the deposit
/// promotion, so a DB blip there must NOT silently let the tx through as a deposit.
#[test]
fn test_system_tx_validate_inspect_account_db_error_surfaces_as_custom() {
    let mut inner = MemoryDatabase::default();
    // Give system address a balance so the test doesn't trip on InsufficientBalance
    // before reaching the inspect_account check.
    inner.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(10u128.pow(18)));

    let mut db = ErrorInjectingDatabase::new(inner);
    // Fail when `validate_tx_against_state` calls `inspect_account(MEGA_SYSTEM_ADDRESS)`.
    db.fail_on_account = Some(MEGA_SYSTEM_ADDRESS);

    let mut context = MegaContext::new(db, MegaSpecId::REX5);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    // `CfgEnv::default().chain_id` is 1; `MegaContext::new` does not override it.
    let chain_id = revm::context::ContextTr::cfg(&context).chain_id;

    // Whitelisted callee — required for the system-tx path to even consider the
    // chain-id / nonce / inspect_account checks. With a non-whitelisted callee the
    // request gets rejected up-front as "callee not in the whitelist" before our
    // injected DB error has a chance to fire.
    let tx = TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit: 1_000_000,
        gas_price: 0,
        chain_id: Some(chain_id),
        nonce: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let res = alloy_evm::Evm::transact_raw(&mut evm, tx);

    match res {
        Err(EVMError::Custom(msg)) => {
            assert!(
                msg.contains("Mega system transaction state read failed"),
                "expected wrapped DB error, got: {msg}",
            );
        }
        other => panic!(
            "expected EVMError::Custom for system-tx inspect_account DB error, got {other:?}"
        ),
    }
}
