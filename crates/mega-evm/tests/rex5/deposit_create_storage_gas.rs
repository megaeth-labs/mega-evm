#![allow(clippy::doc_markdown)]
//! Tests for top-level CREATE storage-gas pricing.
//!
//! Under `MegaSpecId::REX5`, `MegaHandler::validate` derives the CREATE
//! address from the caller's state nonce (read via `journal.inspect_account`,
//! which does not warm the caller in the EIP-2929 access list or push a
//! journal entry), matching `make_create_frame`. Pre-REX5 keeps the
//! `tx.nonce()`-based pricing for replay determinism.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    constants, test_utils::MemoryDatabase, BucketHasher, EvmTxRuntimeLimits, MegaContext, MegaEvm,
    MegaSpecId, MegaTransaction, SimpleBucketHasher, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{context::TxEnv, database::Database as _};

const CALLER: Address = address!("1111111111111111111111111111111111111111");

/// Caller's actual on-chain nonce. The contract WILL deploy at
/// `CALLER.create(STATE_NONCE)`.
const STATE_NONCE: u64 = 5;

/// Nonce field carried by the deposit envelope. Different from STATE_NONCE,
/// which is the whole point: under op-revm a deposit's `tx.nonce` is not
/// validated against state.
const TX_NONCE: u64 = 0;

/// Multiplier 100 → storage gas = `CONTRACT_CREATION_STORAGE_GAS_BASE * (100 - 1)`
/// = 32_000 * 99 = 3_168_000 gas. The cheap (default) bucket has multiplier 1
/// → storage gas = 0. The 3.17M-gas spread is what the test observes.
const HEAVY_MULTIPLIER: u64 = 100;
const HEAVY_CAPACITY: u64 = (MIN_BUCKET_SIZE as u64) * HEAVY_MULTIPLIER;
const EXPECTED_HEAVY_STORAGE_GAS: u64 =
    constants::rex::CONTRACT_CREATION_STORAGE_GAS_BASE * (HEAVY_MULTIPLIER - 1);

/// Builds external envs where the bucket containing the address derived
/// from `nonce_for_heavy_bucket` is configured for the heavy multiplier.
/// Every other bucket falls through to the default (multiplier = 1).
fn envs_with_heavy_bucket_at(
    nonce_for_heavy_bucket: u64,
) -> TestExternalEnvs<Infallible, SimpleBucketHasher> {
    let heavy_address = CALLER.create(nonce_for_heavy_bucket);
    let heavy_bucket = SimpleBucketHasher::bucket_id(heavy_address.as_slice());
    TestExternalEnvs::new().with_bucket_capacity(heavy_bucket, HEAVY_CAPACITY)
}

/// Builds a deposit-style top-level CREATE transaction.
///
/// `tx.nonce` is the envelope nonce (does not need to match state). The
/// `deposit.source_hash != ZERO` field is what makes op-revm classify this
/// as a deposit and skip nonce validation.
fn deposit_create_tx(tx_nonce: u64, gas_limit: u64) -> MegaTransaction {
    let tx_env = TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        nonce: tx_nonce,
        gas_limit,
        gas_price: 0, // deposits don't pay gas
        data: Bytes::new(),
        value: U256::ZERO,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_env);
    tx.deposit.source_hash = B256::from([0x42; 32]);
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Builds a normal (non-deposit) top-level CREATE transaction. `tx.nonce`
/// must match `state.nonce` for `validate_against_state` to pass.
fn normal_create_tx(state_nonce: u64, gas_limit: u64) -> MegaTransaction {
    let tx_env = TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        nonce: state_nonce,
        gas_limit,
        gas_price: 0,
        data: Bytes::new(),
        value: U256::ZERO,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn run(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    envs: &TestExternalEnvs<Infallible, SimpleBucketHasher>,
    tx: MegaTransaction,
) -> revm::context::result::ResultAndState<mega_evm::MegaHaltReason> {
    let mut context = MegaContext::new(db, spec)
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact should not surface EVMError")
}

/// Pre-state with caller account at the chosen state nonce.
fn seed_caller(state_nonce: u64) -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(10_000_000_000_000_000_000u128));
    db.set_account_nonce(CALLER, state_nonce);
    db
}

/// Deposit CREATE storage gas is priced at `caller.create(state_nonce)`,
/// not `caller.create(tx.nonce)`. Heavy SALT on the state-nonce bucket
/// makes the gas charge observable.
#[test]
fn test_deposit_create_storage_gas_uses_state_nonce() {
    let mut db = seed_caller(STATE_NONCE);
    let envs = envs_with_heavy_bucket_at(STATE_NONCE);
    let gas_limit = 50_000_000;
    let tx = deposit_create_tx(TX_NONCE, gas_limit);

    let res = run(MegaSpecId::REX5, &mut db, &envs, tx);

    assert!(
        res.result.is_success(),
        "Deposit CREATE must succeed under REX5 with sufficient gas; got: {:?}",
        res.result
    );

    // Sanity: the deployed address comes from the state nonce.
    let deployed_address = match &res.result {
        revm::context::result::ExecutionResult::Success { output, .. } => match output {
            revm::context::result::Output::Create(_, Some(addr)) => *addr,
            other => panic!("Expected Create output with address, got: {other:?}"),
        },
        _ => panic!("not Success"),
    };
    assert_eq!(deployed_address, CALLER.create(STATE_NONCE));
    assert_ne!(deployed_address, CALLER.create(TX_NONCE));

    let gas_used = res.result.gas_used();
    assert!(
        gas_used >= EXPECTED_HEAVY_STORAGE_GAS,
        "expected gas_used ≥ {EXPECTED_HEAVY_STORAGE_GAS} (heavy-bucket storage gas), got \
         {gas_used}",
    );
}

/// Same diverging-nonce setup under REX4: pre-REX5 prices storage gas at
/// `caller.create(tx.nonce)`, which lands in the default cheap bucket.
#[test]
fn test_pre_rex5_preserves_tx_nonce_storage_gas_pricing() {
    let mut db = seed_caller(STATE_NONCE);
    let envs = envs_with_heavy_bucket_at(STATE_NONCE);
    let gas_limit = 50_000_000;
    let tx = deposit_create_tx(TX_NONCE, gas_limit);

    let res = run(MegaSpecId::REX4, &mut db, &envs, tx);

    assert!(res.result.is_success(), "deposit CREATE must succeed: {:?}", res.result);
    let gas_used = res.result.gas_used();
    assert!(
        gas_used < EXPECTED_HEAVY_STORAGE_GAS,
        "expected gas_used < {EXPECTED_HEAVY_STORAGE_GAS} (cheap bucket), got {gas_used}",
    );
}

/// Non-deposit CREATE under REX5: `tx.nonce == state.nonce`, so the
/// state-nonce read returns the same value as `tx.nonce()` did.
#[test]
fn test_non_deposit_create_storage_gas_unchanged_under_rex5() {
    let mut db = seed_caller(STATE_NONCE);
    let envs = envs_with_heavy_bucket_at(STATE_NONCE);
    let gas_limit = 50_000_000;
    let tx = normal_create_tx(STATE_NONCE, gas_limit);

    let res = run(MegaSpecId::REX5, &mut db, &envs, tx);

    assert!(res.result.is_success(), "CREATE must succeed: {:?}", res.result);
    let gas_used = res.result.gas_used();
    assert!(gas_used >= EXPECTED_HEAVY_STORAGE_GAS, "got gas_used={gas_used}");
}

/// When `state_nonce == tx_nonce == 0`, the state-nonce read produces
/// the same address as `tx.nonce()`; REX4 and REX5 must agree on the
/// receipt.
#[test]
fn test_deposit_create_with_state_nonce_zero_unchanged() {
    let envs = envs_with_heavy_bucket_at(0);
    let gas_limit = 50_000_000;

    let mut db_rex4 = seed_caller(0);
    let res_rex4 = run(MegaSpecId::REX4, &mut db_rex4, &envs, deposit_create_tx(0, gas_limit));

    let mut db_rex5 = seed_caller(0);
    let res_rex5 = run(MegaSpecId::REX5, &mut db_rex5, &envs, deposit_create_tx(0, gas_limit));

    assert_eq!(res_rex4.result.is_success(), res_rex5.result.is_success());
    assert_eq!(res_rex4.result.gas_used(), res_rex5.result.gas_used());
}

/// Confirms `validate()` reads the caller through the canonical pipeline:
/// the state delta carries a bumped caller nonce and the deployed
/// contract account.
#[test]
fn test_deposit_create_caller_state_nonce_is_bumped_after_create() {
    let mut db = seed_caller(STATE_NONCE);
    let envs = envs_with_heavy_bucket_at(STATE_NONCE);
    let gas_limit = 50_000_000;
    let tx = deposit_create_tx(TX_NONCE, gas_limit);

    let res = run(MegaSpecId::REX5, &mut db, &envs, tx);
    assert!(res.result.is_success(), "deposit CREATE should succeed: {:?}", res.result);

    let caller_account = res.state.get(&CALLER).expect("caller account must be in state delta");
    assert_eq!(caller_account.info.nonce, STATE_NONCE + 1);
    let deployed = CALLER.create(STATE_NONCE);
    assert!(res.state.contains_key(&deployed), "deployed contract must be in state delta");
    assert!(
        db.basic(deployed).unwrap().is_none_or(|info| info.is_empty()),
        "db should be untouched; assertions read only the in-memory delta",
    );
}
