//! Tests for DB error handling via `JournalInspectTr`.
//!
//! Verifies that when the database returns an error during `inspect_account_delegated` or
//! `inspect_storage`, the EVM properly halts with `FatalExternalError` and surfaces the error
//! as `EVMError::Custom`.

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, InjectedDbError, MemoryDatabase},
    EVMError, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};
use revm::{
    bytecode::opcode::CALL,
    context::{result::ResultAndState, TxEnv},
    primitives::Address,
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CALLEE: Address = address!("1000000000000000000000000000000000000001");
const TARGET: Address = address!("3000000000000000000000000000000000000003");

fn transact(
    spec: MegaSpecId,
    db: ErrorInjectingDatabase,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
    gas_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<InjectedDbError, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// When `inspect_storage` fails during SSTORE (in `additional_limit_ext::sstore`),
/// the EVM should halt with `FatalExternalError` and return `EVMError::Custom`.
#[test]
fn test_sstore_db_error_on_inspect_storage() {
    let storage_key = U256::from(0x42);
    let storage_value = U256::from(0x1);

    let bytecode = BytecodeBuilder::default().sstore(storage_key, storage_value).stop().build();

    let mut inner_db = MemoryDatabase::default();
    inner_db.set_account_balance(CALLER, U256::from(100_000_000_000u64));
    inner_db.set_account_code(CALLEE, bytecode);

    let mut db = ErrorInjectingDatabase::new(inner_db);
    // Fail when the SSTORE instruction's `inspect_storage` tries to read this slot.
    db.fail_on_storage = Some((CALLEE, storage_key));

    let result = transact(
        MegaSpecId::MINI_REX,
        db,
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO,
        1_000_000,
    );

    match result {
        Err(EVMError::Custom(msg)) => {
            assert!(
                msg.contains("injected storage()"),
                "error message should contain injected error, got: {msg}"
            );
        }
        Err(other) => panic!("expected EVMError::Custom, got: {other:?}"),
        Ok(result) => panic!("expected error, got success: {:?}", result.result),
    }
}

/// When `inspect_account_delegated` fails during CALL with value transfer
/// (in `wrap_call_with_storage_gas!`), the EVM should halt with `FatalExternalError`.
#[test]
fn test_call_with_transfer_db_error_on_inspect_account() {
    // Build bytecode that does CALL with value transfer to TARGET.
    // CALL args: gas, addr, value, argsOffset, argsSize, retOffset, retSize
    let bytecode = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value (non-zero to trigger value transfer path)
        .push_address(TARGET)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .stop()
        .build();

    let mut inner_db = MemoryDatabase::default();
    inner_db.set_account_balance(CALLER, U256::from(100_000_000_000u64));
    inner_db.set_account_code(CALLEE, bytecode);

    let mut db = ErrorInjectingDatabase::new(inner_db);
    // Fail when the CALL instruction's `inspect_account_delegated` tries to load TARGET.
    db.fail_on_account = Some(TARGET);

    let result = transact(
        MegaSpecId::MINI_REX,
        db,
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO,
        1_000_000,
    );

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

/// When `inspect_account_delegated` fails during STATICCALL (in `wrap_call_with_storage_gas!`),
/// the EVM should halt with `FatalExternalError`.
/// This tests a different code path from CALL-with-transfer: STATICCALL has no value parameter
/// but still calls `inspect_account_delegated` on the target to check `is_empty`.
///
/// Uses REX spec because `wrap_call_with_storage_gas!` (which calls `inspect_account_delegated`)
/// is only wired for STATICCALL starting from REX (MINI_REX uses `compute_gas_ext::static_call`
/// which delegates directly to revm's handler without inspecting first).
#[test]
fn test_staticcall_db_error_on_inspect_account() {
    // STATICCALL stack layout: gas, addr, argsOffset, argsSize, retOffset, retSize
    let bytecode = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(TARGET) // addr
        .push_number(100_000_u64) // gas
        .append(revm::bytecode::opcode::STATICCALL)
        .stop()
        .build();

    let mut inner_db = MemoryDatabase::default();
    inner_db.set_account_balance(CALLER, U256::from(100_000_000_000u64));
    inner_db.set_account_code(CALLEE, bytecode);

    let mut db = ErrorInjectingDatabase::new(inner_db);
    // Fail when STATICCALL's `inspect_account_delegated` tries to load TARGET.
    db.fail_on_account = Some(TARGET);

    let result =
        transact(MegaSpecId::REX, db, CALLER, Some(CALLEE), Bytes::new(), U256::ZERO, 1_000_000);

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
