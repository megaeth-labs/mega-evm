//! REX6 CREATE/CREATE2/SSTORE error-path coverage for the unified-metering refactor.
//!
//! The REX6 canonical-metering rewrite of `storage_gas_ext` moved the storage-gas charge of
//! CREATE (`create_rex6`) and SSTORE behind fresh early-return guards, and split CREATE2 operand
//! validation into the shared `compute_created_address` helper. These tests pin the early-return
//! branches that the happy-path parity tests do not reach:
//!
//! - **CREATE storage-gas DB error** (`create_rex6`): `create_contract_storage_gas` returning
//!   `None` (a SALT-env failure) must halt with `FatalExternalError` and surface as
//!   `EVMError::Custom`, not silently charge zero storage gas.
//! - **SSTORE storage-gas DB error** (`sstore`): the zero→non-zero `sstore_set_storage_gas` failure
//!   must do the same.
//! - **CREATE2 operand underflow** (`compute_created_address`): a CREATE2 missing its `offset` or
//!   `length` operand must halt with `StackUnderflow` from the wrapper's own `stack.inspect::<1>()`
//!   / `inspect::<2>()`, before any memory expansion.

use std::convert::Infallible;

use alloy_primitives::{Address, Bytes, TxKind, U256};
use mega_evm::{
    revm::context::result::ResultAndState,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BucketId, EVMError, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvs, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError, SaltEnv,
};
use revm::{
    bytecode::opcode::{CREATE, CREATE2, SSTORE, STOP},
    context::TxEnv,
};

use crate::common::{transact_default, CALLER, CONTRACT};

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// A SALT environment whose `get_bucket_capacity` always fails, so any dynamic storage-gas lookup
/// (`create_contract_storage_gas`, `sstore_set_storage_gas`, …) returns `None` and drives the
/// `FatalExternalError` early-return branch under test.
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

/// Runs `code` at [`CONTRACT`] under `spec` with a SALT env that fails every bucket lookup, so the
/// first dynamic storage-gas charge surfaces as `EVMError::Custom`.
fn transact_with_failing_salt(
    spec: MegaSpecId,
    code: Bytes,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code);

    let envs: ExternalEnvs<(FailingSaltEnv, EmptyExternalEnv)> =
        ExternalEnvs { salt_env: FailingSaltEnv, oracle_env: EmptyExternalEnv };
    let mut context =
        MegaContext::new(&mut db, spec).with_external_envs(envs).with_tx_runtime_limits(
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
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit: 100_000_000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

fn assert_injected_salt_custom_error(
    result: Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>>,
) {
    match result {
        Err(EVMError::Custom(msg)) => {
            assert!(
                msg.contains("injected salt error"),
                "error should carry the injected SALT failure, got: {msg}"
            );
        }
        Err(other) => panic!("expected EVMError::Custom, got: {other:?}"),
        Ok(result) => panic!("expected a fatal DB-error halt, got success: {:?}", result.result),
    }
}

/// `create_rex6` must surface a SALT-env failure on `create_contract_storage_gas` as a fatal
/// external error rather than charging zero storage gas for the new contract.
#[test]
fn test_rex6_create_salt_error_on_create_contract_storage_gas() {
    // CREATE(value=0, offset=0, length=0): empty initcode, so the address is computed and the
    // contract-creation storage-gas charge is reached without needing any memory contents.
    let code = BytecodeBuilder::default()
        .push_number(0u64) // length
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(STOP)
        .build();

    assert_injected_salt_custom_error(transact_with_failing_salt(MegaSpecId::REX6, code));
}

/// REX6 SSTORE of a zero→non-zero slot must surface a SALT-env failure on `sstore_set_storage_gas`
/// as a fatal external error rather than charging zero storage gas for the set.
#[test]
fn test_rex6_sstore_salt_error_on_sstore_set_storage_gas() {
    // SSTORE(key=0, value=1) into a fresh slot: original == present == 0, new != 0, so the
    // dynamic `sstore_set_storage_gas` charge fires.
    let code = BytecodeBuilder::default()
        .push_number(1u64) // value
        .push_number(0u64) // key
        .append(SSTORE)
        .append(STOP)
        .build();

    assert_injected_salt_custom_error(transact_with_failing_salt(MegaSpecId::REX6, code));
}

fn is_stack_underflow(result: &crate::common::Outcome) -> bool {
    matches!(&result.result, revm::context::result::ExecutionResult::Halt { reason, .. }
        if format!("{reason:?}").contains("StackUnderflow"))
}

/// CREATE2 with only the `value` operand on the stack must halt with `StackUnderflow` from the
/// wrapper's `stack.inspect::<1>()` (the `offset` operand) inside `compute_created_address`,
/// before any memory expansion.
#[test]
fn test_rex6_create2_missing_offset_stack_underflow() {
    // One operand on the stack (value); `offset` is absent.
    let code = BytecodeBuilder::default().push_number(0u64).append(CREATE2).append(STOP).build();

    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code);
    let outcome = transact_default(MegaSpecId::REX6, db);

    assert!(is_stack_underflow(&outcome), "expected StackUnderflow, got {:?}", outcome.result);
}

/// CREATE2 with only `value` and `offset` on the stack must halt with `StackUnderflow` from the
/// wrapper's `stack.inspect::<2>()` (the `length` operand) inside `compute_created_address`.
#[test]
fn test_rex6_create2_missing_length_stack_underflow() {
    // Two operands on the stack (offset, value); `length` is absent.
    let code = BytecodeBuilder::default()
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(STOP)
        .build();

    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code);
    let outcome = transact_default(MegaSpecId::REX6, db);

    assert!(is_stack_underflow(&outcome), "expected StackUnderflow, got {:?}", outcome.result);
}
