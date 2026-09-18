//! A database error during execution fails the transaction with that error; it is never a halt
//! the transaction is included with.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, InjectedDbError, MemoryDatabase},
    MegaEvm, MegaTransaction,
};
use revm::{
    bytecode::opcode::{CALL, STATICCALL},
    context::result::EVMError,
};

use crate::common::{call, context, create};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CALLEE: Address = address!("1000000000000000000000000000000000000001");
const TARGET: Address = address!("3000000000000000000000000000000000000003");

fn transact(
    db: ErrorInjectingDatabase,
    tx: MegaTransaction,
) -> Result<
    revm::context::result::ResultAndState<mega_evm::MegaHaltReason>,
    EVMError<InjectedDbError, mega_evm::MegaTransactionError>,
> {
    MegaEvm::new(context(db)).transact_raw(tx)
}

fn assert_db_error<T: core::fmt::Debug>(
    result: Result<T, EVMError<InjectedDbError, mega_evm::MegaTransactionError>>,
    expected: &str,
) {
    match result {
        Err(EVMError::Database(error)) => {
            assert!(format!("{error}").contains(expected), "got: {error}");
        }
        other => panic!("expected a database error, got: {other:?}"),
    }
}

fn funded(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(100_000_000_000u64))
        .account_code(CALLEE, code)
}

/// A failing storage read in `SSTORE` surfaces as the database error.
#[test]
fn test_sstore_storage_read_error_fails_the_transaction() {
    let key = U256::from(0x42);
    let code = BytecodeBuilder::default().sstore(key, U256::from(1)).stop().build();
    let mut db = ErrorInjectingDatabase::new(funded(code));
    db.fail_on_storage = Some((CALLEE, key));
    assert_db_error(
        transact(db, call(CALLER, CALLEE, U256::ZERO, 1_000_000)),
        "injected storage()",
    );
}

fn call_target(opcode: u8) -> Bytes {
    let code = BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64);
    let code = if opcode == CALL { code.push_number(1_u64) } else { code };
    code.push_address(TARGET).push_number(100_000_u64).append(opcode).stop().build()
}

/// A failing account read of a value transfer's recipient surfaces as the database error.
#[test]
fn test_value_call_account_read_error_fails_the_transaction() {
    let mut db = ErrorInjectingDatabase::new(funded(call_target(CALL)));
    db.fail_on_account = Some(TARGET);
    assert_db_error(transact(db, call(CALLER, CALLEE, U256::ZERO, 1_000_000)), "injected basic()");
}

/// A creation that writes storage never reads the created account's storage from the database:
/// it is empty, and a stateless witness has nothing for it.
#[test]
fn test_created_account_storage_is_never_read_from_the_database() {
    let init_code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(0x42)).stop().build();
    let created = CALLER.create(0);
    let inner = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(100_000_000_000u64))
        .account_balance(created, U256::from(1));
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_storage = Some((created, U256::ZERO));
    let result = transact(db, create(CALLER, init_code, 10_000_000)).expect("no database read");
    assert!(result.result.is_success(), "{:?}", result.result);
}

/// A failing account read of a `STATICCALL` target surfaces as the database error.
#[test]
fn test_staticcall_account_read_error_fails_the_transaction() {
    let mut db = ErrorInjectingDatabase::new(funded(call_target(STATICCALL)));
    db.fail_on_account = Some(TARGET);
    assert_db_error(transact(db, call(CALLER, CALLEE, U256::ZERO, 1_000_000)), "injected basic()");
}
