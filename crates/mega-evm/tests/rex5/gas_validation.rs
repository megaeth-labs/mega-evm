//! REX5 final-Mega-gas validation regression tests.
//!
//! Pin the invariant that, before `pre_execution()` runs, `MegaHandler::validate()`
//! has accounted for every Mega-side intrinsic and dynamic storage gas contribution
//! and rejected the transaction as a canonical validation error if either bound is
//! exceeded.
//!
//! Pre-REX5 specs intentionally produce a different shape (synthetic OOG with
//! `gas_used == gas_limit`); the legacy stable-spec tests in
//! `tests/mini_rex/gas.rs::test_mini_rex_insufficient_storage_gas_*_oog` lock that
//! shape in. This module locks down the new REX5 shape and adds a REX4 mirror of
//! the legacy stable-spec test so the gating is exercised in two pre-REX5 specs
//! that follow different code paths into Mega-step-D.

use std::convert::Infallible;

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EVMError, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionError, SaltEnv, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{
    context::{result::ResultAndState, TxEnv},
    primitives::Address,
};

use revm::context::result::InvalidTransaction;

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const NEW_ACCOUNT: Address = address!("9000000000000000000000000000000000000009");

/// Build a context wired to the given spec and salt environment.
fn build_evm(
    db: &mut MemoryDatabase,
    spec: MegaSpecId,
    external_envs: TestExternalEnvs<Infallible>,
) -> MegaEvm<&mut MemoryDatabase, revm::inspector::NoOpInspector, TestExternalEnvs<Infallible>> {
    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    MegaEvm::new(context)
}

fn run_tx(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: TestExternalEnvs<Infallible>,
    tx: TxEnv,
) -> Result<ResultAndState<mega_evm::MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut evm = build_evm(db, spec, external_envs);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

fn external_envs_with_hot_account(
    account: Address,
    multiplier: u64,
) -> TestExternalEnvs<Infallible> {
    let bucket_id = TestExternalEnvs::<Infallible>::bucket_id_for_account(account);
    TestExternalEnvs::<Infallible>::new()
        .with_bucket_capacity(bucket_id, MIN_BUCKET_SIZE as u64 * multiplier)
}

fn assert_call_gas_cost_more_than_gas_limit<E: core::fmt::Debug>(err: &EVMError<Infallible, E>) {
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("CallGasCostMoreThanGasLimit"),
        "expected CallGasCostMoreThanGasLimit, got {dbg}",
    );
}

fn assert_gas_floor_more_than_gas_limit<E: core::fmt::Debug>(err: &EVMError<Infallible, E>) {
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("GasFloorMoreThanGasLimit"),
        "expected GasFloorMoreThanGasLimit, got {dbg}",
    );
}

/// Sender state must be untouched when `validate()` rejects: balance unchanged AND nonce unchanged.
fn assert_sender_untouched(db: &mut MemoryDatabase, expected_balance: U256, expected_nonce: u64) {
    use revm::Database as _;
    let info = db.basic(CALLER).expect("db read should succeed").unwrap_or_default();
    assert_eq!(
        info.balance, expected_balance,
        "sender balance must not change on validation reject"
    );
    assert_eq!(info.nonce, expected_nonce, "sender nonce must not change on validation reject");
}

// ==================================================================================
// REX5 validation-rejection tests
// ==================================================================================

/// REX5: a top-level CALL with non-zero value to an empty callee whose bucket is hot
/// must be rejected by `validate()` once Mega-side new-account storage gas is added.
/// The sender's balance and nonce must not move.
#[test]
fn test_rex5_initial_gas_includes_new_callee_storage_gas_before_validation_rejection() {
    let mut db = MemoryDatabase::default();
    let initial_balance = U256::from(10_000_000u64);
    db.set_account_balance(CALLER, initial_balance);

    let multiplier = 10u64; // Rex-tier new_account storage gas: 2_000_000 * 10 = 20_000_000
    let external_envs = external_envs_with_hot_account(NEW_ACCOUNT, multiplier);

    // Intrinsic ≈ 21k, REX intrinsic storage gas 39k → ~60k. Storage gas is 20M.
    // Choose 80_000 so initial_gas after Mega-step-D blows past gas_limit.
    let insufficient_gas_limit = 80_000;
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(NEW_ACCOUNT),
        data: Bytes::new(),
        value: U256::from(1),
        gas_limit: insufficient_gas_limit,
        ..Default::default()
    };

    let err = run_tx(MegaSpecId::REX5, &mut db, external_envs, tx)
        .expect_err("REX5 must reject Mega-final-gas overrun as a validation error");
    assert_call_gas_cost_more_than_gas_limit(&err);
    assert_sender_untouched(&mut db, initial_balance, 0);
}

/// REX5: a top-level CREATE whose final Mega-side initial gas exceeds the tx gas limit
/// must be rejected by `validate()` rather than synthesizing an OOG after fee debit.
#[test]
fn test_rex5_initial_gas_includes_create_storage_gas_before_validation_rejection() {
    let mut db = MemoryDatabase::default();
    let initial_balance = U256::from(10_000_000u64);
    db.set_account_balance(CALLER, initial_balance);

    let created_address = CALLER.create(0);
    let multiplier = 10u64; // Rex-tier contract creation storage gas dominates the budget.
    let external_envs = external_envs_with_hot_account(created_address, multiplier);

    // Intrinsic + CREATE base ≈ 53k, REX intrinsic 39k → ~92k. Storage gas is in the millions.
    let insufficient_gas_limit = 120_000;
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit: insufficient_gas_limit,
        ..Default::default()
    };

    let err = run_tx(MegaSpecId::REX5, &mut db, external_envs, tx)
        .expect_err("REX5 must reject Mega-final-gas overrun as a validation error");
    assert_call_gas_cost_more_than_gas_limit(&err);
    assert_sender_untouched(&mut db, initial_balance, 0);
}

/// REX5: a tx whose floor gas exceeds the gas limit (driven by Mega calldata-floor scaling)
/// must be rejected with `GasFloorMoreThanGasLimit`. The sender state must not move.
#[test]
fn test_rex5_floor_gas_above_gas_limit_is_validation_rejection() {
    let mut db = MemoryDatabase::default();
    let initial_balance = U256::from(1_000_000_000u64);
    db.set_account_balance(CALLER, initial_balance);

    // For a top-level CALL with non-zero value to the existing sender (no new-account storage):
    //   initial_gas = 21_000 (base) + 4*T (canonical zero-byte token cost) + 40*T (mega calldata
    //                 storage) + 39_000 (REX intrinsic) = 60_000 + 44*T
    //   floor_gas   = 21_000 + 10*T (EIP-7623 floor) + 100*T (mega floor storage) = 21_000 + 110*T
    //
    // For a gas_limit that bands `initial_gas <= gas_limit < floor_gas` we need
    //     60_000 + 44*T <= gas_limit < 21_000 + 110*T,
    // i.e. T > 590. Use T = 700 zero bytes:
    //   initial_gas = 90_800
    //   floor_gas   = 98_000
    let calldata = Bytes::from(vec![0u8; 700]);
    let gas_limit = 95_000;
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(NEW_ACCOUNT),
        data: calldata,
        value: U256::ZERO,
        gas_limit,
        ..Default::default()
    };

    let err = run_tx(MegaSpecId::REX5, &mut db, TestExternalEnvs::<Infallible>::new(), tx)
        .expect_err("REX5 must reject floor_gas > gas_limit as a validation error");
    assert_gas_floor_more_than_gas_limit(&err);
    assert_sender_untouched(&mut db, initial_balance, 0);
}

/// REX5: a normal valid CALL still goes through. Sanity that the new check is not too aggressive.
#[test]
fn test_rex5_valid_tx_still_passes() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(10u64).pow(U256::from(20u64))); // 100 ETH

    let callee: Address = address!("1000000000000000000000000000000000000001");
    db.set_account_balance(callee, U256::from(1u64));

    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(callee),
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit: 1_000_000,
        ..Default::default()
    };

    let res = run_tx(MegaSpecId::REX5, &mut db, TestExternalEnvs::<Infallible>::new(), tx)
        .expect("plain transfer to existing account must succeed under REX5");
    assert!(res.result.is_success(), "got {:?}", res.result);
}

// ==================================================================================
// Stable-spec preservation tests (pre-REX5 specs keep the bug-shape OOG behavior)
// ==================================================================================
//
// MINI_REX coverage already lives in
// `tests/mini_rex/gas.rs::test_mini_rex_insufficient_storage_gas_*_oog` (intentional bug-shape
// pin). The block below mirrors the same scenario at REX4 so the gating is exercised against the
// second pre-REX5 code path (REX adds `TX_INTRINSIC_STORAGE_GAS`, so MINI_REX and REX..REX4 walk
// Mega-step-D differently).

/// REX4 (stable spec) must preserve the historical bug-shape behavior: `validate()` accepts
/// the tx, `pre_execution()` debits and bumps, and execution synthesizes an OOG halt with
/// `gas_used == gas_limit`. This test fails loudly if any future change leaks the REX5
/// rejection back into a stable spec.
#[test]
fn test_rex4_preserves_legacy_oog_after_full_gas_charge() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(10_000_000u64));

    let multiplier = 10u64;
    let external_envs = external_envs_with_hot_account(NEW_ACCOUNT, multiplier);

    // Same scenario shape as the REX5 test, with a gas limit that covers intrinsic+REX
    // intrinsic-storage but not new-account storage gas.
    let insufficient_gas_limit = 80_000;
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(NEW_ACCOUNT),
        data: Bytes::new(),
        value: U256::from(1),
        gas_limit: insufficient_gas_limit,
        ..Default::default()
    };

    let res = run_tx(MegaSpecId::REX4, &mut db, external_envs, tx)
        .expect("pre-REX5 specs must NOT reject this as a validation error");
    assert!(!res.result.is_success(), "REX4 must still produce a halt, not success");
    assert!(res.result.is_halt(), "REX4 must still produce a halt-shaped result");
    assert_eq!(
        res.result.gas_used(),
        insufficient_gas_limit,
        "REX4 must still consume the entire gas limit (bug-shape preserved)"
    );
}

// `_` to prove `InvalidTransaction` from `revm::context::result` is reachable via the
// public re-export and to catch a future revm bump that drops the variant.
const _: fn() = || {
    let _ = InvalidTransaction::CallGasCostMoreThanGasLimit { initial_gas: 0, gas_limit: 0 };
    let _ = InvalidTransaction::GasFloorMoreThanGasLimit { gas_floor: 0, gas_limit: 0 };
};
