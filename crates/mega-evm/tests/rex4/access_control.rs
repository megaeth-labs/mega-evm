//! Tests for the `MegaAccessControl` system contract.
//!
//! When a contract calls `ACCESS_CONTROL_ADDRESS.disableVolatileDataAccess()`, the caller's
//! frame and all inner calls that access volatile data will revert with
//! `VolatileDataAccessDisabled()` error.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IMegaAccessControl, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError, TestExternalEnvs, VolatileDataAccessType, ACCESS_CONTROL_ADDRESS,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
        ContextTr, TxEnv,
    },
    interpreter::{CallInputs, CallOutcome, InterpreterTypes},
    Inspector,
};

// Test addresses
const CALLER: Address = address!("0000000000000000000000000000000000200000");
const PARENT: Address = address!("0000000000000000000000000000000000200001");
const CHILD: Address = address!("0000000000000000000000000000000000200002");
const GRANDCHILD: Address = address!("0000000000000000000000000000000000200003");
const SIBLING: Address = address!("0000000000000000000000000000000000200004");

/// The 4-byte selector for `disableVolatileDataAccess()`.
const DISABLE_VOLATILE_DATA_ACCESS_SELECTOR: [u8; 4] =
    IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

/// The 4-byte selector for `enableVolatileDataAccess()`.
const ENABLE_VOLATILE_DATA_ACCESS_SELECTOR: [u8; 4] =
    IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR;

/// The 4-byte selector for `isVolatileDataAccessDisabled()`.
const IS_VOLATILE_DATA_ACCESS_DISABLED_SELECTOR: [u8; 4] =
    IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR;

/// The 4-byte selector for `VolatileDataAccessDisabled()` error.
const VOLATILE_DATA_ACCESS_DISABLED_SELECTOR: [u8; 4] =
    IMegaAccessControl::VolatileDataAccessDisabled::SELECTOR;

/// The 4-byte selector for `DisabledByParent()` error.
const DISABLED_BY_PARENT_SELECTOR: [u8; 4] = IMegaAccessControl::DisabledByParent::SELECTOR;

/// The 4-byte selector for `NotIntercepted()` error.
const NOT_INTERCEPTED_SELECTOR: [u8; 4] = IMegaAccessControl::NotIntercepted::SELECTOR;

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Decodes `VolatileDataAccessDisabled(uint8 accessType)` from revert data.
fn decode_volatile_data_access_disabled(
    data: &[u8],
) -> IMegaAccessControl::VolatileDataAccessDisabled {
    <IMegaAccessControl::VolatileDataAccessDisabled as SolError>::abi_decode(data)
        .expect("valid VolatileDataAccessDisabled revert data")
}

/// Executes a transaction on Rex4 spec.
fn transact(
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, MegaSpecId::REX4);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// Builds a default transaction calling a contract.
fn default_tx(to: Address) -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(to).gas_limit(100_000_000).build_fill()
}

/// Builds bytecode that calls `disableVolatileDataAccess()` on the access control contract.
fn call_disable_volatile_data_access(builder: BytecodeBuilder) -> BytecodeBuilder {
    // Store selector in memory
    let builder = builder.mstore(0x0, DISABLE_VOLATILE_DATA_ACCESS_SELECTOR);
    // CALL(gas, addr, value, argsOffset, argsSize, retOffset, retSize)
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize (4-byte selector)
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP) // discard success flag
}

/// Builds bytecode that calls `enableVolatileDataAccess()` on the access control contract.
fn call_enable_volatile_data_access(builder: BytecodeBuilder) -> BytecodeBuilder {
    let builder = builder.mstore(0x0, ENABLE_VOLATILE_DATA_ACCESS_SELECTOR);
    builder
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(4_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64)
        .append(CALL)
        .append(POP)
}

/// Builds bytecode that calls `isVolatileDataAccessDisabled()` and returns the bool result.
/// The CALL return data (32-byte ABI-encoded bool) is returned as the frame output.
fn call_is_volatile_data_access_disabled(builder: BytecodeBuilder) -> BytecodeBuilder {
    let builder = builder.mstore(0x0, IS_VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    builder
        .push_number(32_u64) // retSize (32-byte bool)
        .push_number(0x20_u64) // retOffset (put return data at 0x20 to not overwrite calldata)
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64)
        .append(CALL)
        .append(POP) // discard success flag
        // Return the 32-byte bool output
        .push_number(32_u64) // size
        .push_number(0x20_u64) // offset
        .append(RETURN)
}

/// Builds bytecode that CALLs a target address with the given gas.
fn append_call(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

/// Builds bytecode that STATICCALLs a target address with the given gas.
fn append_staticcall(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(target)
        .push_number(gas)
        .append(STATICCALL)
}

/// Builds bytecode that DELEGATECALLs a target address with the given gas.
fn append_delegatecall(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(target)
        .push_number(gas)
        .append(DELEGATECALL)
}

/// Appends bytecode that emits a LOG0 with the top-of-stack value (expected: CALL success flag).
/// Consumes the top stack value.
fn append_log_call_status(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // memory offset
        .append(MSTORE) // MSTORE(0, success_flag)
        .push_number(32_u64) // size (32 bytes, the full word)
        .push_number(0_u64) // offset
        .append(LOG0)
}

/// Asserts that the log at the given index contains the expected CALL success flag.
fn assert_log_call_status(
    result: &ResultAndState<MegaHaltReason>,
    log_index: usize,
    expected_success: bool,
) {
    let logs = result.result.logs();
    assert!(
        logs.len() > log_index,
        "Expected at least {} log(s), got {}",
        log_index + 1,
        logs.len()
    );
    let value = U256::from_be_slice(logs[log_index].data.data.as_ref());
    let expected = if expected_success { U256::from(1) } else { U256::ZERO };
    assert_eq!(value, expected, "Log[{log_index}] call status: expected {expected}, got {value}");
}

/// Appends CALL that captures return data, then returns it.
fn append_call_and_return_data(
    builder: BytecodeBuilder,
    target: Address,
    gas: u64,
) -> BytecodeBuilder {
    append_call(builder, target, gas)
        .append(POP) // discard CALL success flag
        // Copy return data to memory
        .append(RETURNDATASIZE) // size
        .push_number(0_u64) // dataOffset
        .push_number(0_u64) // destOffset
        .append(RETURNDATACOPY)
        // Return the return data
        .append(RETURNDATASIZE)
        .push_number(0_u64) // offset
        .append(RETURN)
}

// ============================================================================
// 1. BASIC disableVolatileDataAccess() BEHAVIOR
// ============================================================================

/// Inner call that accesses TIMESTAMP should revert when volatile access is disabled.
#[test]
fn test_inner_call_timestamp_reverts() {
    // Child bytecode: just reads TIMESTAMP
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent bytecode: call disableVolatileDataAccess(), then call child, log call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");
    assert_log_call_status(&result, 0, false);
}

/// Caller's own frame IS restricted after calling `disableVolatileDataAccess()`.
#[test]
fn test_caller_frame_is_restricted() {
    // Parent bytecode: call disableVolatileDataAccess(), then read TIMESTAMP itself
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = parent_code.append(TIMESTAMP).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    // The parent should revert because its own frame is restricted after disabling.
    assert!(
        !result.result.is_success(),
        "Caller should revert when accessing TIMESTAMP after disabling, got: {:?}",
        result.result
    );
}

/// Inner call that does NOT access volatile data should succeed.
#[test]
fn test_inner_call_without_volatile_access_succeeds() {
    // Child bytecode: just does some arithmetic and stops
    let child_code = BytecodeBuilder::default()
        .push_number(1_u64)
        .push_number(2_u64)
        .append(ADD)
        .append(POP)
        .stop()
        .build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Inner call without volatile access should succeed");
    assert_log_call_status(&result, 0, true);
}

// ============================================================================
// 2. VOLATILE OPCODE COVERAGE
// ============================================================================

/// Test that multiple volatile opcodes each trigger revert in inner calls.
#[test]
fn test_volatile_opcodes_all_revert_in_inner_call() {
    let volatile_opcodes: &[(u8, &str)] = &[
        (TIMESTAMP, "TIMESTAMP"),
        (NUMBER, "NUMBER"),
        (COINBASE, "COINBASE"),
        (DIFFICULTY, "DIFFICULTY"),
        (GASLIMIT, "GASLIMIT"),
        (BASEFEE, "BASEFEE"),
        (BLOCKHASH, "BLOCKHASH"),
        (BLOBBASEFEE, "BLOBBASEFEE"),
        (BLOBHASH, "BLOBHASH"),
    ];

    for &(opcode, name) in volatile_opcodes {
        // Child code: execute the volatile opcode, then POP and STOP
        let child_code = if opcode == BLOCKHASH || opcode == BLOBHASH {
            // BLOCKHASH and BLOBHASH need an argument on the stack
            BytecodeBuilder::default().push_number(0_u64).append(opcode).append(POP).stop().build()
        } else {
            BytecodeBuilder::default().append(opcode).append(POP).stop().build()
        };

        // Parent: disable volatile access, call child, log call status
        let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
        let parent_code = append_call(parent_code, CHILD, 50_000_000);
        let parent_code = append_log_call_status(parent_code).stop().build();

        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(PARENT, parent_code)
            .account_code(CHILD, child_code);

        let result = transact(&mut db, default_tx(PARENT)).unwrap();
        assert!(result.result.is_success(), "Parent tx should succeed for opcode {name}");
        assert_log_call_status(&result, 0, false);
    }
}

// ============================================================================
// 3. REVERT DATA CONTAINS ABI-ENCODED ERROR
// ============================================================================

/// The revert data from an inner call that accesses volatile data should contain
/// the `VolatileDataAccessDisabled(uint8)` error with the correct access type.
#[test]
fn test_revert_data_contains_error_with_access_type() {
    // Child: reads TIMESTAMP (will revert)
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent: disable volatile access, call child, capture revert data
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR, "Selector mismatch");
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Timestamp,
        "Access type should be Timestamp (1)"
    );
}

// ============================================================================
// 4. NESTED CALLS
// ============================================================================

/// Volatile access restriction propagates through nested calls.
/// A → B (disableVolatileDataAccess) → C → D (reads TIMESTAMP) → reverts.
#[test]
fn test_nested_call_volatile_access_reverts() {
    // Grandchild: reads TIMESTAMP
    let grandchild_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Child: calls grandchild and logs grandchild's call status
    let child_code = append_call(BytecodeBuilder::default(), GRANDCHILD, 40_000_000);
    let child_code = append_log_call_status(child_code).stop().build();

    // Parent: disable volatile access, call child, log child's call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code)
        .account_code(GRANDCHILD, grandchild_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed even though grandchild reverted");
    // Child succeeded (returned to parent), but grandchild reverted.
    // Log[0] is from child logging grandchild's call status (0 = reverted).
    // Log[1] is from parent logging child's call status (1 = succeeded).
    assert_log_call_status(&result, 0, false);
    assert_log_call_status(&result, 1, true);
}

// ============================================================================
// 5. GAS ACCOUNTING
// ============================================================================

/// Reverted inner call returns gas to parent.
#[test]
fn test_reverted_inner_call_returns_gas() {
    // Child: reads TIMESTAMP (will revert immediately)
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent: disable volatile access, call child with limited gas, log call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 10_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");
    assert_log_call_status(&result, 0, false);

    // The gas used should be relatively small since child reverted immediately
    // and gas was returned to parent
    let gas_used = result.result.gas_used();
    assert!(gas_used < 5_000_000, "Gas used ({gas_used}) should be relatively small");
}

// ============================================================================
// 6. CALL VARIANTS
// ============================================================================

/// STATICCALL inner call that accesses volatile data also reverts.
#[test]
fn test_staticcall_also_restricted() {
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_staticcall(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed with STATICCALL");
    assert_log_call_status(&result, 0, false);
}

/// DELEGATECALL inner call that accesses volatile data also reverts.
#[test]
fn test_delegatecall_also_restricted() {
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_delegatecall(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed with DELEGATECALL");
    assert_log_call_status(&result, 0, false);
}

// ============================================================================
// 7. BACKWARD COMPATIBILITY
// ============================================================================

/// On Rex3, calling the access control address has no special effect (contract not deployed).
#[test]
fn test_pre_rex4_no_interception() {
    // On Rex3, the access control contract is not deployed, so calling it
    // should not activate any volatile access restrictions.
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent: attempt to call disableVolatileDataAccess, then call child, log call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    // Use Rex3 spec
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX3);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(default_tx(PARENT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    // The child should succeed (TIMESTAMP not blocked on Rex3)
    assert!(result.result.is_success(), "On Rex3, volatile access should NOT be disabled");
    assert_log_call_status(&result, 0, true);
}

// ============================================================================
// 8. CONDITIONAL VOLATILE OPCODES
// ============================================================================

/// BALANCE on a non-beneficiary address should NOT revert when volatile access is disabled,
/// because it does not actually access volatile data.
#[test]
fn test_balance_non_beneficiary_not_restricted() {
    // Child: BALANCE of a non-beneficiary address (CHILD itself)
    let child_code = BytecodeBuilder::default()
        .push_address(CHILD) // non-beneficiary address
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();

    // Parent: disable volatile access, then call child, log call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "BALANCE on non-beneficiary should not be restricted");
    assert_log_call_status(&result, 0, true);
}

/// BALANCE on the beneficiary address SHOULD revert when volatile access is disabled.
#[test]
fn test_balance_beneficiary_restricted() {
    // The default beneficiary is Address::ZERO in tests.
    let beneficiary = Address::ZERO;

    // Child: BALANCE of the beneficiary address
    let child_code = BytecodeBuilder::default()
        .push_address(beneficiary)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();

    // Parent: disable volatile access, call child, capture revert data
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    // The inner call should have reverted with VolatileDataAccessDisabled(Beneficiary)
    let output = result.result.output().expect("Should have output");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Beneficiary,
        "Access type should be Beneficiary (10)"
    );
}

/// EXTCODESIZE on a non-beneficiary address should NOT revert when volatile access is disabled.
#[test]
fn test_extcodesize_non_beneficiary_not_restricted() {
    // Child: EXTCODESIZE of a non-beneficiary address
    let child_code = BytecodeBuilder::default()
        .push_address(CHILD)
        .append(EXTCODESIZE)
        .append(POP)
        .stop()
        .build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "EXTCODESIZE on non-beneficiary should not be restricted");
    assert_log_call_status(&result, 0, true);
}

/// When parent accesses volatile data (TIMESTAMP) before calling `disableVolatileDataAccess()`,
/// the child should still be restricted when accessing the same volatile data type.
/// The pre-execution check in the instruction handler detects the disabled state regardless of
/// whether the bitmap bit is already set.
#[test]
fn test_parent_accesses_volatile_then_child_restricted() {
    // Child: reads TIMESTAMP (should revert because volatile access is disabled)
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent: read TIMESTAMP first, then disable volatile access, then call child
    let parent_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP);
    let parent_code = call_disable_volatile_data_access(parent_code);
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    // The inner call should have reverted with VolatileDataAccessDisabled(Timestamp)
    let output = result.result.output().expect("Should have output");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Timestamp,
        "Child should revert with access type Timestamp even though parent already accessed it"
    );
}

// ============================================================================
// 9. SIBLING CALL SCOPING
// ============================================================================

/// When C1 calls `disableVolatileDataAccess()` and returns, sibling C2's children should NOT be
/// restricted. The disable flag should be deactivated when C1's frame returns.
///
/// ```text
/// PARENT (depth 2) → C1 (depth 3) → disableVolatileDataAccess() → return
///                   → C2 (depth 3) → GRANDCHILD (depth 4) reads TIMESTAMP → should succeed
/// ```
#[test]
fn test_sibling_call_not_restricted() {
    // C1: calls disableVolatileDataAccess() and returns
    let c1_code = call_disable_volatile_data_access(BytecodeBuilder::default()).stop().build();

    // GRANDCHILD: reads TIMESTAMP (should succeed because C2 never disabled volatile access)
    let grandchild_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // C2 (SIBLING): calls GRANDCHILD and propagates the call result
    // CALL returns 1 on success, 0 on failure. Store to memory and return it.
    let c2_code = append_call(BytecodeBuilder::default(), GRANDCHILD, 40_000_000)
        // Stack: [success_flag]
        .push_number(0_u64) // memory offset
        .append(MSTORE) // MSTORE(offset=0, value=success_flag)
        .push_number(32_u64) // size
        .push_number(0_u64) // offset
        .append(RETURN)
        .build();

    // PARENT: call C1, then call C2 and return C2's output
    let parent_code = append_call(BytecodeBuilder::default(), CHILD, 50_000_000)
        .append(POP) // discard C1's success flag
        // Now call C2 and capture its return data
        .push_number(32_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(SIBLING)
        .push_number(50_000_000_u64) // gas
        .append(CALL)
        .append(POP) // discard C2's success flag
        // Return C2's return data (the GRANDCHILD call result stored at memory[0])
        .push_number(32_u64) // size
        .push_number(0_u64) // offset
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, c1_code) // C1
        .account_code(SIBLING, c2_code) // C2
        .account_code(GRANDCHILD, grandchild_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    // The return data should contain the GRANDCHILD call success flag (1 = success).
    let output = result.result.output().expect("Should have output");
    let grandchild_success = U256::from_be_slice(output.as_ref());
    assert_eq!(
        grandchild_success,
        U256::from(1),
        "GRANDCHILD should succeed: C2's child should NOT be restricted by C1's disableVolatileDataAccess()"
    );
}

// ============================================================================
// 10. enableVolatileDataAccess()
// ============================================================================

/// Same frame disables then enables, child can access volatile data.
#[test]
fn test_enable_after_disable_succeeds() {
    // Child: reads TIMESTAMP (should succeed because parent re-enabled)
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent: disable, then enable, then call child
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = call_enable_volatile_data_access(parent_code);
    let parent_code = append_call(parent_code, CHILD, 50_000_000)
        // CALL returns 1 on success, 0 on failure. Store and return.
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    let child_success = U256::from_be_slice(output.as_ref());
    assert_eq!(
        child_success,
        U256::from(1),
        "Child should succeed after parent re-enabled volatile data access"
    );
}

/// Parent disables, child tries to enable → reverts with `DisabledByParent()`.
#[test]
fn test_enable_by_child_reverts_when_parent_disabled() {
    // Child: tries to call enableVolatileDataAccess(), captures return data
    let child_code = call_enable_volatile_data_access(BytecodeBuilder::default());
    // enableVolatileDataAccess() call will have reverted. Capture the return data from it.
    // The enable CALL returns 0 on failure. Let's just return the enable call's return data.
    let child_code = child_code
        .append(RETURNDATASIZE)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    // Parent: disable volatile access, call child, capture child's return data
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    assert_eq!(
        output.as_ref(),
        &DISABLED_BY_PARENT_SELECTOR,
        "Child's enable call should revert with DisabledByParent()"
    );
}

/// Calling `enableVolatileDataAccess()` when not disabled is a no-op (succeeds).
#[test]
fn test_enable_when_not_disabled_is_noop() {
    // Parent: just call enableVolatileDataAccess() without prior disable
    let parent_code = call_enable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = parent_code.stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(
        result.result.is_success(),
        "enableVolatileDataAccess() when not disabled should succeed"
    );
}

// ============================================================================
// 11. isVolatileDataAccessDisabled()
// ============================================================================

/// Query returns false when no disable is active.
#[test]
fn test_query_returns_false_when_not_disabled() {
    // Parent: call isVolatileDataAccessDisabled() and return the result
    let parent_code = call_is_volatile_data_access_disabled(BytecodeBuilder::default()).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    let disabled = U256::from_be_slice(output.as_ref());
    assert_eq!(disabled, U256::ZERO, "Should return false when not disabled");
}

/// Query returns true when a parent disabled volatile data access.
#[test]
fn test_query_returns_true_when_parent_disabled() {
    // Child: call isVolatileDataAccessDisabled() and return the result
    let child_code = call_is_volatile_data_access_disabled(BytecodeBuilder::default()).build();

    // Parent: disable, then call child that queries
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    let disabled = U256::from_be_slice(output.as_ref());
    assert_eq!(disabled, U256::from(1), "Should return true when parent disabled");
}

/// Query returns true for the frame that called disable (caller IS now restricted).
#[test]
fn test_query_returns_true_for_disabling_frame() {
    // Parent: disable, then query isVolatileDataAccessDisabled() in same frame
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = call_is_volatile_data_access_disabled(parent_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    let disabled = U256::from_be_slice(output.as_ref());
    assert_eq!(
        disabled,
        U256::from(1),
        "Should return true when queried in the frame that called disable"
    );
}

// ============================================================================
// 12. DIRECT TX CALLS TO ACCESS CONTROL CONTRACT
// ============================================================================

/// Helper to build a TX that calls the access control contract directly with the given selector.
fn direct_access_control_tx(selector: &[u8; 4]) -> TxEnv {
    direct_access_control_tx_with_value(selector, U256::ZERO)
}

/// Helper to build a TX that calls the access control contract directly with selector and value.
fn direct_access_control_tx_with_value(selector: &[u8; 4], value: U256) -> TxEnv {
    TxEnvBuilder::default()
        .caller(CALLER)
        .call(ACCESS_CONTROL_ADDRESS)
        .value(value)
        .gas_limit(100_000_000)
        .data(Bytes::copy_from_slice(selector))
        .build_fill()
}

/// Direct TX calling `disableVolatileDataAccess()` should succeed without frame stack underflow.
#[test]
fn test_direct_tx_disable_volatile_data_access() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let result =
        transact(&mut db, direct_access_control_tx(&DISABLE_VOLATILE_DATA_ACCESS_SELECTOR))
            .unwrap();
    assert!(
        result.result.is_success(),
        "Direct TX to disableVolatileDataAccess should succeed, got: {:?}",
        result.result
    );
}

/// Direct TX calling `disableVolatileDataAccess()` with non-zero value should revert.
#[test]
fn test_direct_tx_disable_volatile_data_access_with_value_reverts() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let result = transact(
        &mut db,
        direct_access_control_tx_with_value(
            &DISABLE_VOLATILE_DATA_ACCESS_SELECTOR,
            U256::from(1_u64),
        ),
    )
    .unwrap();
    assert!(
        !result.result.is_success(),
        "Direct TX with non-zero value should revert, got: {:?}",
        result.result
    );
}

/// Direct TX calling `enableVolatileDataAccess()` should succeed without frame stack underflow.
#[test]
fn test_direct_tx_enable_volatile_data_access() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let result =
        transact(&mut db, direct_access_control_tx(&ENABLE_VOLATILE_DATA_ACCESS_SELECTOR)).unwrap();
    assert!(
        result.result.is_success(),
        "Direct TX to enableVolatileDataAccess should succeed, got: {:?}",
        result.result
    );
}

/// Direct TX calling `isVolatileDataAccessDisabled()` should succeed and return false.
#[test]
fn test_direct_tx_is_volatile_data_access_disabled() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let result =
        transact(&mut db, direct_access_control_tx(&IS_VOLATILE_DATA_ACCESS_DISABLED_SELECTOR))
            .unwrap();
    assert!(
        result.result.is_success(),
        "Direct TX to isVolatileDataAccessDisabled should succeed, got: {:?}",
        result.result
    );

    let output = result.result.output().expect("Should have output");
    let disabled = U256::from_be_slice(output.as_ref());
    assert_eq!(disabled, U256::ZERO, "Should return false when called directly by TX");
}

/// Direct TX with unknown selector should fall through and revert with `NotIntercepted()`.
#[test]
fn test_direct_tx_unknown_selector_falls_through_and_reverts_not_intercepted() {
    let unknown_selector = [0xde, 0xad, 0xbe, 0xef];
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(ACCESS_CONTROL_ADDRESS, mega_evm::ACCESS_CONTROL_CODE);

    let result = transact(&mut db, direct_access_control_tx(&unknown_selector)).unwrap();
    assert!(
        !result.result.is_success(),
        "Unknown selector should fall through and revert, got: {:?}",
        result.result
    );

    let output = result.result.output().expect("Should have output");
    assert_eq!(
        output.len(),
        4,
        "Fallback should return only NotIntercepted selector, got {} bytes",
        output.len()
    );
    assert_eq!(
        &output[..4],
        &NOT_INTERCEPTED_SELECTOR,
        "Unknown selector should revert with NotIntercepted()"
    );
}

// ============================================================================
// 13. INSPECTOR VISIBILITY FOR SYSTEM CONTRACT CALLS
// ============================================================================

/// An inspector that records all `call` and `call_end` invocations.
#[derive(Default)]
struct CallTrackingInspector {
    /// Target addresses seen in `call` hooks, in order.
    calls: Vec<Address>,
    /// Target addresses seen in `call_end` hooks, in order.
    call_ends: Vec<Address>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for CallTrackingInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.calls.push(inputs.target_address);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends.push(inputs.target_address);
    }
}

/// When an inspector is attached, it should see `call` and `call_end` for system contract
/// calls that are intercepted in `frame_init`, even though no interpreter frame is created.
#[test]
fn test_inspector_sees_system_contract_call() {
    // Child: simple arithmetic, no volatile access
    let child_code = BytecodeBuilder::default()
        .push_number(1_u64)
        .push_number(2_u64)
        .append(ADD)
        .append(POP)
        .stop()
        .build();

    // Parent: call disableVolatileDataAccess(), then call CHILD, log call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut inspector = CallTrackingInspector::default();
    let mut evm = MegaEvm::new(context).with_inspector(&mut inspector);
    let mut tx = MegaTransaction::new(default_tx(PARENT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success(), "Transaction should succeed");

    // Inspector should have seen 3 calls:
    // 1. Top-level call to PARENT
    // 2. PARENT -> ACCESS_CONTROL_ADDRESS (intercepted in frame_init)
    // 3. PARENT -> CHILD (normal call)
    assert_eq!(
        inspector.calls.len(),
        3,
        "Inspector should see 3 call hooks: top-level + access control + child, got: {:?}",
        inspector.calls
    );
    assert_eq!(inspector.calls[0], PARENT, "First call should be to PARENT");
    assert_eq!(
        inspector.calls[1], ACCESS_CONTROL_ADDRESS,
        "Second call should be to ACCESS_CONTROL_ADDRESS"
    );
    assert_eq!(inspector.calls[2], CHILD, "Third call should be to CHILD");

    // call_end should be invoked for all 3 calls, in reverse order
    assert_eq!(
        inspector.call_ends.len(),
        3,
        "Inspector should see 3 call_end hooks, got: {:?}",
        inspector.call_ends
    );
    assert_eq!(
        inspector.call_ends[0], ACCESS_CONTROL_ADDRESS,
        "First call_end should be ACCESS_CONTROL_ADDRESS (innermost, returned first)"
    );
    assert_eq!(inspector.call_ends[1], CHILD, "Second call_end should be CHILD");
    assert_eq!(inspector.call_ends[2], PARENT, "Third call_end should be PARENT (outermost)");

    assert_log_call_status(&result, 0, true);
}

// ============================================================================
// 14. GAS COST OF SYSTEM CONTRACT CALL
// ============================================================================

/// The system contract call intercepted in `frame_init` should only consume the CALL opcode
/// overhead (warm account access = 100 gas), not the `gas_limit` forwarded to the child frame.
/// The child frame's gas is fully refunded since the interception returns `Gas::new(gas_limit)`.
#[test]
fn test_system_contract_call_gas_cost() {
    // Parent bytecode:
    // 1. GAS                         — push gas_before
    // 2. CALL(100_000, ACCESS_CONTROL, 0, disableVolatileDataAccess selector)
    // 3. POP                         — discard CALL success flag
    // 4. GAS                         — push gas_after
    // 5. SWAP1                       — [gas_after, gas_before] -> [gas_before, gas_after]
    // 6. SUB                         — gas_before - gas_after = gas_consumed
    // 7. MSTORE at 0x20              — store gas_consumed
    // 8. RETURN 32 bytes from 0x20
    //
    // Note: the GAS opcode itself costs 2 gas. The measured delta includes the cost of
    // setting up the CALL arguments (pushes), the CALL opcode overhead, POP, and one GAS.
    // The CALL forwards 100,000 gas to the child frame, which should all come back.

    // First, store the selector in memory (needed for CALL input)
    let parent_code = BytecodeBuilder::default().mstore(0x0, DISABLE_VOLATILE_DATA_ACCESS_SELECTOR);

    let parent_code = parent_code
        .append(GAS) // gas_before (costs 2 gas, measured AFTER this)
        // Set up CALL arguments
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas to forward
        .append(CALL)
        .append(POP) // discard success flag
        .append(GAS) // gas_after
        // gas_before is below gas_after on the stack: [gas_before, gas_after]
        // SWAP1 to get [gas_after, gas_before], then SUB = gas_before - gas_after
        .append(SWAP1)
        .append(SUB) // gas_consumed = gas_before - gas_after
        // Store and return
        .push_number(0x20_u64)
        .append(MSTORE)
        .push_number(32_u64) // size
        .push_number(0x20_u64) // offset
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Transaction should succeed");

    let output = result.result.output().expect("Should have output");
    let gas_consumed = U256::from_be_slice(output.as_ref()).to::<u64>();

    // The gas consumed by the measured region includes:
    // - 7 PUSH instructions (3 gas each = 21 gas) for CALL arguments
    // - 1 PUSH20 for address (3 gas)
    // - CALL cold account access (2600 gas, since ACCESS_CONTROL_ADDRESS is first accessed here)
    // - POP (2 gas)
    // - GAS (2 gas)
    // - SWAP1 (3 gas)
    // Total overhead ≈ 2631 gas
    //
    // Critically, it should NOT include the 100,000 gas forwarded to the child frame,
    // because the intercepted call returns all gas.
    assert!(
        gas_consumed < 3000,
        "System contract call gas delta should be small (CALL overhead only), got: {gas_consumed}. \
         If this is close to 100,000, the child frame gas was not refunded."
    );

    // Sanity check: it should at least include the cold CALL cost (2600)
    assert!(
        gas_consumed >= 2600,
        "Gas consumed ({gas_consumed}) should be at least 2600 (cold CALL cost)"
    );
}

// ============================================================================
// 15. BLOCKED VOLATILE ACCESS DOES NOT POLLUTE TRACKER
// ============================================================================

/// When volatile data access is disabled and a child frame attempts to read volatile data
/// (causing it to revert), the `volatile_data_accessed` bitmap and `compute_gas_limit`
/// should NOT be modified.
/// The instruction handler reverts before the opcode executes, so no tracker state is changed.
#[test]
fn test_blocked_volatile_access_does_not_set_bitmap() {
    // Child: reads TIMESTAMP (will revert because volatile access is disabled)
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent: disable volatile access, call child (which reverts), log call status, then stop.
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let volatile_data_tracker = context.volatile_data_tracker.clone();

    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(default_tx(PARENT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");
    assert_log_call_status(&result, 0, false);

    // The blocked TIMESTAMP access should NOT have set the volatile_data_accessed bitmap.
    let tracker = volatile_data_tracker.borrow();
    assert!(
        !tracker.accessed(),
        "volatile_data_accessed should be empty after blocked access, got: {:?}",
        tracker.get_volatile_data_accessed()
    );
    assert!(
        tracker.get_compute_gas_limit().is_none(),
        "compute_gas_limit should not be set after blocked access, got: {:?}",
        tracker.get_compute_gas_limit()
    );
}

/// When volatile data access is disabled, BALANCE on the beneficiary should revert
/// and NOT pollute the `volatile_data_accessed` bitmap or `compute_gas_limit`.
#[test]
fn test_blocked_beneficiary_balance_does_not_set_bitmap() {
    let beneficiary = Address::ZERO;

    // Child: BALANCE of the beneficiary (will revert because volatile access is disabled)
    let child_code = BytecodeBuilder::default()
        .push_address(beneficiary)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();

    // Parent: disable volatile access, call child (which reverts), log call status, then stop.
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let volatile_data_tracker = context.volatile_data_tracker.clone();

    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(default_tx(PARENT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");
    assert_log_call_status(&result, 0, false);

    // The blocked BALANCE(beneficiary) access should NOT have set the bitmap.
    let tracker = volatile_data_tracker.borrow();
    assert!(
        !tracker.accessed(),
        "volatile_data_accessed should be empty after blocked beneficiary BALANCE, got: {:?}",
        tracker.get_volatile_data_accessed()
    );
    assert!(
        tracker.get_compute_gas_limit().is_none(),
        "compute_gas_limit should not be set after blocked beneficiary BALANCE, got: {:?}",
        tracker.get_compute_gas_limit()
    );
}

/// When volatile data access is disabled, SLOAD on the oracle contract should revert
/// and NOT pollute the `volatile_data_accessed` bitmap or `compute_gas_limit`.
#[test]
fn test_blocked_oracle_sload_does_not_set_bitmap() {
    // Oracle contract code: SLOAD(0) and return.
    // When called by an inner frame with volatile access disabled, this triggers the
    // pre-execution check in the SLOAD instruction handler and reverts.
    let oracle_code = BytecodeBuilder::default()
        .push_number(0_u64) // key = 0
        .append(SLOAD)
        .append(POP)
        .stop()
        .build();

    // Parent: disable volatile access, then call the oracle contract (which will revert),
    // log call status, then stop.
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, ORACLE_CONTRACT_ADDRESS, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let volatile_data_tracker = context.volatile_data_tracker.clone();

    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(default_tx(PARENT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");
    assert_log_call_status(&result, 0, false);

    // The blocked oracle SLOAD should NOT have set the bitmap.
    let tracker = volatile_data_tracker.borrow();
    assert!(
        !tracker.accessed(),
        "volatile_data_accessed should be empty after blocked oracle SLOAD, got: {:?}",
        tracker.get_volatile_data_accessed()
    );
    assert!(
        tracker.get_compute_gas_limit().is_none(),
        "compute_gas_limit should not be set after blocked oracle SLOAD, got: {:?}",
        tracker.get_compute_gas_limit()
    );
}

// ============================================================================
// 16. DELEGATECALL / CALLCODE TO ACCESS CONTROL CONTRACT
// ============================================================================

/// Builds bytecode that CALLCODEs a target address with the given gas and no calldata.
fn append_callcode(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(target)
        .push_number(gas)
        .append(CALLCODE)
}

/// Executes a transaction on Rex4 spec with oracle external environment.
fn transact_with_oracle(
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let external_envs = TestExternalEnvs::<Infallible>::new()
        .with_oracle_storage(U256::from(0), U256::from(0x1234));
    let mut context =
        MegaContext::new(db, MegaSpecId::REX4).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// DELEGATECALL to the access control contract should NOT be intercepted.
/// The underlying contract code executes and reverts with `NotIntercepted()`.
/// Subsequent inner calls that access volatile data should succeed.
#[test]
fn test_delegatecall_to_access_control_not_intercepted() {
    // Child: reads TIMESTAMP (should succeed — disable was never activated)
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent: DELEGATECALL access control with disableVolatileDataAccess selector,
    // then CALL child which reads TIMESTAMP.
    let parent_code = BytecodeBuilder::default().mstore(0x0, DISABLE_VOLATILE_DATA_ACCESS_SELECTOR);
    // DELEGATECALL(gas, addr, argsOffset, argsSize, retOffset, retSize)
    let parent_code = parent_code
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(DELEGATECALL)
        .append(POP); // discard success flag (should be 0 = failure due to NotIntercepted revert)
    let parent_code = append_call(parent_code, CHILD, 50_000_000)
        // Stack: [call_success]. Store and return.
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code)
        .account_code(ACCESS_CONTROL_ADDRESS, mega_evm::ACCESS_CONTROL_CODE);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    // Child CALL should have succeeded (returned 1)
    let output = result.result.output().expect("Should have output");
    let child_success = U256::from_be_slice(output.as_ref());
    assert_eq!(
        child_success,
        U256::from(1),
        "Child should succeed because DELEGATECALL to access control was not intercepted"
    );
}

/// CALLCODE to the access control contract should NOT be intercepted.
/// The underlying contract code executes and reverts with `NotIntercepted()`.
/// Subsequent inner calls that access volatile data should succeed.
#[test]
fn test_callcode_to_access_control_not_intercepted() {
    // Child: reads TIMESTAMP (should succeed — disable was never activated)
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // Parent: CALLCODE access control with disableVolatileDataAccess selector,
    // then CALL child which reads TIMESTAMP.
    let parent_code = BytecodeBuilder::default().mstore(0x0, DISABLE_VOLATILE_DATA_ACCESS_SELECTOR);
    // CALLCODE(gas, addr, value, argsOffset, argsSize, retOffset, retSize)
    let parent_code = parent_code
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize (4-byte selector)
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(CALLCODE)
        .append(POP); // discard success flag (should be 0 = failure due to NotIntercepted revert)
    let parent_code = append_call(parent_code, CHILD, 50_000_000)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code)
        .account_code(ACCESS_CONTROL_ADDRESS, mega_evm::ACCESS_CONTROL_CODE);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    // Child CALL should have succeeded (returned 1)
    let output = result.result.output().expect("Should have output");
    let child_success = U256::from_be_slice(output.as_ref());
    assert_eq!(
        child_success,
        U256::from(1),
        "Child should succeed because CALLCODE to access control was not intercepted"
    );
}

// ============================================================================
// 17. DISABLE IN REVERTED CHILD DOES NOT AFFECT SIBLING
// ============================================================================

/// When a child frame calls `disableVolatileDataAccess()` and then reverts, the disable
/// should be cleared when the child's frame returns (via `enable_access_if_returning`).
/// A sibling call should NOT be restricted.
///
/// ```text
/// PARENT → CHILD (calls disable, then REVERTs)
///        → SIBLING (reads TIMESTAMP) → should succeed
/// ```
#[test]
fn test_disable_in_reverted_child_does_not_affect_sibling() {
    // CHILD: calls disableVolatileDataAccess(), then immediately reverts
    let child_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_number(0_u64) // size
        .push_number(0_u64) // offset
        .append(REVERT)
        .build();

    // SIBLING: reads TIMESTAMP (should succeed)
    let sibling_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();

    // PARENT: call CHILD (which reverts), log child call status, then call SIBLING,
    // return SIBLING's success flag
    let parent_code = append_call(BytecodeBuilder::default(), CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code);
    let parent_code = append_call(parent_code, SIBLING, 50_000_000)
        // Stack: [sibling_success]. Store and return.
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code)
        .account_code(SIBLING, sibling_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    assert_log_call_status(&result, 0, false);

    let output = result.result.output().expect("Should have output");
    let sibling_success = U256::from_be_slice(output.as_ref());
    assert_eq!(
        sibling_success,
        U256::from(1),
        "SIBLING should succeed: CHILD's disable was cleared when CHILD's frame reverted"
    );
}

// ============================================================================
// 18. ORACLE SLOAD WITH DISABLED VOLATILE ACCESS
// ============================================================================

/// Oracle SLOAD should revert with VolatileDataAccessDisabled(Oracle) when volatile
/// data access is disabled.
#[test]
fn test_oracle_sload_reverts_when_volatile_access_disabled() {
    // Oracle contract code: SLOAD(0)
    let oracle_code = BytecodeBuilder::default()
        .push_number(0_u64) // key = 0
        .append(SLOAD)
        .append(POP)
        .stop()
        .build();

    // Parent: disable volatile access, call oracle, capture return data
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code =
        append_call_and_return_data(parent_code, ORACLE_CONTRACT_ADDRESS, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let result = transact_with_oracle(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    // The oracle SLOAD should have reverted with VolatileDataAccessDisabled(Oracle)
    let output = result.result.output().expect("Should have output");
    assert_eq!(
        &output[..4],
        &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
        "Oracle SLOAD should revert with VolatileDataAccessDisabled error"
    );
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(decoded.accessType, VolatileDataAccessType::Oracle, "Access type should be Oracle");
}

// ============================================================================
// 19. CREATE WITH DISABLED VOLATILE ACCESS
// ============================================================================

/// CREATE whose init code accesses TIMESTAMP should fail when volatile access is disabled.
/// The CREATE returns address 0 (failure), but the parent transaction itself succeeds.
#[test]
fn test_create_reverts_when_volatile_access_disabled() {
    // Init code that accesses TIMESTAMP, then returns empty deployed code.
    // TIMESTAMP -> POP -> PUSH1(0) -> PUSH1(0) -> RETURN (deploy zero-length code)
    let init_code = BytecodeBuilder::default()
        .append(TIMESTAMP)
        .append(POP)
        .push_number(0_u64) // size
        .push_number(0_u64) // offset
        .append(RETURN)
        .build();

    // Parent: disable volatile access, then CREATE with the init code, return created address.
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    // Store init code in memory at offset 0x40 (after selector storage area)
    let parent_code = parent_code
        .mstore(0x40, init_code.clone())
        .push_number(init_code.len() as u64) // size
        .push_number(0x40_u64) // offset
        .push_number(0_u64) // value
        .append(CREATE)
        // Stack: [created_address]. Store and return.
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    // CREATE should have failed (returned address 0) because init code's TIMESTAMP reverted
    let output = result.result.output().expect("Should have output");
    let created_address = U256::from_be_slice(output.as_ref());
    assert_eq!(
        created_address,
        U256::ZERO,
        "CREATE should fail (return 0) because init code accessed volatile data"
    );
}

// ============================================================================
// 20. CALL-LIKE OPCODES TARGETING BENEFICIARY
// ============================================================================

/// CALL to the beneficiary address should revert when volatile access is disabled.
/// The child contract CALLs the beneficiary; the parent captures the child's revert data.
#[test]
fn test_call_beneficiary_restricted() {
    let beneficiary = Address::ZERO;

    // Child: CALL the beneficiary address
    let child_code = append_call(BytecodeBuilder::default(), beneficiary, 100_000).stop().build();

    // Parent: disable volatile access, call child, capture revert data
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Beneficiary,
        "CALL to beneficiary should revert with Beneficiary access type"
    );
}

/// STATICCALL to the beneficiary address should revert when volatile access is disabled.
#[test]
fn test_staticcall_beneficiary_restricted() {
    let beneficiary = Address::ZERO;

    // Child: STATICCALL the beneficiary address
    let child_code =
        append_staticcall(BytecodeBuilder::default(), beneficiary, 100_000).stop().build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Beneficiary,
        "STATICCALL to beneficiary should revert with Beneficiary access type"
    );
}

/// DELEGATECALL to the beneficiary address should revert when volatile access is disabled.
#[test]
fn test_delegatecall_beneficiary_restricted() {
    let beneficiary = Address::ZERO;

    // Child: DELEGATECALL the beneficiary address
    let child_code =
        append_delegatecall(BytecodeBuilder::default(), beneficiary, 100_000).stop().build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Beneficiary,
        "DELEGATECALL to beneficiary should revert with Beneficiary access type"
    );
}

/// CALLCODE to the beneficiary address should revert when volatile access is disabled.
#[test]
fn test_callcode_beneficiary_restricted() {
    let beneficiary = Address::ZERO;

    // Child: CALLCODE the beneficiary address
    let child_code =
        append_callcode(BytecodeBuilder::default(), beneficiary, 100_000).stop().build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");

    let output = result.result.output().expect("Should have output");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Beneficiary,
        "CALLCODE to beneficiary should revert with Beneficiary access type"
    );
}

/// CALL to a non-beneficiary address should NOT revert when volatile access is disabled.
#[test]
fn test_call_non_beneficiary_not_restricted() {
    // Child: CALL a non-beneficiary address (GRANDCHILD)
    let grandchild_code = BytecodeBuilder::default().stop().build();
    let child_code = append_call(BytecodeBuilder::default(), GRANDCHILD, 100_000).stop().build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code)
        .account_code(GRANDCHILD, grandchild_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "CALL to non-beneficiary should not be restricted");
    assert_log_call_status(&result, 0, true);
}

/// CALL to beneficiary should work normally (with gas detention) when volatile access is NOT
/// disabled.
#[test]
fn test_call_beneficiary_not_restricted_without_disable() {
    let beneficiary = Address::ZERO;

    // Child: CALL beneficiary without disabling volatile access.
    let child_code = append_call(BytecodeBuilder::default(), beneficiary, 100_000).stop().build();

    let parent_code = append_call(BytecodeBuilder::default(), CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(
        result.result.is_success(),
        "CALL to beneficiary should succeed when volatile access is not disabled"
    );
    assert_log_call_status(&result, 0, true);
}

/// Blocked CALL to beneficiary should NOT pollute the volatile data tracker.
#[test]
fn test_blocked_call_beneficiary_does_not_pollute_tracker() {
    let beneficiary = Address::ZERO;

    // Child: CALL beneficiary (will be blocked by volatile access disable)
    let child_code = append_call(BytecodeBuilder::default(), beneficiary, 100_000).stop().build();

    // Parent: disable volatile access, call child, log call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let volatile_data_tracker = context.volatile_data_tracker.clone();

    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(default_tx(PARENT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");
    assert_log_call_status(&result, 0, false);

    // The blocked CALL to beneficiary should NOT have set the volatile_data_accessed bitmap.
    let tracker = volatile_data_tracker.borrow();
    assert!(
        !tracker.accessed(),
        "volatile_data_accessed should be empty after blocked CALL to beneficiary"
    );
    assert!(
        tracker.get_compute_gas_limit().is_none(),
        "compute_gas_limit should not be set after blocked CALL to beneficiary"
    );
}

// ============================================================================
// 21. SELFDESTRUCT TARGETING BENEFICIARY
// ============================================================================

/// SELFDESTRUCT targeting the beneficiary should revert when volatile access is disabled.
#[test]
fn test_selfdestruct_beneficiary_restricted() {
    let beneficiary = Address::ZERO;

    // Child: SELFDESTRUCT targeting the beneficiary address
    let child_code =
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build();

    // Parent: disable volatile access, call child, capture revert data
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_data(parent_code, CHILD, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    let output = result.result.output().expect("should have output");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Beneficiary,
        "SELFDESTRUCT to beneficiary should revert with Beneficiary access type"
    );
}

/// SELFDESTRUCT targeting a non-beneficiary address should NOT revert when volatile access
/// is disabled.
#[test]
fn test_selfdestruct_non_beneficiary_not_restricted() {
    // Child: SELFDESTRUCT targeting a non-beneficiary address (GRANDCHILD)
    let child_code =
        BytecodeBuilder::default().push_address(GRANDCHILD).append(SELFDESTRUCT).build();

    // Parent: disable volatile access, call child, log call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(result.result.is_success(), "SELFDESTRUCT to non-beneficiary should not be restricted");
    assert_log_call_status(&result, 0, true);
}

/// SELFDESTRUCT targeting the beneficiary should work (with gas detention) when volatile
/// access is NOT disabled.
#[test]
fn test_selfdestruct_beneficiary_not_restricted_without_disable() {
    let beneficiary = Address::ZERO;

    // Child: SELFDESTRUCT targeting the beneficiary
    let child_code =
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build();

    // Parent: call child without disabling volatile access, log call status
    let parent_code = append_call(BytecodeBuilder::default(), CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let result = transact(&mut db, default_tx(PARENT)).unwrap();
    assert!(
        result.result.is_success(),
        "SELFDESTRUCT to beneficiary should succeed when volatile access is not disabled"
    );
    assert_log_call_status(&result, 0, true);
}

/// Blocked SELFDESTRUCT to beneficiary should NOT pollute the volatile data tracker.
#[test]
fn test_blocked_selfdestruct_beneficiary_does_not_pollute_tracker() {
    let beneficiary = Address::ZERO;

    // Child: SELFDESTRUCT targeting the beneficiary (will be blocked)
    let child_code =
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build();

    // Parent: disable volatile access, call child, log call status
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let volatile_data_tracker = context.volatile_data_tracker.clone();

    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(default_tx(PARENT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success(), "Parent tx should succeed");
    assert_log_call_status(&result, 0, false);

    // The blocked SELFDESTRUCT to beneficiary should NOT have set the bitmap.
    let tracker = volatile_data_tracker.borrow();
    assert!(
        !tracker.accessed(),
        "volatile_data_accessed should be empty after blocked SELFDESTRUCT to beneficiary"
    );
    assert!(
        tracker.get_compute_gas_limit().is_none(),
        "compute_gas_limit should not be set after blocked SELFDESTRUCT to beneficiary"
    );
}

/// On Rex3 (pre-Rex4), SELFDESTRUCT targeting the beneficiary should NOT be checked or
/// restricted for volatile data access. The volatile data check is Rex4-only.
#[test]
fn test_selfdestruct_beneficiary_not_restricted_pre_rex4() {
    let beneficiary = Address::ZERO;

    // Child: SELFDESTRUCT targeting the beneficiary
    let child_code =
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build();

    // Parent: call child, log call status
    let parent_code = append_call(BytecodeBuilder::default(), CHILD, 50_000_000);
    let parent_code = append_log_call_status(parent_code).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(CHILD, child_code);

    // Use Rex3 spec (pre-Rex4)
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX3);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let volatile_data_tracker = context.volatile_data_tracker.clone();

    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(default_tx(PARENT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    // SELFDESTRUCT targeting beneficiary should succeed on Rex3
    assert!(
        result.result.is_success(),
        "On Rex3, SELFDESTRUCT to beneficiary should not be restricted"
    );
    assert_log_call_status(&result, 0, true);

    // Volatile data tracker should NOT have marked beneficiary balance access
    let tracker = volatile_data_tracker.borrow();
    assert!(
        !tracker.accessed(),
        "On Rex3, SELFDESTRUCT to beneficiary should not mark volatile data access"
    );
}
