//! Tests for the compute gas limit feature of the `MegaETH` EVM.
//!
//! Tests the compute gas limit functionality that tracks computational work
//! separately from storage and data costs.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
};

// ============================================================================
// CONSTANTS
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT: Address = address!("0000000000000000000000000000000000100001");
const CONTRACT2: Address = address!("0000000000000000000000000000000000100002");

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Executes a transaction with specified compute gas limit.
fn transact(
    spec: MegaSpecId,
    db: &mut CacheDB<EmptyDB>,
    compute_gas_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64), EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec, DefaultExternalEnvs::default());
    // Set compute gas limit
    context.additional_limit.borrow_mut().compute_gas_limit = compute_gas_limit;
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx)?;

    let ctx = evm.ctx_ref();
    let compute_gas_used = ctx.additional_limit.borrow().get_usage().compute_gas;

    Ok((r, compute_gas_used))
}

/// Helper to check if the result is a compute gas limit exceeded halt.
fn is_compute_gas_limit_exceeded(result: &ResultAndState<MegaHaltReason>) -> bool {
    matches!(
        &result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::ComputeGasLimitExceeded { .. }, .. }
    )
}

/// Helper to extract compute gas limit info from halt reason.
fn get_compute_gas_limit_info(result: &ResultAndState<MegaHaltReason>) -> Option<(u64, u64)> {
    match &result.result {
        ExecutionResult::Halt {
            reason: MegaHaltReason::ComputeGasLimitExceeded { limit, actual },
            ..
        } => Some((*limit, *actual)),
        _ => None,
    }
}

// ============================================================================
// BASIC TRACKING TESTS
// ============================================================================

#[test]
fn test_empty_contract_compute_gas() {
    let bytecode = BytecodeBuilder::default().append(PUSH0).append(STOP).build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();

    let (result, compute_gas_used) =
        transact(MegaSpecId::MINI_REX, &mut db, 1_000_000_000, tx).unwrap();

    assert!(result.result.is_success());
    // Should have some gas from transaction intrinsic cost and opcodes
    assert!(compute_gas_used > 0);
    assert!(compute_gas_used < 50_000); // Should be small for simple operations
    assert_eq!(compute_gas_used, result.result.gas_used());
}

#[test]
fn test_simple_arithmetic_compute_gas() {
    let bytecode = BytecodeBuilder::default()
        .push_number(1u8)
        .push_number(2u8)
        .append(ADD)
        .append(POP)
        .push_number(3u8)
        .push_number(4u8)
        .append(MUL)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();

    let (result, compute_gas_used) =
        transact(MegaSpecId::MINI_REX, &mut db, 1_000_000_000, tx).unwrap();

    assert!(result.result.is_success());
    // Should track gas for all arithmetic operations
    assert!(compute_gas_used > 0);
    assert_eq!(compute_gas_used, result.result.gas_used());
}

// ============================================================================
// LIMIT ENFORCEMENT TESTS
// ============================================================================

#[test]
fn test_compute_gas_limit_not_exceeded() {
    // Need enough operations so execution gas > 21,000 for meaningful test
    let mut bytecode = BytecodeBuilder::default();
    for _ in 0..2000 {
        bytecode = bytecode.push_number(1u8).push_number(2u8).append(ADD).append(POP);
    }
    let bytecode = bytecode.append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode.clone());

    let tx = TxEnvBuilder::new()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(1_000_000) // High tx gas limit for validation
        .build_fill();

    // First, measure the actual gas used
    let (_r, actual_gas) = transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx.clone()).unwrap();

    // Reset db to ensure consistent state
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    // Now set limit to exactly the actual gas used
    let (result, compute_gas_used) =
        transact(MegaSpecId::MINI_REX, &mut db, actual_gas, tx).unwrap();

    // Should succeed since we're exactly at the limit (uses > not >=)
    assert!(
        result.result.is_success(),
        "Transaction should succeed with gas_used={} and limit={}",
        compute_gas_used,
        actual_gas
    );
    assert_eq!(compute_gas_used, actual_gas);
}

#[test]
fn test_compute_gas_limit_exceeded() {
    let mut bytecode = BytecodeBuilder::default();
    // Add many operations to ensure execution gas > 21,000 (validation requirement)
    // Need ~2000 iterations × 11 gas = 22,000 gas for execution
    for _ in 0..2000 {
        bytecode = bytecode.push_number(1u8).push_number(2u8).append(ADD).append(POP);
    }
    let bytecode = bytecode.append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode.clone());

    let tx = TxEnvBuilder::new()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(1_000_000) // High tx gas limit for validation
        .build_fill();

    // First measure actual usage
    let (_, actual_usage) = transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx.clone()).unwrap();

    // Compute gas tracks only opcode execution gas (intrinsic gas is reset after validation)
    // 2000 iterations of (PUSH1 + PUSH1 + ADD + POP) = 2000 × 11 = 22,000 gas
    assert!(actual_usage >= 22_000, "Expected at least 22,000 gas, got {}", actual_usage);

    // Reset db to ensure consistent state
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    // Set compute gas limit below execution needs (will pass 21,000 validation)
    let limit = actual_usage - 1000;
    let (result, compute_gas_used) = transact(MegaSpecId::MINI_REX, &mut db, limit, tx).unwrap();

    // Should halt with compute gas limit exceeded
    assert!(
        is_compute_gas_limit_exceeded(&result),
        "Expected compute gas limit exceeded, actual gas: {}, limit: {}, measured: {}",
        compute_gas_used,
        limit,
        actual_usage
    );
    assert!(compute_gas_used > limit);

    // Verify the halt reason contains correct info
    let (halt_limit, halt_actual) = get_compute_gas_limit_info(&result).unwrap();
    assert_eq!(halt_limit, limit);
    assert!(halt_actual > halt_limit);
}

#[test]
fn test_compute_gas_refund_on_limit_exceeded() {
    let mut bytecode = BytecodeBuilder::default();
    // Add many operations (need execution gas > 21,000 for validation)
    for _ in 0..2000 {
        bytecode = bytecode.push_number(1u8).push_number(2u8).append(ADD).append(POP);
    }
    let bytecode = bytecode.append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode.clone());

    let tx = TxEnvBuilder::new()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(10_000_000) // High tx gas limit for validation
        .build_fill();

    // First measure actual usage
    let (_, actual_usage) = transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx.clone()).unwrap();

    // Compute gas tracks only opcode execution gas (intrinsic gas is reset after validation)
    // 2000 iterations of (PUSH1 + PUSH1 + ADD + POP) = 2000 × 11 = 22,000 gas
    assert!(actual_usage >= 22_000, "Expected at least 22,000 gas, got {}", actual_usage);

    // Reset db to ensure consistent state
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    // Call with low compute gas limit just below actual usage
    let limit = actual_usage - 1000;
    let (result, compute_gas_used) = transact(MegaSpecId::MINI_REX, &mut db, limit, tx).unwrap();

    // Should halt with compute gas limit exceeded, but remaining gas is refunded
    assert!(is_compute_gas_limit_exceeded(&result));
    assert_eq!(compute_gas_used, result.result.gas_used());
    assert!(result.result.gas_used() < 43_000);
}

// ============================================================================
// INSTRUCTION COVERAGE TESTS
// ============================================================================

#[test]
fn test_compute_gas_storage_operations() {
    let bytecode = BytecodeBuilder::default()
        .push_number(0xFFu8)
        .append(PUSH0) // key
        .append(SSTORE)
        .append(PUSH0) // key
        .append(SLOAD)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000))
        .account_code(CONTRACT, bytecode);

    let tx =
        TxEnvBuilder::new().caller(CALLER).call(CONTRACT).gas_limit(1_000_000_000).build_fill();

    let (result, compute_gas_used) = transact(MegaSpecId::MINI_REX, &mut db, 100_000, tx).unwrap();

    assert!(result.result.is_success());
    // Storage operations are expensive
    assert!(compute_gas_used > 20_000);
    assert!(compute_gas_used < 100_000);
}

#[test]
fn test_compute_gas_memory_operations() {
    let bytecode = BytecodeBuilder::default()
        .mstore(0x40, vec![0xFFu8])
        .push_number(0x40u8)
        .append(MLOAD)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let tx =
        TxEnvBuilder::new().caller(CALLER).call(CONTRACT).gas_limit(1_000_000_000).build_fill();

    let (result, compute_gas_used) = transact(MegaSpecId::MINI_REX, &mut db, 100_000, tx).unwrap();

    assert!(result.result.is_success());
    // Memory operations including expansion cost
    assert!(compute_gas_used > 0);
    assert!(compute_gas_used < 100_000);
}

#[test]
fn test_compute_gas_log_operations() {
    let bytecode = BytecodeBuilder::default()
        .push_number(0x20u8)
        .append(PUSH0) // offset
        .append(LOG0)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();

    let (result, compute_gas_used) = transact(MegaSpecId::MINI_REX, &mut db, 30_000, tx).unwrap();

    assert!(result.result.is_success());
    // Should track gas (intrinsic + log operations)
    assert!(compute_gas_used > 0);
    assert!(compute_gas_used < 30_000);
}

// ============================================================================
// NESTED CALL TESTS
// ============================================================================

#[test]
fn test_nested_call_compute_gas_accumulation() {
    // Callee does some work
    let mut callee_bytecode = BytecodeBuilder::default();
    for _ in 0..100 {
        callee_bytecode = callee_bytecode.push_number(1u8).push_number(2u8).append(ADD).append(POP);
    }
    let callee_bytecode = callee_bytecode.append(STOP).build();

    // Caller does work and calls callee
    let mut caller_bytecode = BytecodeBuilder::default();
    // Do some work
    for _ in 0..10 {
        caller_bytecode = caller_bytecode.push_number(1u8).push_number(2u8).append(ADD).append(POP);
    }
    // CALL callee: gas, address, value, argsOffset, argsSize, retOffset, retSize
    caller_bytecode = caller_bytecode
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argsSize
        .push_number(0u8) // argsOffset
        .push_number(0u8); // value
    caller_bytecode = caller_bytecode.push_address(CONTRACT2); // address
    caller_bytecode = caller_bytecode.push_number(0xFFFFu16).append(CALL).append(POP).append(STOP);

    let caller_bytecode = caller_bytecode.build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, caller_bytecode)
        .account_code(CONTRACT2, callee_bytecode.clone());

    // Get baseline gas for just calling callee
    let tx_callee = TxEnvBuilder::new().caller(CALLER).call(CONTRACT2).build_fill();
    let (_, callee_gas) = transact(MegaSpecId::MINI_REX, &mut db, 10_000_000, tx_callee).unwrap();

    // Call with nested call
    let tx_caller = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();
    let (result, total_gas) =
        transact(MegaSpecId::MINI_REX, &mut db, 10_000_000, tx_caller).unwrap();

    assert!(result.result.is_success());
    // Total gas should be more than just callee gas
    assert!(total_gas > callee_gas);
}

#[test]
fn test_compute_gas_limit_exceed_in_nested_call() {
    // Callee with many operations (need execution gas > 21,000 for validation)
    let mut callee_bytecode = BytecodeBuilder::default();
    for _ in 0..2000 {
        callee_bytecode = callee_bytecode.push_number(1u8).push_number(2u8).append(ADD).append(POP);
    }
    let callee_bytecode = callee_bytecode.append(STOP).build();

    // Caller that calls callee
    let mut caller_bytecode = BytecodeBuilder::default()
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argsSize
        .push_number(0u8) // argsOffset
        .push_number(0u8); // value
    caller_bytecode = caller_bytecode.push_address(CONTRACT2);
    let caller_bytecode =
        caller_bytecode.push_number(0xFFFFu16).append(CALL).append(POP).append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, caller_bytecode.clone())
        .account_code(CONTRACT2, callee_bytecode.clone());

    let tx = TxEnvBuilder::new()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(10_000_000) // High tx gas limit for validation
        .build_fill();

    // First measure actual usage
    let (_, actual_usage) = transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx.clone()).unwrap();

    // Compute gas includes call overhead + callee operations
    assert!(actual_usage >= 22_000, "Expected at least 22,000 gas, got {}", actual_usage);

    // Reset db to ensure consistent state
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, caller_bytecode)
        .account_code(CONTRACT2, callee_bytecode);

    // Set low compute gas limit - should exceed in nested call
    let limit = actual_usage - 1000;
    let (result, _) = transact(MegaSpecId::MINI_REX, &mut db, limit, tx).unwrap();

    // Should halt with compute gas limit exceeded
    assert!(is_compute_gas_limit_exceeded(&result));
    assert!(result.result.gas_used() < 1_000_000);
}

// ============================================================================
// MULTI-DIMENSIONAL LIMIT TESTS
// ============================================================================

#[test]
fn test_correct_halt_reason_compute_gas() {
    let mut bytecode = BytecodeBuilder::default();
    // Need execution gas > 21,000 for validation
    for _ in 0..2000 {
        bytecode = bytecode.push_number(1u8).push_number(2u8).append(ADD).append(POP);
    }
    let bytecode = bytecode.append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode.clone());

    let tx = TxEnvBuilder::new()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(10_000_000) // High tx gas limit for validation
        .build_fill();

    // First measure actual usage
    let (_, actual_usage) = transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx.clone()).unwrap();

    // Compute gas tracks only opcode execution gas (intrinsic gas is reset after validation)
    // 2000 iterations of (PUSH1 + PUSH1 + ADD + POP) = 2000 × 11 = 22,000 gas
    assert!(actual_usage >= 22_000, "Expected at least 22,000 gas, got {}", actual_usage);

    // Reset db to ensure consistent state
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    // Set limit just below actual
    let set_limit = actual_usage - 1000;
    let (result, _) = transact(MegaSpecId::MINI_REX, &mut db, set_limit, tx).unwrap();

    // Verify correct halt reason
    assert!(is_compute_gas_limit_exceeded(&result));

    let (limit, actual) = get_compute_gas_limit_info(&result).unwrap();
    assert_eq!(limit, set_limit);
    assert!(actual > limit);
}

// ============================================================================
// TRANSACTION RESET TESTS
// ============================================================================

#[test]
fn test_compute_gas_resets_across_transactions() {
    let bytecode = BytecodeBuilder::default()
        .push_number(1u8)
        .push_number(2u8)
        .append(ADD)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    // First transaction
    let tx1 = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();
    let (result1, gas1) = transact(MegaSpecId::MINI_REX, &mut db, 10_000_000, tx1).unwrap();

    assert!(result1.result.is_success());

    // Second transaction - gas should reset, not accumulate
    let tx2 = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();
    let (result2, gas2) = transact(MegaSpecId::MINI_REX, &mut db, 10_000_000, tx2).unwrap();

    assert!(result2.result.is_success());

    // Gas should be similar for both transactions (reset between)
    // Allow some variance due to warm/cold storage access
    let variance = gas1.max(gas2) - gas1.min(gas2);
    assert!(variance < gas1 / 10, "Gas variance too large: {} vs {}", gas1, gas2);
}

// ============================================================================
// SPEC COMPARISON TESTS
// ============================================================================

#[test]
fn test_compute_gas_tracked_in_mini_rex() {
    let bytecode = BytecodeBuilder::default()
        .push_number(1u8)
        .push_number(2u8)
        .append(ADD)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();

    let (result, compute_gas_used) =
        transact(MegaSpecId::MINI_REX, &mut db, 10_000_000, tx).unwrap();

    assert!(result.result.is_success());
    // In MINI_REX, compute gas should be tracked
    assert!(compute_gas_used > 0);
}

#[test]
fn test_compute_gas_not_tracked_in_equivalence() {
    let bytecode = BytecodeBuilder::default()
        .append(PUSH1)
        .append(1)
        .append(PUSH1)
        .append(2)
        .append(ADD)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();

    let (result, compute_gas_used) =
        transact(MegaSpecId::EQUIVALENCE, &mut db, 10_000_000, tx).unwrap();

    assert!(result.result.is_success());
    // In EQUIVALENCE, compute gas should NOT be tracked
    assert_eq!(compute_gas_used, 0);
}

// ============================================================================
// EDGE CASE TESTS
// ============================================================================

#[test]
fn test_compute_gas_limit_zero() {
    let bytecode = BytecodeBuilder::default().append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    // With zero compute gas limit, should fail at validation (can't cover intrinsic 21000)
    let result = transact(MegaSpecId::MINI_REX, &mut db, 0, tx);
    assert!(result.is_err());
}

#[test]
fn test_compute_gas_limit_one() {
    let bytecode = BytecodeBuilder::default().append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill();

    // With limit of 1, should fail at validation (can't cover intrinsic 21000)
    let result = transact(MegaSpecId::MINI_REX, &mut db, 1, tx);
    assert!(result.is_err());
}

#[test]
fn test_compute_gas_high_usage() {
    let mut bytecode = BytecodeBuilder::default();
    // Add many operations
    for _ in 0..1000 {
        bytecode = bytecode.append(PUSH1).append(1).append(PUSH1).append(2).append(ADD).append(POP);
    }
    let bytecode = bytecode.append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(100_000_000))
        .account_code(CONTRACT, bytecode);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();

    let (result, compute_gas_used) =
        transact(MegaSpecId::MINI_REX, &mut db, 1_000_000_000, tx).unwrap();

    assert!(result.result.is_success());
    // Should use substantial compute gas (intrinsic gas is reset after validation)
    // 1000 iterations × 11 gas = 11,000 gas
    assert!(compute_gas_used >= 21_000, "Expected at least 10,000 gas, got {}", compute_gas_used);
}
