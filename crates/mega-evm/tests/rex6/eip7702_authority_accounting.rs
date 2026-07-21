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

/// A net-new authority that overflows the state-growth limit halts the REX6 tx AND is never
/// applied. The halt is a HALT (not `Err`), so `apply_eip7702_auth_list`'s pre-frame `mark_touch`
/// would otherwise persist; the guard skips applying the list. Pins: halt fires, and `AUTHORITY_A`
/// is neither nonce-bumped nor delegated.
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
    assert_eq!(
        usage.state_growth, 1,
        "validate()'s accounting still records the attempted authority creation",
    );
    assert!(
        res.state.get(&CALLEE).is_none_or(|a| !a.is_touched()),
        "the first frame must not start once pre-frame state growth already exceeds the limit",
    );
    // The account may still show up in `res.state` as a cold LoadedAsNotExisting read-touch from
    // `validate()`'s accounting scan — that's not a delegation. The invariant is: not delegated
    // (no EIP-7702 code) and not nonce-bumped.
    let authority_after = res.state.get(&AUTHORITY_A);
    assert!(
        authority_after.is_none_or(
            |a| a.info.nonce == 0 && a.info.code.as_ref().is_none_or(|c| !c.is_eip7702())
        ),
        "the authorization must NOT be applied once state growth is already known to \
         exceed the limit, got {authority_after:?}",
    );
}

/// The skip is all-or-nothing: once the tx's authority creations exceed `TX_STATE_GROWTH_LIMIT`,
/// the ENTIRE list is skipped, not just the authority that crossed it. Two net-new authorities,
/// limit 1: both must be left untouched (nonce 0, no delegation), not only the second.
#[test]
fn test_rex6_authority_state_growth_overflow_skips_all_authorities() {
    let (res, usage) = transact_with_limits(
        MegaSpecId::REX6,
        &mut funded_db(),
        &no_heavy_buckets(),
        EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(1),
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0), auth(AUTHORITY_B, 1, 0)]),
    );

    assert!(
        matches!(
            &res.result,
            ExecutionResult::Halt {
                reason: MegaHaltReason::StateGrowthLimitExceeded { limit: 1, actual: 2 },
                ..
            }
        ),
        "REX6 must halt when applied authorities overflow the state-growth limit: {res:?}",
    );
    assert_eq!(
        usage.state_growth, 2,
        "validate()'s accounting still records both attempted authority creations",
    );
    for authority in [AUTHORITY_A, AUTHORITY_B] {
        let after = res.state.get(&authority);
        assert!(
            after.is_none_or(|a| {
                a.info.nonce == 0 && a.info.code.as_ref().is_none_or(|c| !c.is_eip7702())
            }),
            "authority {authority} must not be materialized once state growth already \
             exceeds the limit, got {after:?}",
        );
    }
}

/// Sibling of the overflow tests: a tx whose authority creations stay within the state-growth
/// budget must still apply normally (the state-growth guard must not fire on a non-overflowing tx).
#[test]
fn test_rex6_authority_state_growth_within_limit_still_applies() {
    let (res, usage) = transact_with_limits(
        MegaSpecId::REX6,
        &mut funded_db(),
        &no_heavy_buckets(),
        EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(1),
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0)]),
    );

    assert!(res.result.is_success(), "a tx within the state-growth budget must succeed: {res:?}");
    assert_eq!(usage.state_growth, 1, "the single net-new authority is recorded");
    let authority = res.state.get(&AUTHORITY_A).expect("authority update should be preserved");
    assert_eq!(authority.info.nonce, 1, "the authorization must still apply when within budget");
    assert!(authority.info.code.as_ref().is_some_and(|c| c.is_eip7702()), "delegation must be set");
}

/// When several dimensions overflow at once, the skip fires regardless of which one the halt
/// reports. Here KV (limit 0) and state growth (limit 1, two net-new authorities) both exceed; KV
/// is checked first so `KVUpdateLimitExceeded` is the reported reason, but the guard — reading the
/// aggregate `has_exceeded_limit` — still skips the whole list and materializes neither authority.
#[test]
fn test_rex6_authority_state_growth_overflow_detected_even_when_kv_limit_reported() {
    let (res, usage) = transact_with_limits(
        MegaSpecId::REX6,
        &mut funded_db(),
        &no_heavy_buckets(),
        EvmTxRuntimeLimits::no_limits().with_tx_kv_updates_limit(0).with_tx_state_growth_limit(1),
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0), auth(AUTHORITY_B, 1, 0)]),
    );

    // KV is checked before StateGrowth in the priority order and its limit (0) is exceeded by
    // intrinsic usage alone, so the reported halt reason is KVUpdateLimitExceeded, not
    // StateGrowthLimitExceeded.
    assert!(
        matches!(
            &res.result,
            ExecutionResult::Halt { reason: MegaHaltReason::KVUpdateLimitExceeded { .. }, .. }
        ),
        "expected the KV dimension to be the reported halt reason: {res:?}",
    );
    // The intrinsic KV exceed latched in `on_new_tx` short-circuits the validate-time authority
    // scan entirely (no authority account is even loaded — see
    // `test_rex6_authority_scan_skipped_when_pre_frame_limit_latched`), so no per-authority
    // state growth is recorded for the doomed transaction.
    assert_eq!(usage.state_growth, 0, "the latched pre-frame exceed must skip the authority scan");
    for authority in [AUTHORITY_A, AUTHORITY_B] {
        let after = res.state.get(&authority);
        assert!(
            after.is_none_or(|a| {
                a.info.nonce == 0 && a.info.code.as_ref().is_none_or(|c| !c.is_eip7702())
            }),
            "authority {authority} must not be materialized even though a DIFFERENT \
             dimension (KV) is the reported halt reason, got {after:?}",
        );
    }
}

/// The skip must fire for ANY pre-frame authority-accounting overflow, not just state growth.
/// `on_rex6_eip7702_authority_applied` records a KV + `DataSize` write for *every* applied
/// authority but state growth only for net-new ones — so a tx full of *existing* authorities grows
/// state by 0 yet exceeds a tight `tx_kv_updates_limit`. The guard must still skip the whole list;
/// otherwise those authorities are applied and their pre-frame writes persist past the KV HALT,
/// breaking the per-tx KV limit exactly like the state-growth case. A state-growth-only guard
/// misses this.
#[test]
fn test_rex6_authority_kv_overflow_skips_all_authorities() {
    // Both authorities already exist → applying each is 0 state growth but +1 KV update. Two of
    // them exceed a KV limit of 1 with no state growth at all.
    let mut db = funded_db()
        .account_balance(AUTHORITY_A, U256::from(1u64))
        .account_balance(AUTHORITY_B, U256::from(1u64));
    let (res, usage) = transact_with_limits(
        MegaSpecId::REX6,
        &mut db,
        &no_heavy_buckets(),
        EvmTxRuntimeLimits::no_limits().with_tx_kv_updates_limit(1),
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0), auth(AUTHORITY_B, 1, 0)]),
    );

    assert!(
        matches!(
            &res.result,
            ExecutionResult::Halt { reason: MegaHaltReason::KVUpdateLimitExceeded { .. }, .. }
        ),
        "existing authorities exceeding the KV limit must halt on KV: {res:?}",
    );
    assert_eq!(usage.state_growth, 0, "existing authorities grow no state — only the KV dimension");
    for authority in [AUTHORITY_A, AUTHORITY_B] {
        let after = res.state.get(&authority);
        assert!(
            after.is_none_or(|a| {
                a.info.nonce == 0 && a.info.code.as_ref().is_none_or(|c| !c.is_eip7702())
            }),
            "authority {authority} must not be applied on a KV overflow with 0 state growth (the \
             guard must cover KV/DataSize, not just state growth), got {after:?}",
        );
    }
}

/// The fourth pre-frame dimension: `compute_gas`. An applied authority that is the block
/// beneficiary lowers the compute-gas cap (REX4 beneficiary detention) inside
/// `record_rex6_eip7702_authority_accounting`, and the tx's own EIP-7702 intrinsic compute
/// (recorded via `record_compute_gas(initial_gas)` right after) then exceeds that cap — a pre-frame
/// compute overflow with no DataSize/KV/state-growth overflow. The guard must still skip the whole
/// list; a check that enumerated only DataSize/KV/state-growth (or only state-growth) would miss it
/// and let the beneficiary authority persist past the `ComputeGasLimitExceeded` HALT.
#[test]
fn test_rex6_authority_compute_overflow_skips_authorities() {
    const TX_COMPUTE_LIMIT: u64 = 200_000_000;
    // Detention cap far below the tx's EIP-7702 intrinsic compute (~46k for one authorization).
    const TINY_DETENTION_CAP: u64 = 1_000;

    // BENEFICIARY is funded (exists), so `auth(BENEFICIARY, 1, 0)` applies and — being the block
    // beneficiary — triggers detention.
    let mut db = funded_db().account_balance(BENEFICIARY, U256::from(1u64));
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(10_000_000)
        .authorization_list_recovered(vec![auth(BENEFICIARY, 1, 0)])
        .build_fill();

    let (res, _detained) =
        transact_detention(MegaSpecId::REX6, &mut db, TX_COMPUTE_LIMIT, TINY_DETENTION_CAP, tx);

    // The intrinsic compute (~46k) exceeds the detained cap (1k). In this beneficiary-detention
    // context that surfaces as `VolatileDataAccessOutOfGas`, but it is the same pre-frame
    // compute-over-cap that latches `has_exceeded_limit` — which is what the guard reads.
    assert!(
        matches!(
            &res.result,
            ExecutionResult::Halt { reason: MegaHaltReason::VolatileDataAccessOutOfGas { .. }, .. }
        ),
        "the beneficiary-detention compute overflow must halt: {res:?}",
    );
    let after = res.state.get(&BENEFICIARY);
    assert!(
        after.is_none_or(|a| {
            a.info.nonce == 0 && a.info.code.as_ref().is_none_or(|c| !c.is_eip7702())
        }),
        "the beneficiary authority must not be applied on a compute overflow (the guard must cover \
         compute_gas too), got {after:?}",
    );
}

/// The fourth pre-frame dimension: `data_size`. `on_rex6_eip7702_authority_applied` charges the
/// applied authority's account write (+40) to `data_size` on top of the intrinsic TX base +
/// calldata + authorization-record size that `before_tx_start` already recorded, so a single
/// net-new authority against a tight `tx_data_size_limit` overflows before any frame is pushed —
/// with state growth and KV both comfortably within their (unset) limits. The guard must still
/// skip the whole list; a check that missed the data-size lane would let the authority persist
/// past the `DataLimitExceeded` HALT.
#[test]
fn test_rex6_authority_data_size_overflow_skips_authorities() {
    // One applied, net-new authorization: `before_tx_start` records BASE_TX_SIZE (110) + one
    // AUTHORIZATION_SIZE record (101) + the caller account update (40) = 251, then
    // `on_rex6_eip7702_authority_applied` adds the authority's own account write (40) = 291. A
    // limit of 290 makes data size the exceeded dimension.
    let (res, usage) = transact_with_limits(
        MegaSpecId::REX6,
        &mut funded_db(),
        &no_heavy_buckets(),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(290),
        tx_with_auths(vec![auth(AUTHORITY_A, 1, 0)]),
    );

    assert!(
        matches!(
            &res.result,
            ExecutionResult::Halt {
                reason: MegaHaltReason::DataLimitExceeded { limit: 290, actual: 291 },
                ..
            }
        ),
        "REX6 must halt when an applied authority overflows the data-size limit: {res:?}",
    );
    assert_eq!(
        usage.data_size, 291,
        "validate()'s accounting still records the attempted authority write",
    );
    let authority_after = res.state.get(&AUTHORITY_A);
    assert!(
        authority_after.is_none_or(
            |a| a.info.nonce == 0 && a.info.code.as_ref().is_none_or(|c| !c.is_eip7702())
        ),
        "the authorization must NOT be applied once data size is already known to exceed the \
         limit, got {authority_after:?}",
    );
}

/// Fee semantics: skipping the whole auth list forgoes the EIP-7702 refund an already-existing
/// authority would have earned (`PER_EMPTY_ACCOUNT_COST - PER_AUTH_BASE_COST` = `12_500`, recorded
/// unconditionally by `post_execution::refund` and subtracted from `gas_used`).
///
/// Differential: two runs, same two-authority list, both overflow and skip — differing ONLY in
/// whether `AUTHORITY_A` pre-exists (i.e. would be refund-eligible). `gas_used` must be identical;
/// applying the pre-existing authority would drop the existing-`A` run by `12_500`. (Absolute
/// `gas_used` isn't hardcoded — the soft frame-boundary halt charges only spent gas.)
#[test]
fn test_rex6_authority_state_growth_overflow_forgoes_refund() {
    let auths = vec![auth(AUTHORITY_A, 1, 0), auth(AUTHORITY_B, 1, 0)];
    let overflow_limits = || EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(0);

    // Run 1: AUTHORITY_A already exists (its application would earn the existing-account refund).
    let mut db_existing = funded_db().account_balance(AUTHORITY_A, U256::from(1u64));
    let (res_existing, _) = transact_with_limits(
        MegaSpecId::REX6,
        &mut db_existing,
        &no_heavy_buckets(),
        overflow_limits(),
        tx_with_auths(auths.clone()),
    );

    // Run 2: AUTHORITY_A does not exist (never refund-eligible).
    let (res_fresh, _) = transact_with_limits(
        MegaSpecId::REX6,
        &mut funded_db(),
        &no_heavy_buckets(),
        overflow_limits(),
        tx_with_auths(auths),
    );

    // Both overflow the zero state-growth limit and halt (existing-A grows by 1 via B; fresh-A by 2
    // via A and B) — the `actual` differs but both exceed the limit.
    for (label, res) in [("existing-A", &res_existing), ("fresh-A", &res_fresh)] {
        assert!(
            matches!(
                &res.result,
                ExecutionResult::Halt {
                    reason: MegaHaltReason::StateGrowthLimitExceeded { limit: 0, .. },
                    ..
                }
            ),
            "{label} run must halt on the state-growth overflow: {res:?}",
        );
    }

    // The whole list is skipped in both, so the pre-existing authority's 12_500 refund is forgone:
    // gas_used is identical. Were it applied, only the existing-A run would drop by 12_500.
    assert_eq!(
        res_existing.result.gas_used(),
        res_fresh.result.gas_used(),
        "skipping the whole auth list forgoes the pre-existing authority's EIP-7702 refund, so \
         gas_used must not differ on account of A's pre-existence",
    );

    // And the existing authority is not delegated either — the skip is total.
    let a_after = res_existing.state.get(&AUTHORITY_A);
    assert!(
        a_after.is_none_or(
            |a| a.info.nonce == 0 && a.info.code.as_ref().is_none_or(|c| !c.is_eip7702())
        ),
        "the existing authority must not be delegated, got {a_after:?}",
    );
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

/// A type-4 tx whose intrinsic data alone exceeds the TX data-size limit is doomed to the
/// first-frame `DataLimitExceeded` halt before any authorization applies; the REX6
/// validate-time authority scan must not load authority accounts for it (stateless replay
/// carries no witness entries for authorizations that will never be applied). Pinned by
/// injecting a DB error on the authority read.
#[test]
fn test_rex6_authority_scan_skipped_when_pre_frame_limit_latched() {
    use mega_evm::test_utils::ErrorInjectingDatabase;

    let inner = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_account = Some(AUTHORITY_A);

    // A data-size limit smaller than the base TX size: `on_new_tx` latches the exceed before
    // `validate()` reaches the authorization scan.
    let mut context = MegaContext::new(db, MegaSpecId::REX6)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(10));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx_with_auths(vec![auth(AUTHORITY_A, 0, 0)]));
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx).expect(
        "the latched pre-frame exceed must skip the authority scan, not surface a DB error",
    );
    assert!(
        matches!(
            r.result,
            revm::context::result::ExecutionResult::Halt {
                reason: MegaHaltReason::DataLimitExceeded { .. },
                ..
            }
        ),
        "must halt on the data-size limit: {:?}",
        r.result
    );
}

/// A net-new authority priced in a saturated SALT bucket must be rejected at validation
/// (`initial_gas` folds the authority's dynamic account-creation gas, which here saturates
/// toward `u64::MAX`), not wrap `initial_gas` around and execute with too little prepaid gas.
#[test]
fn test_rex6_saturated_authority_salt_gas_rejects_instead_of_wrapping() {
    let mut db = funded_db();
    let bucket = SimpleBucketHasher::bucket_id(AUTHORITY_A.as_slice());
    let envs: Envs = TestExternalEnvs::new().with_bucket_capacity(bucket, u64::MAX);

    let r = try_transact(
        MegaSpecId::REX6,
        &mut db,
        &envs,
        tx_with_auths(vec![auth(AUTHORITY_A, 0, 0)]),
    );
    assert!(
        r.is_err(),
        "a saturated authority SALT price must fail validation, not execute: {r:?}"
    );
}
