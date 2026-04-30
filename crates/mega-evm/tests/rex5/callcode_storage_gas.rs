//! Tests for Rex5's fix to `CALLCODE` new-account storage gas metering.
//!
//! Pre-Rex5, the storage-gas wrapper for `CALLCODE` checked emptiness and charged
//! `new_account_storage_gas` against the stack `to` address — the code-source. For
//! `CALLCODE`, however, execution happens in the caller's account context, so the
//! storage account being potentially "created" is the caller's, not the code-source.
//! Charging against the code-source can charge new-account storage gas spuriously
//! when the code-source happens to be empty.
//!
//! Rex5 changes the wrapper to meter new-account storage gas against
//! `interpreter.input.target_address()` (the caller / current frame). The stack
//! `to` is still used as the code-source for the underlying `CALLCODE` instruction.
//! Pre-Rex5 specs preserve their (frozen) prior behavior.
//!
//! `CALL` behavior is unchanged across all specs: the stack `to` is the value
//! recipient and is the correct address for emptiness / new-account metering.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants::rex::NEW_ACCOUNT_STORAGE_GAS_BASE,
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, InjectedDbError, MemoryDatabase},
    BucketId, EVMError, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvs, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError, SaltEnv, TestExternalEnvs,
    MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CALLCODE, STOP},
    context::{result::ResultAndState, TxEnv},
};

const CALLER: Address = address!("2000000000000000000000000000000000000001");
const CALLEE: Address = address!("1000000000000000000000000000000000000001");
/// An address that is not present in the database — i.e. an empty account.
const EMPTY_TARGET: Address = address!("3000000000000000000000000000000000000001");

/// Builds bytecode that performs `CALLCODE(gas=GAS, target, value=1, args=[], ret=[])`
/// followed by `STOP`. The CALL stipend covers gas inside the (empty-code) callee.
fn callcode_bytecode(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value = 1 wei
        .push_address(target)
        .push_number(100_000_u64) // gas
        .append(CALLCODE)
        .append(STOP)
        .build()
}

/// Builds bytecode that performs `CALL(gas=GAS, target, value=1, args=[], ret=[])`
/// followed by `STOP`.
fn call_bytecode(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value = 1 wei
        .push_address(target)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(STOP)
        .build()
}

#[allow(clippy::too_many_arguments)]
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: &TestExternalEnvs,
    caller: Address,
    callee: Address,
    value: U256,
    gas_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context =
        MegaContext::new(db, spec).with_external_envs(external_envs.into()).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_data_size_limit(u64::MAX)
                .with_tx_kv_updates_limit(u64::MAX),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller,
        kind: TxKind::Call(callee),
        data: Bytes::new(),
        value,
        gas_limit,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// Runs the given bytecode on `spec` with a configurable bucket multiplier for the
/// empty target. Returns the transaction's `gas_used`.
fn run_with_target_multiplier(spec: MegaSpecId, bytecode: Bytes, target_multiplier: u64) -> u64 {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_balance(CALLEE, U256::from(1_000_000_000u64))
        .account_code(CALLEE, bytecode);

    let target_bucket = TestExternalEnvs::<Infallible>::bucket_id_for_account(EMPTY_TARGET);
    let external_envs = TestExternalEnvs::new()
        .with_bucket_capacity(target_bucket, MIN_BUCKET_SIZE as u64 * target_multiplier);

    let result = transact(spec, &mut db, &external_envs, CALLER, CALLEE, U256::ZERO, 10_000_000)
        .expect("transaction must succeed");
    assert!(result.result.is_success(), "execution must succeed: {:?}", result.result);
    result.result.gas_used()
}

// ============================================================================
// CALLCODE: Rex5 fix — no new-account storage gas charged
// ============================================================================

/// Under Rex5, a value-transferring `CALLCODE` to an empty code-source must NOT
/// charge new-account storage gas, because the storage context is the (non-empty)
/// caller contract.
#[test]
fn test_rex5_callcode_to_empty_no_new_account_storage_gas() {
    let bytecode = callcode_bytecode(EMPTY_TARGET);
    let gas_mult1 = run_with_target_multiplier(MegaSpecId::REX5, bytecode.clone(), 1);
    let gas_mult10 = run_with_target_multiplier(MegaSpecId::REX5, bytecode, 10);

    assert_eq!(
        gas_mult10, gas_mult1,
        "Rex5 CALLCODE must not charge new-account storage gas based on the code-source bucket",
    );
}

// ============================================================================
// CALLCODE: Pre-Rex5 frozen behavior — bug preserved
// ============================================================================

/// Pre-Rex5 (Rex4) preserves the original (buggy) behavior: a value-transferring
/// `CALLCODE` to an empty code-source charges new-account storage gas based on the
/// code-source's bucket. This test pins that behavior so a future regression in
/// stable-spec semantics is caught.
#[test]
fn test_rex4_callcode_to_empty_charges_new_account_storage_gas() {
    let bytecode = callcode_bytecode(EMPTY_TARGET);
    let gas_mult1 = run_with_target_multiplier(MegaSpecId::REX4, bytecode.clone(), 1);
    let gas_mult10 = run_with_target_multiplier(MegaSpecId::REX4, bytecode, 10);

    let expected_extra = NEW_ACCOUNT_STORAGE_GAS_BASE * 9;
    assert_eq!(
        gas_mult10 - gas_mult1,
        expected_extra,
        "Rex4 (frozen) must keep charging new-account storage gas against the code-source bucket",
    );
}

// ============================================================================
// CALL: behavior unchanged — value-transferring CALL to empty target still charges
// ============================================================================

/// Under Rex5, a value-transferring `CALL` to an empty target still charges
/// new-account storage gas based on the target's bucket. The fix is scoped to
/// `CALLCODE` only; `CALL` semantics are unchanged.
#[test]
fn test_rex5_call_to_empty_still_charges_new_account_storage_gas() {
    let bytecode = call_bytecode(EMPTY_TARGET);
    let gas_mult1 = run_with_target_multiplier(MegaSpecId::REX5, bytecode.clone(), 1);
    let gas_mult10 = run_with_target_multiplier(MegaSpecId::REX5, bytecode, 10);

    let expected_extra = NEW_ACCOUNT_STORAGE_GAS_BASE * 9;
    assert_eq!(
        gas_mult10 - gas_mult1,
        expected_extra,
        "Rex5 CALL must continue to charge new-account storage gas against the target bucket",
    );
}

/// Pre-Rex5 (Rex4) `CALL` behavior is unchanged: value-transferring CALL to an
/// empty target charges new-account storage gas based on the target's bucket.
#[test]
fn test_rex4_call_to_empty_charges_new_account_storage_gas() {
    let bytecode = call_bytecode(EMPTY_TARGET);
    let gas_mult1 = run_with_target_multiplier(MegaSpecId::REX4, bytecode.clone(), 1);
    let gas_mult10 = run_with_target_multiplier(MegaSpecId::REX4, bytecode, 10);

    let expected_extra = NEW_ACCOUNT_STORAGE_GAS_BASE * 9;
    assert_eq!(
        gas_mult10 - gas_mult1,
        expected_extra,
        "Rex4 CALL must charge new-account storage gas against the target bucket",
    );
}

// ============================================================================
// Error-path tests — coverage for FatalExternalError branches in call_code
// ============================================================================

/// A SALT environment that always fails `get_bucket_capacity`, triggering the
/// `new_account_storage_gas` → `None` → `FatalExternalError` path in `call_code`.
#[derive(Debug)]
struct FailingSaltEnv;

impl SaltEnv for FailingSaltEnv {
    type Error = String;

    fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, String> {
        Err("injected salt error".into())
    }

    fn bucket_id_for_account(_account: Address) -> BucketId {
        0
    }

    fn bucket_id_for_slot(_address: Address, _key: U256) -> BucketId {
        0
    }
}

fn transact_with_error_db(
    spec: MegaSpecId,
    db: ErrorInjectingDatabase,
    caller: Address,
    callee: Address,
    gas_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<InjectedDbError, MegaTransactionError>> {
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let mut context =
        MegaContext::new(db, spec).with_external_envs(external_envs.into()).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_data_size_limit(u64::MAX)
                .with_tx_kv_updates_limit(u64::MAX),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller,
        kind: TxKind::Call(callee),
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

fn transact_with_failing_salt(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    caller: Address,
    callee: Address,
    gas_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let envs: ExternalEnvs<(FailingSaltEnv, EmptyExternalEnv)> =
        ExternalEnvs { salt_env: FailingSaltEnv, oracle_env: EmptyExternalEnv };
    let mut context = MegaContext::new(db, spec).with_external_envs(envs).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(u64::MAX)
            .with_tx_kv_updates_limit(u64::MAX),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller,
        kind: TxKind::Call(callee),
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// When `inspect_account_delegated` fails during CALLCODE (in `storage_gas_ext::call_code`),
/// the EVM should halt with `FatalExternalError` and return `EVMError::Custom`.
/// Under Rex4, the storage address is the code-source (stack `to` = `EMPTY_TARGET`).
#[test]
fn test_callcode_db_error_on_inspect_account() {
    let bytecode = callcode_bytecode(EMPTY_TARGET);
    let inner_db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_balance(CALLEE, U256::from(1_000_000_000u64))
        .account_code(CALLEE, bytecode);

    let mut db = ErrorInjectingDatabase::new(inner_db);
    db.fail_on_account = Some(EMPTY_TARGET);

    let result = transact_with_error_db(MegaSpecId::REX4, db, CALLER, CALLEE, 1_000_000);

    match result {
        Err(EVMError::Custom(msg)) => {
            assert!(
                msg.contains("injected basic()"),
                "error message should contain injected error, got: {msg}"
            );
        }
        Err(other) => panic!("expected EVMError::Custom, got: {other:?}"),
        Ok(result) => panic!("expected error, got success: {:?}", result.result),
    }
}

/// When the SALT environment fails during `new_account_storage_gas` inside CALLCODE
/// (in `storage_gas_ext::call_code`), the EVM should halt with `FatalExternalError`.
/// This path is reached under Rex4 when the code-source is empty and value is non-zero.
#[test]
fn test_callcode_salt_error_on_new_account_storage_gas() {
    let bytecode = callcode_bytecode(EMPTY_TARGET);
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_balance(CALLEE, U256::from(1_000_000_000u64))
        .account_code(CALLEE, bytecode);

    let result = transact_with_failing_salt(MegaSpecId::REX4, &mut db, CALLER, CALLEE, 1_000_000);

    match result {
        Err(EVMError::Custom(msg)) => {
            assert!(
                msg.contains("injected salt error"),
                "error message should contain salt error, got: {msg}"
            );
        }
        Err(other) => panic!("expected EVMError::Custom, got: {other:?}"),
        Ok(result) => panic!("expected error, got success: {:?}", result.result),
    }
}
