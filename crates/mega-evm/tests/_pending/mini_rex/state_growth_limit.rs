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

// ============================================================================
// NET GROWTH MODEL TESTS
// ============================================================================

// ============================================================================
// FRAME-BASED TRACKING TESTS
// ============================================================================

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

// ============================================================================
// COMPLEX SCENARIOS TESTS
// ============================================================================

// ============================================================================
// TRACKER MIGRATION COVERAGE TESTS
// ============================================================================
