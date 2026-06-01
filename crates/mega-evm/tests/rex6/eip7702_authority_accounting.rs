//! REX6 regression: consolidated EIP-7702 authorization accounting.
//!
//! REX6 routes every per-authorization effect through one journal-aware scan in `validate`:
//! - net-new authorities are charged dynamic SALT account-creation gas, so a type-4 tx that creates
//!   an authority in a heavy SALT bucket consumes more gas than it did pre-REX6;
//! - DataSize/KV are charged only for *applied* authorities (passed the chain-id/nonce/code gates),
//!   not every recoverable one, so a skipped authorization no longer inflates resource usage.
//!
//! Pre-REX6 keeps the old split (ungated `before_tx_start` DataSize/KV + pre-execution
//! state-growth scan, no authority SALT gas), frozen for replay parity — the REX5 arms pin it.

use std::convert::Infallible;

use alloy_eips::eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants, test_utils::MemoryDatabase, BucketHasher, EVMError, EvmTxRuntimeLimits, LimitUsage,
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError,
    SimpleBucketHasher, TestExternalEnvs, ACCOUNT_INFO_WRITE_SIZE, MIN_BUCKET_SIZE,
};
use revm::{
    context::{
        result::{ExecutionResult, InvalidTransaction, ResultAndState},
        tx::TxEnvBuilder,
        BlockEnv, TxEnv,
    },
    handler::EvmTr,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000800000");
const CALLEE: Address = address!("0000000000000000000000000000000000800001");
const AUTHORITY_A: Address = address!("0000000000000000000000000000000000800010");
const AUTHORITY_B: Address = address!("0000000000000000000000000000000000800011");
const DELEGATE: Address = address!("0000000000000000000000000000000000900001");
/// Used as the block beneficiary in the detention test.
const BENEFICIARY: Address = address!("0000000000000000000000000000000000800099");

// ============================================================================
// TEST CONSTANTS
// ============================================================================

/// Multiplier 100 → a net-new account in this bucket costs `base * 99` storage gas; the default
/// bucket has multiplier 1 → 0. The spread is what the SALT-gas test observes.
const HEAVY_MULTIPLIER: u64 = 100;
const HEAVY_CAPACITY: u64 = (MIN_BUCKET_SIZE as u64) * HEAVY_MULTIPLIER;

// ============================================================================
// HELPERS
// ============================================================================

type Envs = TestExternalEnvs<Infallible, SimpleBucketHasher>;

fn no_heavy_buckets() -> Envs {
    TestExternalEnvs::new()
}

fn heavy_bucket_for(address: Address) -> Envs {
    let bucket = SimpleBucketHasher::bucket_id(address.as_slice());
    TestExternalEnvs::new().with_bucket_capacity(bucket, HEAVY_CAPACITY)
}

fn transact_with_limits(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    envs: &Envs,
    limits: EvmTxRuntimeLimits,
    tx: TxEnv,
) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    let mut context =
        MegaContext::new(db, spec).with_external_envs(envs.into()).with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (r, usage)
}

/// Runs with the resource limits effectively disabled, so a test observes raw usage / gas rather
/// than a limit halt.
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    envs: &Envs,
    tx: TxEnv,
) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    let limits = EvmTxRuntimeLimits::from_spec(spec)
        .with_tx_data_size_limit(u64::MAX)
        .with_tx_kv_updates_limit(u64::MAX)
        .with_tx_state_growth_limit(u64::MAX);
    transact_with_limits(spec, db, envs, limits, tx)
}

/// Like [`transact`] but returns the raw result, so a validation rejection (e.g. an unaffordable
/// gas requirement) is observable as an `Err` instead of panicking on unwrap.
fn try_transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    envs: &Envs,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context =
        MegaContext::new(db, spec).with_external_envs(envs.into()).with_tx_runtime_limits(
            EvmTxRuntimeLimits::from_spec(spec)
                .with_tx_data_size_limit(u64::MAX)
                .with_tx_kv_updates_limit(u64::MAX)
                .with_tx_state_growth_limit(u64::MAX),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// A recoverable authorization for `authority` delegating to `DELEGATE`. A nonzero mismatching
/// `chain_id` makes it recoverable but un-appliable (the application gate rejects it).
fn auth(authority: Address, chain_id: u64, nonce: u64) -> RecoveredAuthorization {
    RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(chain_id), address: DELEGATE, nonce },
        RecoveredAuthority::Valid(authority),
    )
}

/// An authorization whose signature does not recover to any authority. Skipped by every
/// downstream pass because the recovery gate fails before any account read.
fn auth_unrecoverable(chain_id: u64, nonce: u64) -> RecoveredAuthorization {
    RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(chain_id), address: DELEGATE, nonce },
        RecoveredAuthority::Invalid,
    )
}

fn tx_with_auths(auths: Vec<RecoveredAuthorization>) -> TxEnv {
    TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(10_000_000)
        .authorization_list_recovered(auths)
        .build_fill()
}

fn funded_db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64))
}

/// Runs `tx` with `BENEFICIARY` as the block beneficiary and a beneficiary-detention compute-gas
/// cap, returning the result and the detained compute-gas limit.
fn transact_detention(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_compute_limit: u64,
    detention_cap: u64,
    tx: TxEnv,
) -> (ResultAndState<MegaHaltReason>, u64) {
    let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
    let mut context = MegaContext::new(db, spec).with_block(block).with_tx_runtime_limits(
        EvmTxRuntimeLimits::from_spec(spec)
            .with_tx_compute_gas_limit(tx_compute_limit)
            .with_block_env_access_compute_gas_limit(detention_cap),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let detained = evm.ctx_ref().additional_limit.borrow().detained_compute_gas_limit();
    (r, detained)
}

// ============================================================================
// TESTS
// ============================================================================

/// A net-new authority in a heavy SALT bucket costs more gas than one in the default bucket.
///
/// Both REX6 runs apply the same valid net-new authorization (`chain_id` 1 = tx chain, nonce 0), so
/// they differ only by the dynamic SALT account-creation gas — which only the heavy-bucket run
/// charges. The SALT gas must not bleed into the data-size or KV dimensions.
#[test]
fn test_rex6_new_authority_charges_salt_gas() {
    let auths = vec![auth(AUTHORITY_A, 1, 0)];

    let (res_heavy, u_heavy) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &heavy_bucket_for(AUTHORITY_A),
        tx_with_auths(auths.clone()),
    );
    let (res_default, u_default) =
        transact(MegaSpecId::REX6, &mut funded_db(), &no_heavy_buckets(), tx_with_auths(auths));
    assert!(res_heavy.result.is_success(), "heavy-bucket tx should succeed: {res_heavy:?}");
    assert!(res_default.result.is_success(), "default-bucket tx should succeed: {res_default:?}");

    // The heavy bucket charges exactly `base * (multiplier - 1)` extra new-account gas.
    let expected_salt = constants::rex::NEW_ACCOUNT_STORAGE_GAS_BASE * (HEAVY_MULTIPLIER - 1);
    assert_eq!(
        res_heavy.result.gas_used() - res_default.result.gas_used(),
        expected_salt,
        "REX6 must charge exactly the heavy-bucket SALT gas for the new authority",
    );

    // One applied net-new authority is one state-growth unit, and the SALT gas touches neither
    // the data-size nor the KV dimension.
    assert_eq!(u_heavy.state_growth, 1, "one net-new authority = one state-growth unit");
    assert_eq!(u_heavy.data_size, u_default.data_size, "SALT gas must not affect data size");
    assert_eq!(u_heavy.kv_updates, u_default.kv_updates, "SALT gas must not affect KV");
}

/// REX6 *enforces* the net-new authority SALT gas against `gas_limit`, not just accounts for it.
///
/// The SALT account-creation gas is folded into `initial_gas` before the gas-limit check, so a
/// type-4 tx that cannot afford it is rejected at validation (an `Err`, fees/nonce untouched), not
/// run with a gas-burn halt. A budget that comfortably runs the same authorization in the default
/// bucket is too small once the heavy-bucket SALT (~2.475M) is added.
#[test]
fn test_rex6_authority_salt_gas_enforced_against_gas_limit() {
    let auths = vec![auth(AUTHORITY_A, 1, 0)];

    // Default bucket (SALT = 0): its total gas_used is the budget handed to the heavy run below. It
    // sits just above the shared intrinsic and far below intrinsic + heavy SALT.
    let (res_default, _) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &no_heavy_buckets(),
        tx_with_auths(auths.clone()),
    );
    assert!(res_default.result.is_success(), "default-bucket run should succeed: {res_default:?}");
    let default_budget = res_default.result.gas_used();

    // Same authorization, heavy bucket, but only the default run's budget: the heavy SALT pushes
    // initial_gas past gas_limit, so validation rejects before execution.
    let tight_tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(default_budget)
        .authorization_list_recovered(auths.clone())
        .build_fill();
    let err =
        try_transact(MegaSpecId::REX6, &mut funded_db(), &heavy_bucket_for(AUTHORITY_A), tight_tx)
            .expect_err("heavy-bucket run under the default budget must be rejected");
    assert!(
        matches!(
            err,
            EVMError::Transaction(MegaTransactionError::Base(
                InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
            ))
        ),
        "expected CallGasCostMoreThanGasLimit from the unaffordable SALT gas, got {err:?}",
    );

    // The same heavy run with a budget that covers the SALT succeeds — proving the rejection was
    // the SALT gas, not an unrelated shortfall.
    let (res_ok, _) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &heavy_bucket_for(AUTHORITY_A),
        tx_with_auths(auths),
    );
    assert!(
        res_ok.result.is_success(),
        "heavy-bucket run with enough gas should succeed: {res_ok:?}"
    );
}

/// A recoverable-but-unapplied authorization writes nothing; only applied authorities are charged.
///
/// The skipped authorization's `chain_id` (999) mismatches the tx chain (1), so the application
/// gate rejects it. Compared against the same authority with a matching chain id (applied), the
/// applied run charges exactly one account write more: data +40, KV +1, state-growth +1.
#[test]
fn test_rex6_skipped_authority_not_charged_datasize_kv() {
    let envs = no_heavy_buckets();

    let (res_skip, u_skip) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_B, 999, 0)]),
    );
    let (res_applied, u_applied) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_B, 1, 0)]),
    );
    assert!(res_skip.result.is_success(), "skipped-auth tx should succeed: {res_skip:?}");
    assert!(res_applied.result.is_success(), "applied-auth tx should succeed: {res_applied:?}");

    assert_eq!(u_skip.state_growth, 0, "a skipped authority creates no state growth");
    assert_eq!(u_applied.state_growth, 1, "an applied net-new authority creates one");
    assert_eq!(
        u_applied.data_size - u_skip.data_size,
        ACCOUNT_INFO_WRITE_SIZE,
        "an applied authority charges exactly one account write more than a skipped one",
    );
    assert_eq!(
        u_applied.kv_updates - u_skip.kv_updates,
        1,
        "an applied authority charges exactly one more KV update than a skipped one",
    );
}

/// Two sequential-nonce authorizations for the same authority both apply, but create it only once.
///
/// `[auth(A, nonce 0), auth(A, nonce 1)]`: the first creates A (net-new), the second matches A's
/// simulated nonce and re-delegates it (not net-new). Both are applied, so each charges one account
/// write (data +40, KV +1) — but only the first is state growth. Contrasted with a run whose second
/// authorization is skipped (stale nonce), holding the authorization-record size constant, the
/// duplicate's extra applied write is isolated to exactly one account write and no extra growth.
#[test]
fn test_rex6_duplicate_authority_applies_twice_grows_once() {
    let envs = no_heavy_buckets();

    // Second auth applies: nonce 1 == A's simulated nonce after the first application.
    let (res_dup, u_dup) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0), auth(AUTHORITY_A, 1, 1)]),
    );
    // Same two-record shape, but the second auth is skipped: stale nonce 99 != simulated nonce 1.
    let (res_skip2, u_skip2) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0), auth(AUTHORITY_A, 1, 99)]),
    );
    assert!(res_dup.result.is_success(), "duplicate-auth tx should succeed: {res_dup:?}");
    assert!(res_skip2.result.is_success(), "second-skipped tx should succeed: {res_skip2:?}");

    // The duplicate second application is not new state growth — A is created exactly once either
    // way.
    assert_eq!(u_dup.state_growth, 1, "a duplicated authority is created once");
    assert_eq!(u_skip2.state_growth, 1, "the applied first authority is created once");

    // The applied second authorization charges exactly one more account write than the skipped one;
    // both lists are length 2, so the authorization-record data size cancels.
    assert_eq!(
        u_dup.data_size - u_skip2.data_size,
        ACCOUNT_INFO_WRITE_SIZE,
        "the duplicate's second applied authorization charges one more account write",
    );
    assert_eq!(
        u_dup.kv_updates - u_skip2.kv_updates,
        1,
        "the duplicate's second applied authorization charges one more KV update",
    );

    // Both applications bumped A's nonce; the skipped run applied only the first.
    let authority_nonce = |r: &ResultAndState<MegaHaltReason>| {
        r.state.get(&AUTHORITY_A).map(|a| a.info.nonce).unwrap_or_default()
    };
    assert_eq!(authority_nonce(&res_dup), 2, "both authorizations applied: nonce bumped twice");
    assert_eq!(authority_nonce(&res_skip2), 1, "only the first authorization applied: nonce once");
}

/// An applied authority that already exists is charged DataSize/KV but is not state growth.
///
/// Delegating an account that already exists writes it (data +40, KV +1) but creates no net-new
/// state entry. Against a net-new authority — same single applied write — the existing one differs
/// only in the state-growth dimension (0 vs 1).
#[test]
fn test_rex6_existing_authority_charged_but_not_state_growth() {
    let envs = no_heavy_buckets();

    // AUTHORITY_A is pre-funded, so it already exists; AUTHORITY_B does not (net-new).
    let mut db_existing = funded_db().account_balance(AUTHORITY_A, U256::from(1u64));
    let (res_existing, u_existing) = transact(
        MegaSpecId::REX6,
        &mut db_existing,
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0)]),
    );
    let (res_new, u_new) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_B, 1, 0)]),
    );
    assert!(
        res_existing.result.is_success(),
        "existing-authority tx should succeed: {res_existing:?}"
    );
    assert!(res_new.result.is_success(), "net-new-authority tx should succeed: {res_new:?}");

    // Both applied authorities write their account exactly once: identical DataSize / KV.
    assert_eq!(
        u_existing.data_size, u_new.data_size,
        "an applied authority writes one account either way"
    );
    assert_eq!(
        u_existing.kv_updates, u_new.kv_updates,
        "an applied authority charges one KV update either way"
    );
    // Only the net-new authority grows state.
    assert_eq!(u_existing.state_growth, 0, "delegating an existing account is not state growth");
    assert_eq!(u_new.state_growth, 1, "delegating a net-new account is state growth");
}

/// A net-new authority that overflows the state-growth limit still halts the REX6 transaction.
///
/// REX6 records the authority's state growth in `validate` (earlier than the pre-REX6 pre-execution
/// scan), but the overflow is latched and surfaced at the first frame boundary just the same: the
/// tx halts with `StateGrowthLimitExceeded`, the first frame never starts, and the authorization
/// was still applied (nonce bumped) before the halt.
#[test]
fn test_rex6_authority_state_growth_overflow_still_halts() {
    let (res, usage) = transact_with_limits(
        MegaSpecId::REX6,
        &mut funded_db(),
        &no_heavy_buckets(),
        EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(0),
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0)]),
    );

    assert!(
        matches!(
            &res.result,
            ExecutionResult::Halt {
                reason: MegaHaltReason::StateGrowthLimitExceeded { limit: 0, actual: 1 },
                ..
            }
        ),
        "REX6 must halt when an applied authority overflows the state-growth limit: {res:?}",
    );
    assert_eq!(usage.state_growth, 1, "the authority creation is recorded before the halt");
    assert!(
        res.state.get(&CALLEE).is_none_or(|a| !a.is_touched()),
        "the first frame must not start once pre-frame state growth already exceeds the limit",
    );
    let authority = res.state.get(&AUTHORITY_A).expect("authority update should be preserved");
    assert_eq!(authority.info.nonce, 1, "the authorization was still applied before the halt");
}

/// An authority that is also the value-transfer recipient is charged its SALT gas once, not twice.
///
/// Without REX6's recipient exclusion, both the value-transfer new-account charge and the authority
/// SALT charge would bill the same heavy account creation, doubling a ~3.17M-gas SALT charge. A
/// heavy-bucket value transfer with an authorization that materializes the same recipient is
/// compared against the same transfer without one: the delta stays well under one heavy SALT.
#[test]
fn test_rex6_recipient_authority_not_double_charged() {
    let envs = heavy_bucket_for(AUTHORITY_A);

    // Baseline: value transfer to the empty heavy-bucket account, no authorization → the recipient
    // new-account branch bills its heavy SALT once.
    let no_auth = TxEnvBuilder::default()
        .caller(CALLER)
        .call(AUTHORITY_A)
        .value(U256::from(1))
        .gas_limit(10_000_000)
        .build_fill();
    let (res_no_auth, _) = transact(MegaSpecId::REX6, &mut funded_db(), &envs, no_auth);
    assert!(res_no_auth.result.is_success(), "no-auth transfer should succeed: {res_no_auth:?}");

    // Same transfer, but with an authorization that materializes the recipient: REX6 excludes it
    // from the value-transfer new-account charge and bills the heavy SALT once via the authority.
    let with_auth = TxEnvBuilder::default()
        .caller(CALLER)
        .call(AUTHORITY_A)
        .value(U256::from(1))
        .gas_limit(10_000_000)
        .authorization_list_recovered(vec![auth(AUTHORITY_A, 1, 0)])
        .build_fill();
    let (res_with_auth, _) = transact(MegaSpecId::REX6, &mut funded_db(), &envs, with_auth);
    assert!(
        res_with_auth.result.is_success(),
        "with-auth transfer should succeed: {res_with_auth:?}"
    );

    // The with-auth tx adds only authorization overhead (intrinsic + per-auth), well under one
    // heavy SALT (~3.17M). A double charge would inflate gas_used by roughly a full heavy SALT.
    let delta = res_with_auth.result.gas_used().saturating_sub(res_no_auth.result.gas_used());
    assert!(
        delta < 1_000_000,
        "the recipient authority's heavy SALT must be charged once, not twice (delta={delta})",
    );
}

/// An applied authority that is the block beneficiary triggers beneficiary gas detention in REX6.
///
/// The authority — and neither the caller nor the recipient (`CALLEE`) — is the beneficiary, so
/// only the authority-side marking can detain. REX6 lowers the compute-gas limit to the detention
/// cap; REX5 has no authority-side beneficiary marking, so it does not detain.
#[test]
fn test_rex6_authority_beneficiary_triggers_detention() {
    const TX_COMPUTE_LIMIT: u64 = 200_000_000;
    const DETENTION_CAP: u64 = 20_000_000;

    // The authority IS the block beneficiary; the caller and recipient (`CALLEE`) are not.
    let auths = vec![auth(BENEFICIARY, 1, 0)];
    let tx = || {
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CALLEE)
            .gas_limit(10_000_000)
            .authorization_list_recovered(auths.clone())
            .build_fill()
    };

    let (res6, detained6) = transact_detention(
        MegaSpecId::REX6,
        &mut funded_db(),
        TX_COMPUTE_LIMIT,
        DETENTION_CAP,
        tx(),
    );
    let (res5, detained5) = transact_detention(
        MegaSpecId::REX5,
        &mut funded_db(),
        TX_COMPUTE_LIMIT,
        DETENTION_CAP,
        tx(),
    );
    assert!(res6.result.is_success(), "REX6 should succeed: {res6:?}");
    assert!(res5.result.is_success(), "REX5 should succeed: {res5:?}");

    // REX6 marks beneficiary detention for the applied `authority == beneficiary`, lowering the
    // compute-gas limit to the detention cap.
    assert!(
        detained6 <= DETENTION_CAP,
        "REX6 must detain compute gas when an applied authority is the beneficiary (detained6={detained6})",
    );
    // REX5 has no authority-side beneficiary marking, so it does not detain.
    assert!(
        detained5 > DETENTION_CAP,
        "REX5 must not detain from an authority (detained5={detained5})",
    );
}

/// The validate-time scan mirrors the caller-nonce bump, so a self-authorization is accounted
/// exactly as it is applied.
///
/// On a call tx the caller's nonce is bumped to `tx.nonce + 1` before `apply_eip7702_auth_list`
/// runs, so a self-authorization (`authority == caller`) applies iff its nonce == tx.nonce + 1. The
/// validate-time scan runs before that bump and must mirror it; otherwise the applied/skipped
/// decision and its DataSize/KV accounting diverge from the real application by one. CALLER's
/// account nonce is 0, and `tx_with_auths` builds a tx with nonce 0.
#[test]
fn test_rex6_self_authorization_nonce_matches_application() {
    let envs = no_heavy_buckets();

    let caller_delegated = |res: &ResultAndState<MegaHaltReason>| -> bool {
        res.state.get(&CALLER).and_then(|a| a.info.code.as_ref()).is_some_and(|c| c.is_eip7702())
    };

    // auth.nonce == tx.nonce (0) → must NOT apply (the bumped caller nonce is 1).
    let (res_skip, u_skip) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(CALLER, 1, 0)]),
    );
    assert!(res_skip.result.is_success(), "skip-case tx should succeed: {res_skip:?}");
    assert!(!caller_delegated(&res_skip), "auth.nonce == tx.nonce must NOT apply a self-auth");

    // auth.nonce == tx.nonce + 1 (1) → must apply.
    let (res_apply, u_apply) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(CALLER, 1, 1)]),
    );
    assert!(res_apply.result.is_success(), "apply-case tx should succeed: {res_apply:?}");
    assert!(caller_delegated(&res_apply), "auth.nonce == tx.nonce+1 must apply a self-auth");

    // The scan's accounting must match the application: the applied case charges exactly one more
    // authority account write than the skipped case (a mismatched scan would invert the two).
    assert_eq!(
        u_apply.data_size - u_skip.data_size,
        ACCOUNT_INFO_WRITE_SIZE,
        "applied self-auth must charge exactly one account write more than the skipped case (apply={} skip={})",
        u_apply.data_size,
        u_skip.data_size,
    );
    assert_eq!(
        u_apply.kv_updates - u_skip.kv_updates,
        1,
        "applied self-auth charges exactly one more KV update than the skipped case",
    );
}

/// An authorization whose nonce is `u64::MAX` is skipped by the application gate and charges no
/// per-applied resources, mirroring revm's `2**64 - 1` reject.
///
/// Compared against the same authority with a matching nonce (applied), the applied run charges
/// exactly one more account write — data +40, KV +1, state-growth +1. Both runs carry the same
/// authorization-record count, so the per-record `AUTHORIZATION_DATA_SIZE` contribution cancels in
/// the diff.
#[test]
fn test_rex6_u64_max_nonce_authority_skipped() {
    let envs = no_heavy_buckets();

    let (res_skip, u_skip) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_A, 1, u64::MAX)]),
    );
    let (res_applied, u_applied) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0)]),
    );
    assert!(res_skip.result.is_success(), "u64::MAX-nonce tx should succeed: {res_skip:?}");
    assert!(res_applied.result.is_success(), "applied-auth tx should succeed: {res_applied:?}");

    assert_eq!(u_skip.state_growth, 0, "a u64::MAX-nonce authority creates no state growth");
    assert_eq!(u_applied.state_growth, 1, "an applied net-new authority creates one");
    assert_eq!(
        u_applied.data_size - u_skip.data_size,
        ACCOUNT_INFO_WRITE_SIZE,
        "an applied authority charges exactly one account write more than a u64::MAX-nonce one",
    );
    assert_eq!(
        u_applied.kv_updates - u_skip.kv_updates,
        1,
        "an applied authority charges exactly one more KV update than a u64::MAX-nonce one",
    );
}

/// An authorization whose signature does not recover to any authority is skipped before any
/// account read and charges no per-applied resources.
///
/// Compared against an applied authorization (same record count), the applied run charges exactly
/// one more account write. This exercises the recoverability gate independently of the chain-id,
/// nonce, and code gates.
#[test]
fn test_rex6_unrecoverable_authority_skipped() {
    let envs = no_heavy_buckets();

    let (res_skip, u_skip) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth_unrecoverable(1, 0)]),
    );
    let (res_applied, u_applied) = transact(
        MegaSpecId::REX6,
        &mut funded_db(),
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0)]),
    );
    assert!(res_skip.result.is_success(), "unrecoverable-auth tx should succeed: {res_skip:?}");
    assert!(res_applied.result.is_success(), "applied-auth tx should succeed: {res_applied:?}");

    assert_eq!(u_skip.state_growth, 0, "an unrecoverable authority creates no state growth");
    assert_eq!(u_applied.state_growth, 1, "an applied net-new authority creates one");
    assert_eq!(
        u_applied.data_size - u_skip.data_size,
        ACCOUNT_INFO_WRITE_SIZE,
        "an applied authority charges exactly one account write more than an unrecoverable one",
    );
    assert_eq!(
        u_applied.kv_updates - u_skip.kv_updates,
        1,
        "an applied authority charges exactly one more KV update than an unrecoverable one",
    );
}
