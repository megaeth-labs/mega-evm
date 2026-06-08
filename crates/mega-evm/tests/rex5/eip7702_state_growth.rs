//! Tests for EIP-7702 authority state-growth tracking.
//!
//! REX5 adds state-growth accounting for EIP-7702 authority accounts during pre-execution.
//! Only valid authorizations whose authority account was not already present count as state growth.
//!
//! Pre-REX5, EIP-7702 authority accounts are not counted toward state growth.

use alloy_eips::eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction,
};
use revm::{
    context::{
        result::{ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
    handler::EvmTr,
    state::Bytecode,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000800000");
const CALLEE: Address = address!("0000000000000000000000000000000000800001");
const AUTHORITY_A: Address = address!("0000000000000000000000000000000000800010");
const AUTHORITY_B: Address = address!("0000000000000000000000000000000000800011");

// ============================================================================
// HELPERS
// ============================================================================

fn transact_with_limits(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
    limits: EvmTxRuntimeLimits,
) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(limits);
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

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    transact_with_limits(spec, db, tx, EvmTxRuntimeLimits::from_spec(spec))
}

// ============================================================================
// TESTS
// ============================================================================

/// A type-4 transaction with one new authority should count +1 state growth in REX5.
///
/// The authorization is valid and the authority account does not exist before auth processing.
#[test]
fn test_rex5_new_authority_state_growth() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
        RecoveredAuthority::Valid(AUTHORITY_A),
    )];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    assert_eq!(usage.state_growth, 1, "state growth should include the new authority account");

    let authority_account = result.state.get(&AUTHORITY_A).expect("authority should be updated");
    assert_eq!(authority_account.info.nonce, 1, "successful auth should increment authority nonce");
    assert_eq!(
        authority_account.info.code_hash,
        Bytecode::new_eip7702(delegate).hash_slow(),
        "successful auth should install EIP-7702 delegation code",
    );
    assert!(
        authority_account.info.code.as_ref().is_some_and(|code| code.is_eip7702()),
        "successful auth should write EIP-7702 bytecode",
    );
}

/// The same transaction under REX4 should NOT include authority accounts in state growth.
///
/// Pre-REX5, EIP-7702 authority accounts are not counted toward state growth.
#[test]
fn test_rex4_single_authority_no_state_growth() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
        RecoveredAuthority::Valid(AUTHORITY_A),
    )];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    // REX4 does not count authority accounts in state growth.
    assert_eq!(
        usage.state_growth, 0,
        "REX4 should not count authority accounts in state growth: {}",
        usage.state_growth
    );
}

/// REX5 with two new authorities should count +2 state growth.
#[test]
fn test_rex5_two_new_authorities_state_growth() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![
        RecoveredAuthorization::new_unchecked(
            Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
            RecoveredAuthority::Valid(AUTHORITY_A),
        ),
        RecoveredAuthorization::new_unchecked(
            Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
            RecoveredAuthority::Valid(AUTHORITY_B),
        ),
    ];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    assert_eq!(usage.state_growth, 2, "state growth should include both new authority accounts");
}

/// Repeated authorizations for the same new authority should only count the authority once.
///
/// The first authorization creates the authority account and increments its nonce.
/// The second authorization has the original nonce, so it is skipped by EIP-7702 auth processing.
#[test]
fn test_rex5_duplicate_new_authority_counts_once() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![
        RecoveredAuthorization::new_unchecked(
            Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
            RecoveredAuthority::Valid(AUTHORITY_A),
        ),
        RecoveredAuthorization::new_unchecked(
            Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
            RecoveredAuthority::Valid(AUTHORITY_A),
        ),
    ];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    assert_eq!(usage.state_growth, 1, "duplicate authority must only count once");
}

/// REX5 should not count state growth when a valid EIP-7702 authority already exists.
///
/// Delegating an existing authority modifies that account but does not create a net-new state
/// entry.
#[test]
fn test_rex5_existing_authority_no_state_growth() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
        RecoveredAuthority::Valid(AUTHORITY_A),
    )];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64))
        .account_balance(AUTHORITY_A, U256::from(1u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    assert_eq!(usage.state_growth, 0, "existing authority must not count as state growth");

    let authority_account =
        result.state.get(&AUTHORITY_A).expect("authority should still be updated");
    assert_eq!(
        authority_account.info.nonce, 1,
        "valid authorization should increment the existing authority nonce",
    );
    assert_eq!(
        authority_account.info.code_hash,
        Bytecode::new_eip7702(delegate).hash_slow(),
        "valid authorization should still install delegation code on an existing authority",
    );
}

/// REX5 should not count state growth for a resolved authority whose authorization is invalid.
#[test]
fn test_rex5_nonce_mismatch_authority_no_state_growth() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: delegate, nonce: 1 },
        RecoveredAuthority::Valid(AUTHORITY_A),
    )];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    assert_eq!(usage.state_growth, 0, "invalid authorization must not count as state growth");
    assert!(
        result.state.get(&AUTHORITY_A).is_none_or(|account| !account.is_touched()),
        "nonce-mismatched authority should not be modified",
    );
}

/// An authorization with an invalid (unrecoverable) authority should NOT count
/// toward state growth, since `authority()` returns `None`.
#[test]
fn test_rex5_invalid_authority_no_state_growth() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
        RecoveredAuthority::Invalid,
    )];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    assert_eq!(
        usage.state_growth, 0,
        "invalid authority should not count toward state growth: {}",
        usage.state_growth
    );
    assert!(!result.state.contains_key(&AUTHORITY_A), "invalid authority should not touch state");
}

/// A valid authority with non-EIP-7702 code should be skipped and should not count as state
/// growth.
#[test]
fn test_rex5_non_eip7702_authority_no_state_growth() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
        RecoveredAuthority::Valid(AUTHORITY_A),
    )];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));
    db.set_account_code(AUTHORITY_A, BytecodeBuilder::default().stop().build());

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    assert_eq!(usage.state_growth, 0, "non-EIP-7702 authority code must not count as state growth");
    assert!(
        result.state.get(&AUTHORITY_A).is_none_or(|account| !account.is_touched()),
        "non-EIP-7702 authority should not be modified",
    );
}

/// A new authority can overflow the state-growth limit before the first frame starts.
///
/// The EIP-7702 auth list is still applied during pre-execution, but the outer call halts before
/// the first frame is initialized.
#[test]
fn test_rex5_new_authority_state_growth_limit_exceeded_before_first_frame() {
    let delegate = address!("0000000000000000000000000000000000900001");
    let authorization_list = vec![RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
        RecoveredAuthority::Valid(AUTHORITY_A),
    )];

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(CALLEE, U256::from(1u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .authorization_list_recovered(authorization_list)
        .build_fill();

    let (result, usage) = transact_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx,
        EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(0),
    );

    assert!(matches!(
        &result.result,
        ExecutionResult::Halt {
            reason: MegaHaltReason::StateGrowthLimitExceeded { limit: 0, actual: 1 },
            ..
        }
    ));
    assert_eq!(usage.state_growth, 1, "authority creation should still be recorded before halt");
    assert!(
        result.state.get(&CALLEE).is_none_or(|account| !account.is_touched()),
        "first frame should not start once pre-execution state growth already exceeds the limit",
    );

    let authority_account =
        result.state.get(&AUTHORITY_A).expect("pre-execution authority update should be preserved");
    assert_eq!(
        authority_account.info.nonce, 1,
        "auth-list processing should still increment the authority nonce before the halt",
    );
    assert_eq!(
        authority_account.info.code_hash,
        Bytecode::new_eip7702(delegate).hash_slow(),
        "auth-list processing should still write the delegation code before the halt",
    );
}
