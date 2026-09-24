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
