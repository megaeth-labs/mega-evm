//! REX5 DB-error paths added by the system-tx validation and the keyless deploy
//! sandbox rework.
//!
//! Two failure modes need pinning:
//!
//! - **System-tx validate** (`evm/execution.rs`): the new pre-deposit `inspect_account` on
//!   `MEGA_SYSTEM_ADDRESS` can surface a database error. The `map_err` must wrap it as
//!   `EVMError::Custom` so OP handler returns the canonical fatal-external shape rather than
//!   silently dropping the system tx.
//! - **Keyless deploy step 7** (`sandbox/execution.rs`): the deploy-address occupancy read before
//!   sandbox setup can surface a database error — pre-REX6 via `basic(deploy_address)`, REX6 via
//!   the journaled, cold `inspect_account_code_hash(ctx.journal_mut(), deploy_address)`, which
//!   reads only `code_hash` and never hydrates `info.code`. Both arms' `map_err` must wrap it as
//!   the selector-only `KeylessDeployError::InternalError` and surface as `Revert` (validation-
//!   style — no sandbox state to merge).

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    revm::context::result::ExecutionResult,
    sandbox::{
        decode_error_result,
        tests::{CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER, CREATE2_FACTORY_TX},
        KeylessDeployError,
    },
    test_utils::{ErrorInjectingDatabase, InjectedDbError, MemoryDatabase},
    EVMError, IKeylessDeploy, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS, MEGA_SYSTEM_ADDRESS,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    context::TxEnv,
    inspector::NoOpInspector,
    primitives::{StorageKey, StorageValue, KECCAK_EMPTY},
    state::{AccountInfo, Bytecode},
    Database,
};

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

/// REX6 twin of the test above: under REX6 the step-7 occupancy check reads the deploy address
/// through the journaled, cold `inspect_account_code_hash`, not the raw `basic`. That read's
/// `map_err` DB-error arm must still surface as the selector-only `InternalError` revert, signer
/// untouched. Without this the REX6 branch's error path is never exercised (the raw-`basic` test
/// above only covers the pre-REX6 arm).
#[test]
fn test_keyless_deploy_address_db_error_maps_to_internal_error_revert_rex6() {
    let mut inner = MemoryDatabase::default();
    inner.set_account_balance(
        CREATE2_FACTORY_DEPLOYER,
        U256::from(1_000_000_000_000_000_000_000u128),
    );
    inner.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let mut db = ErrorInjectingDatabase::new(inner);
    // Step 7 under REX6 does `inspect_account_code_hash(ctx.journal_mut(), deploy_address)`; fail
    // that read.
    db.fail_on_account = Some(CREATE2_FACTORY_CONTRACT);

    let external_envs = TestExternalEnvs::<Infallible>::new();
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(CREATE2_FACTORY_TX),
        gasLimitOverride: U256::from(10_000_000u64),
    }
    .abi_encode();

    let mut context =
        MegaContext::new(db, MegaSpecId::REX6).with_external_envs(external_envs.into());
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
        "REX6 inspect_account_code_hash DB failure on deploy address must surface as InternalError, got {decoded:?}",
    );

    let signer_after =
        res.state.get(&CREATE2_FACTORY_DEPLOYER).cloned().map(|acc| acc.info).unwrap_or_default();
    assert_eq!(
        signer_after.nonce, 0,
        "DB error before sandbox must not bump signer nonce (no replay barrier)",
    );
}

/// An [`ErrorInjectingDatabase`] whose account-read failure fires once: the first failing
/// `basic()` read disarms it, so a later read of the same account succeeds (a transient error).
#[derive(Debug)]
struct FailOnceDatabase(ErrorInjectingDatabase);

impl Database for FailOnceDatabase {
    type Error = InjectedDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, InjectedDbError> {
        let read = self.0.basic(address);
        if read.is_err() {
            self.0.fail_on_account = None;
        }
        read
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, InjectedDbError> {
        self.0.code_by_hash(code_hash)
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, InjectedDbError> {
        self.0.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, InjectedDbError> {
        self.0.block_hash(number)
    }
}

/// The deploy-address occupancy read fails closed on its own: when only that read fails, the
/// keyless deploy reverts with `InternalError` and deploys nothing, although the sandbox's own
/// read of the same address would succeed.
fn assert_one_off_deploy_address_read_error_aborts_the_deploy(spec: MegaSpecId) {
    let mut inner = MemoryDatabase::default();
    inner.set_account_balance(
        CREATE2_FACTORY_DEPLOYER,
        U256::from(1_000_000_000_000_000_000_000u128),
    );
    inner.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_account = Some(CREATE2_FACTORY_CONTRACT);
    let db = FailOnceDatabase(db);

    let external_envs = TestExternalEnvs::<Infallible>::new();
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(CREATE2_FACTORY_TX),
        gasLimitOverride: U256::from(10_000_000u64),
    }
    .abi_encode();

    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
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
        other => panic!("{spec:?}: expected outer Revert, got {other:?}"),
    };
    let decoded = decode_error_result(&revert_output)
        .expect("revert payload must decode to a known KeylessDeployError");
    assert!(
        matches!(decoded, KeylessDeployError::InternalError),
        "{spec:?}: a failed occupancy read must surface as InternalError, got {decoded:?}",
    );
    let deployed = res.state.get(&CREATE2_FACTORY_CONTRACT).map(|acc| acc.info.code_hash);
    assert!(
        deployed.is_none_or(|code_hash| code_hash == KECCAK_EMPTY),
        "{spec:?}: nothing may be deployed after a failed occupancy read",
    );
}

/// Pre-REX6 occupancy read (`database.basic(deploy_address)`).
#[test]
fn test_keyless_deploy_address_one_off_db_error_aborts_the_deploy() {
    assert_one_off_deploy_address_read_error_aborts_the_deploy(MegaSpecId::REX5);
}

/// REX6 occupancy read (`inspect_account_code_hash`).
#[test]
fn test_keyless_deploy_address_one_off_db_error_aborts_the_deploy_rex6() {
    assert_one_off_deploy_address_read_error_aborts_the_deploy(MegaSpecId::REX6);
}

/// REX5+ keyless deploy must reject with `InternalError` (selector-only revert) when the
/// pre-sandbox signer nonce read fails. `get_account_nonce` is the first DB read against
/// the deploy signer, and its DB-fallback `map_err` is the only place this failure can be
/// classified before the sandbox is even constructed.
#[test]
fn test_keyless_signer_nonce_db_error_maps_to_internal_error_revert() {
    let mut inner = MemoryDatabase::default();
    inner.set_account_balance(
        CREATE2_FACTORY_DEPLOYER,
        U256::from(1_000_000_000_000_000_000_000u128),
    );
    inner.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let mut db = ErrorInjectingDatabase::new(inner);
    // Step 5 of `execute_keyless_deploy_call` reads the signer's nonce via
    // `get_account_nonce` (journal cache → `journal.database.basic(signer)` fallback).
    // Fail that fallback path.
    db.fail_on_account = Some(CREATE2_FACTORY_DEPLOYER);

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
        "signer nonce DB read failure must surface as InternalError, got {decoded:?}",
    );

    // Pre-sandbox failure: signer must not be touched, no replay barrier.
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
