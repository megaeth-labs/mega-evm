//! Tests for the state growth limit feature of the `MegaETH` EVM.
//!
//! These tests verify that the state growth limit functionality correctly tracks and limits
//! the creation of new accounts and storage slots during transaction execution.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
    handler::EvmTr,
    Database, DatabaseCommit,
};

// Test addresses
const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");
const NEW_ACCOUNT: Address = address!("0000000000000000000000000000000000100003");

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Executes a transaction with specified state growth limit.
///
/// Returns the execution result and the actual state growth used.
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    state_growth_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64), EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(state_growth_limit),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx)?;

    let ctx = evm.ctx_ref();
    let state_growth = ctx.additional_limit.borrow().get_usage().state_growth;
    Ok((r, state_growth))
}

/// Checks if the execution result indicates that the state growth limit was exceeded.
fn is_state_growth_limit_exceeded(result: &ResultAndState<MegaHaltReason>) -> bool {
    matches!(
        &result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::StateGrowthLimitExceeded { .. }, .. }
    )
}

/// Creates a default transaction builder calling a contract.
fn default_tx_builder(to: Address) -> TxEnvBuilder {
    TxEnvBuilder::default().caller(CALLER).call(to).gas_limit(100_000_000)
}

// ============================================================================
// BASIC STATE GROWTH TRACKING TESTS
// ============================================================================

#[test]
fn test_empty_transaction() {
    let code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "Empty transaction should have zero state growth");
}

#[test]
fn test_create_new_account_via_value_transfer() {
    let code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    // Transfer value to a new account
    let tx = default_tx_builder(NEW_ACCOUNT).value(U256::from(100)).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 1, "Creating new account via value transfer should count as +1");
}

#[test]
fn test_create_new_storage_slot() {
    // Contract that writes to a single storage slot
    let code = BytecodeBuilder::default().sstore(U256::from(0), U256::from(42)).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 1, "Writing to empty storage slot should count as +1");
}

#[test]
fn test_clear_storage_slot() {
    // Contract that writes to a slot then clears it
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(42)) // Write: 0 -> 42 (+1)
        .sstore(U256::from(0), U256::from(0)) // Clear: 42 -> 0 (-1)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "Creating then clearing a slot should result in zero net growth");
}

#[test]
fn test_contract_creation() {
    // Simple contract to deploy
    let deployed_code = BytecodeBuilder::default().stop().build();

    // Deployment code that returns the contract code
    let init_code = BytecodeBuilder::default().return_with_data(deployed_code).build();

    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    // CREATE transaction
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .create()
        .data(init_code)
        .gas_limit(10_000_000)
        .build_fill();

    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 1, "Contract creation should count as +1");
}

#[test]
fn test_multiple_writes_same_slot() {
    // Contract that writes to the same slot multiple times
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // First write: 0 -> 1 (+1)
        .sstore(U256::from(0), U256::from(2)) // Overwrite: 1 -> 2 (0)
        .sstore(U256::from(0), U256::from(3)) // Overwrite: 2 -> 3 (0)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 1, "Multiple writes to same slot should only count first write");
}

// ============================================================================
// NET GROWTH MODEL TESTS
// ============================================================================

#[test]
fn test_net_growth_calculation() {
    // Create 3 slots, clear 1 slot -> net growth = 2
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .sstore(U256::from(2), U256::from(3)) // +1
        .sstore(U256::from(0), U256::from(0)) // -1
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 2, "Net growth should be 3 - 1 = 2");
}

#[test]
fn test_growth_eventually_zero() {
    // Create and clear multiple slots, ending at zero
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .sstore(U256::from(0), U256::from(0)) // -1
        .sstore(U256::from(1), U256::from(0)) // -1
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "All creates and clears should cancel out to zero");
}

#[test]
fn test_multiple_operations_net_growth() {
    // Complex sequence: create 5, clear 2 -> net 3
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(1)) // +1
        .sstore(U256::from(2), U256::from(1)) // +1
        .sstore(U256::from(3), U256::from(1)) // +1
        .sstore(U256::from(4), U256::from(1)) // +1
        .sstore(U256::from(0), U256::from(0)) // -1
        .sstore(U256::from(1), U256::from(0)) // -1
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 3, "Net growth should be 5 - 2 = 3");
}

#[test]
fn test_storage_slot_already_exists() {
    // Write to a slot that already has a value (should not count)
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(100)) // Modify existing slot (0)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    // Pre-populate the storage slot
    db.set_account_storage(CALLEE, U256::from(0), U256::from(42));

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "Modifying existing storage slot should not count as growth");
}

// ============================================================================
// FRAME-BASED TRACKING TESTS
// ============================================================================

#[test]
fn test_simple_call_executes() {
    // First, verify that a simple CALL actually executes the child code
    // Child returns a specific value to prove it executed
    let child_code = BytecodeBuilder::default().return_with_data(vec![0x42_u8]).build();

    // Parent calls child and checks return value
    let parent_code = BytecodeBuilder::default()
        .push_number(32_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CONTRACT) // child address
        .push_number(10_000_000_u64) // gas
        .append(CALL)
        // CALL result is on stack (1 = success, 0 = failure)
        .assert_stack_value(0, U256::from(1)) // Verify CALL succeeded
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed if child executes");
}

#[test]
fn test_nested_call_successful_child() {
    // Child contract that creates a storage slot
    let child_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1 in child
        .stop()
        .build();

    // Parent contract that creates a storage slot and calls child
    let parent_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1 in parent
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CONTRACT) // child address
        .push_number(10_000_000_u64) // gas - increased
        .append(CALL)
        // Check if CALL succeeded
        .assert_stack_value(0, U256::from(1)) // CALL should return 1 on success
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 2, "Parent and child growth should both count");
}

#[test]
fn test_nested_call_reverting_child() {
    // Child contract that creates a storage slot then reverts
    let child_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1 in child (will be discarded)
        .revert()
        .build();

    // Parent contract that creates a storage slot and calls child
    let parent_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1 in parent
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CONTRACT) // child address
        .push_number(10_000_000_u64) // gas
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 1, "Child growth should be discarded on revert");
}

#[test]
fn test_deeply_nested_calls() {
    // Level 2 contract: creates 1 slot
    let level2_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .stop()
        .build();

    // Level 1 contract: creates 1 slot, calls level 2
    let level1_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CONTRACT) // level 2 address
        .push_number(10_000_000_u64) // gas
        .append(CALL)
        .stop()
        .build();

    // Level 0 (parent): creates 1 slot, calls level 1
    let parent_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CALLEE) // level 1 address
        .push_number(10_000_000_u64) // gas
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(address!("0000000000000000000000000000000000100010"), parent_code)
        .account_code(CALLEE, level1_code)
        .account_code(CONTRACT, level2_code);

    let tx = default_tx_builder(address!("0000000000000000000000000000000000100010")).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 3, "All levels should contribute to growth");
}

#[test]
fn test_parent_creates_child_reverts() {
    // Child that reverts after creating state
    let child_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1 (will be discarded)
        .sstore(U256::from(1), U256::from(2)) // +1 (will be discarded)
        .revert()
        .build();

    // Parent creates 3 slots, calls child
    let parent_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .sstore(U256::from(2), U256::from(3)) // +1
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CONTRACT) // child address
        .push_number(10_000_000_u64) // gas
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 3, "Only parent growth should count");
}

#[test]
fn test_both_parent_and_child_create_state() {
    // Child creates 2 slots
    let child_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .stop()
        .build();

    // Parent creates 3 slots, calls child
    let parent_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .sstore(U256::from(2), U256::from(3)) // +1
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CONTRACT) // child address
        .push_number(10_000_000_u64) // gas
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 5, "Parent (3) + child (2) = 5 total growth");
}

// ============================================================================
// LIMIT ENFORCEMENT TESTS
// ============================================================================

#[test]
fn test_limit_exactly_at_limit() {
    // Create exactly 3 slots with limit of 3
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .sstore(U256::from(2), U256::from(3)) // +1
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 3, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 3, "Exactly at limit should succeed");
}

#[test]
fn test_limit_exceeded_by_one() {
    // Try to create 4 slots with limit of 3
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .sstore(U256::from(2), U256::from(3)) // +1
        .sstore(U256::from(3), U256::from(4)) // +1 (exceeds limit)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 3, tx).unwrap();

    assert!(result.result.is_halt());
    assert!(is_state_growth_limit_exceeded(&result));

    // Verify the halt reason details - the actual growth is preserved in the halt reason
    match &result.result {
        ExecutionResult::Halt {
            reason: MegaHaltReason::StateGrowthLimitExceeded { limit, actual },
            ..
        } => {
            assert_eq!(*limit, 3);
            assert_eq!(*actual, 4, "Should report actual growth that exceeded limit");
        }
        _ => panic!("Expected StateGrowthLimitExceeded halt"),
    }
}

#[test]
fn test_limit_exceeded_in_nested_call() {
    // Child creates 2 slots
    let child_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .stop()
        .build();

    // Parent creates 2 slots, then calls child (total would be 4, limit is 3)
    let parent_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CONTRACT) // child address
        .push_number(10_000_000_u64) // gas
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 3, tx).unwrap();

    assert!(result.result.is_halt());
    assert!(is_state_growth_limit_exceeded(&result));
}

#[test]
fn test_state_reverted_when_exceeding_limit() {
    // Create 2 slots, then exceed limit on 3rd (with limit of 2)
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(100)) // +1
        .sstore(U256::from(1), U256::from(200)) // +1
        .sstore(U256::from(2), U256::from(300)) // +1 (exceeds)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 2, tx).unwrap();

    assert!(result.result.is_halt());
    assert!(is_state_growth_limit_exceeded(&result));

    // State should not be committed
    db.commit(result.state);

    // Verify the storage was not persisted
    let storage_0 = db.storage(CALLEE, U256::from(0)).unwrap();
    let storage_1 = db.storage(CALLEE, U256::from(1)).unwrap();
    let storage_2 = db.storage(CALLEE, U256::from(2)).unwrap();

    assert_eq!(storage_0, U256::ZERO, "Storage should be reverted");
    assert_eq!(storage_1, U256::ZERO, "Storage should be reverted");
    assert_eq!(storage_2, U256::ZERO, "Storage should be reverted");
}

#[test]
fn test_gas_preserved_on_limit_exceeded() {
    // Create slots until limit exceeded
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .sstore(U256::from(1), U256::from(2)) // +1
        .sstore(U256::from(2), U256::from(3)) // +1 (exceeds)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let gas_limit = 10_000_000_u64;
    let tx = TxEnvBuilder::default().caller(CALLER).call(CALLEE).gas_limit(gas_limit).build_fill();

    let (result, _state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 2, tx).unwrap();

    assert!(result.result.is_halt());
    assert!(is_state_growth_limit_exceeded(&result));

    // Gas should not all be consumed
    let gas_used = result.result.gas_used();
    assert!(gas_used < gas_limit, "Gas should be preserved on limit exceeded, not all consumed");
}

// ============================================================================
// COMPLEX SCENARIOS TESTS
// ============================================================================

#[test]
fn test_mixed_operations() {
    // Contract that creates account via CREATE and writes storage
    let deployed_code = BytecodeBuilder::default().stop().build();

    let init_code = BytecodeBuilder::default().return_with_data(deployed_code).build();

    // Main contract: writes storage then creates contract
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1 storage
        .sstore(U256::from(1), U256::from(2)) // +1 storage
        .push_number(0_u64) // value
        .mstore(0x00, init_code.clone()) // Store init code in memory
        .push_number(init_code.len() as u64) // size
        .push_number(0x00_u64) // offset
        .append(CREATE) // +1 account
        .append(POP) // Pop created address
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 3, "2 storage slots + 1 account = 3 growth");
}

#[test]
fn test_multiple_transactions_reset_counters() {
    let code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1)) // +1
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    // First transaction
    let tx1 = default_tx_builder(CALLEE).nonce(0).build_fill();
    let (result1, state_growth1) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx1).unwrap();
    assert!(result1.result.is_success());
    assert_eq!(state_growth1, 1);

    // Commit first transaction
    db.commit(result1.state);

    // Second transaction - counter should reset, but slot is now non-zero
    let tx2 = default_tx_builder(CALLEE).nonce(1).build_fill();
    let (result2, state_growth2) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx2).unwrap();
    assert!(result2.result.is_success());
    assert_eq!(state_growth2, 0, "Second tx should have 0 growth as slot already exists");
}

#[test]
fn test_state_growth_tracked_in_mini_rex() {
    let code = BytecodeBuilder::default().sstore(U256::from(0), U256::from(1)).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::MINI_REX, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 1, "MINI_REX spec should track state growth");
}

#[test]
fn test_state_growth_not_tracked_in_equivalence() {
    let code = BytecodeBuilder::default().sstore(U256::from(0), U256::from(1)).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::EQUIVALENCE, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "EQUIVALENCE spec should not track state growth");
}
