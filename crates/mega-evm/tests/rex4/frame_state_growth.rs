//! Tests for Rex4 per-frame state growth limits.
//!
//! Rex4 introduces per-frame state growth budgets: each inner call frame receives
//! `remaining * 98 / 100` of the parent's remaining state growth budget.
//! When a frame exceeds its budget, it reverts (not halts) with ABI-encoded
//! `MegaLimitExceeded(uint8 kind, uint64 limit)` revert data.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaLimitExceeded, MegaSpecId,
    MegaTransaction, MegaTransactionError,
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
const CONTRACT2: Address = address!("0000000000000000000000000000000000100003");
const CONTRACT3: Address = address!("0000000000000000000000000000000000100004");

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

/// Checks if the execution result indicates that the state growth limit was exceeded (TX-level).
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

/// Builds bytecode that writes `n` distinct storage slots (slot 0..n-1) to non-zero values.
fn write_n_slots(mut builder: BytecodeBuilder, n: u64) -> BytecodeBuilder {
    for i in 0..n {
        builder = builder.sstore(U256::from(i), U256::from(i + 1));
    }
    builder
}

/// Builds bytecode that writes `n` slots and then clears `m` of them back to zero.
fn write_n_clear_m_slots(builder: BytecodeBuilder, n: u64, m: u64) -> BytecodeBuilder {
    let mut b = write_n_slots(builder, n);
    for i in 0..m {
        b = b.sstore(U256::from(i), U256::from(0));
    }
    b
}

/// Appends a CALL to `target` with the given gas amount onto the builder.
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

/// Appends a CALL that captures the return data: calls `target`, then copies RETURNDATASIZE
/// bytes of return data to memory offset 0, then RETURNs it.
fn append_call_and_return_revert_data(
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
// 1. BASIC PER-FRAME BUDGET ALLOCATION
// ============================================================================

#[test]
fn test_top_level_frame_gets_full_tx_limit() {
    // Top-level frame should get the full TX limit (no 98/100 reduction).
    let code = write_n_slots(BytecodeBuilder::default(), 100).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 100, "Top-level frame should be able to use full TX limit");
}

#[test]
fn test_child_frame_budget_is_98_percent_of_parent() {
    // TX limit = 100 → child budget = 100 * 98/100 = 98
    // Child creates 98 slots → should succeed
    let child_code = write_n_slots(BytecodeBuilder::default(), 98).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 98, "Child with budget 98 should succeed creating 98 slots");
}

#[test]
fn test_child_frame_exceeds_98_percent_budget() {
    // TX limit = 100 → child budget = 98
    // Child creates 99 slots → exceeds per-frame budget → reverted
    // Parent continues → TX succeeds
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed after child's frame-local revert");
    assert_eq!(state_growth, 0, "Child's growth should be discarded on frame-local revert");
}

#[test]
fn test_grandchild_budget_progressive_reduction() {
    // TX limit = 1000
    // Child budget = 1000 * 98/100 = 980
    // Grandchild budget = 980 * 98/100 = 960
    // Grandchild creates 960 slots → succeeds
    let grandchild_code = write_n_slots(BytecodeBuilder::default(), 960).stop().build();

    let child_code =
        append_call(BytecodeBuilder::default(), CONTRACT2, 80_000_000).append(POP).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 90_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code)
        .account_code(CONTRACT2, grandchild_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 960, "Grandchild should succeed within 960 budget");
}

#[test]
fn test_grandchild_exceeds_progressive_budget() {
    // TX limit = 1000, grandchild budget = 960
    // Grandchild creates 961 → exceeds → reverted
    let grandchild_code = write_n_slots(BytecodeBuilder::default(), 961).stop().build();

    let child_code =
        append_call(BytecodeBuilder::default(), CONTRACT2, 80_000_000).append(POP).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 90_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code)
        .account_code(CONTRACT2, grandchild_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success(), "Parent/child should succeed after grandchild revert");
    assert_eq!(state_growth, 0, "Grandchild's 961 slots discarded on revert");
}

#[test]
fn test_child_budget_accounts_for_parent_usage() {
    // TX limit = 1000, parent creates 200 slots (remaining = 800)
    // Child budget = 800 * 98/100 = 784
    // Child creates 784 → succeeds
    let child_code = write_n_slots(BytecodeBuilder::default(), 784).stop().build();

    let parent_code = write_n_slots(BytecodeBuilder::default(), 200);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 200 + 784, "Parent (200) + child (784) = 984");
}

#[test]
fn test_child_exceeds_budget_after_parent_usage() {
    // TX limit = 1000, parent uses 200, child budget = 784
    // Child creates 785 → exceeds → reverted
    let child_code = write_n_slots(BytecodeBuilder::default(), 785).stop().build();

    let parent_code = write_n_slots(BytecodeBuilder::default(), 200);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed after child revert");
    assert_eq!(state_growth, 200, "Only parent's 200 slots should persist");
}

#[test]
fn test_sibling_frames_get_independent_budgets() {
    // TX limit = 1000
    // Child A: creates 100 slots, succeeds → parent remaining = 900
    // Child B: budget = 900 * 98/100 = 882, creates 882 → succeeds
    let child_a_code = write_n_slots(BytecodeBuilder::default(), 100).stop().build();
    let child_b_code = write_n_slots(BytecodeBuilder::default(), 882).stop().build();

    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP);
    let parent_code = append_call(parent_code, CONTRACT2, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_a_code)
        .account_code(CONTRACT2, child_b_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 100 + 882, "Child A (100) + Child B (882) = 982");
}

#[test]
fn test_zero_remaining_budget_child_gets_zero() {
    // TX limit = 100, parent creates 100 slots (remaining = 0)
    // Child budget = 0 * 98/100 = 0 → child creates 1 slot → reverted
    let child_code = write_n_slots(BytecodeBuilder::default(), 1).stop().build();

    let parent_code = write_n_slots(BytecodeBuilder::default(), 100);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed");
    assert_eq!(state_growth, 100, "Only parent's 100 slots should persist");
}

#[test]
fn test_small_remaining_budget_integer_division() {
    // TX limit = 100, parent creates 99 slots (remaining = 1)
    // Child budget = 1 * 98/100 = 0 (integer floor division)
    // Child creates 1 slot → exceeds 0 → reverted
    let child_code = write_n_slots(BytecodeBuilder::default(), 1).stop().build();

    let parent_code = write_n_slots(BytecodeBuilder::default(), 99);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed");
    assert_eq!(state_growth, 99, "Only parent's 99 slots");
}

// ============================================================================
// 2. FRAME-LOCAL ABSORPTION SEMANTICS
// ============================================================================

#[test]
fn test_frame_local_exceed_reverts_not_halts() {
    // TX limit = 100, child budget = 98, child creates 99 → reverted (absorbed)
    // Overall TX result is success, NOT halt.
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success(), "Should be success, not halt");
    assert!(!result.result.is_halt(), "Should NOT be halt");
}

#[test]
fn test_frame_local_exceed_gas_returned_to_parent() {
    // Child exceeds, parent continues executing (proving gas was returned).
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();

    // Parent: CALL child (reverts), then write own slot to prove it continues.
    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000)
        .append(POP)
        .sstore(U256::from(0), U256::from(42)) // prove parent continues
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let gas_limit = 100_000_000_u64;
    let tx = TxEnvBuilder::default().caller(CALLER).call(CALLEE).gas_limit(gas_limit).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 1, "Parent wrote 1 slot after child revert");
    let gas_used = result.result.gas_used();
    assert!(gas_used < gas_limit, "Gas should not be fully consumed");
}

#[test]
fn test_frame_local_exceed_child_state_discarded() {
    // Child exceeds and reverts → child's storage writes not persisted.
    // Parent's writes are persisted.
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();

    let parent_code = BytecodeBuilder::default().sstore(U256::from(0), U256::from(42)); // parent write
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 1, "Only parent's slot counts");

    // Commit and verify storage
    db.commit(result.state);

    let parent_val = db.storage(CALLEE, U256::from(0)).unwrap();
    assert_eq!(parent_val, U256::from(42), "Parent storage should persist");

    let child_val = db.storage(CONTRACT, U256::from(0)).unwrap();
    assert_eq!(child_val, U256::ZERO, "Child storage should be reverted");
}

#[test]
fn test_parent_continues_after_child_exceed() {
    // Parent creates 2 slots, calls child (exceeds), parent creates 2 more.
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();

    let parent_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1))
        .sstore(U256::from(1), U256::from(2));
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000)
        .append(POP)
        .sstore(U256::from(2), U256::from(3))
        .sstore(U256::from(3), U256::from(4))
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 4, "Parent's 4 slots should persist, child's discarded");
}

#[test]
fn test_top_level_exceed_is_frame_local_revert() {
    // In Rex4, even the top-level frame uses per-frame enforcement.
    // TX limit = 10, top-level frame creates 11 slots → per-frame exceed → Revert.
    // (This differs from pre-Rex4, where this would be a TX-level Halt.)
    let code = write_n_slots(BytecodeBuilder::default(), 11).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact(MegaSpecId::REX4, &mut db, 10, tx).unwrap();

    assert!(
        matches!(result.result, ExecutionResult::Revert { .. }),
        "Rex4 top-level exceed should be a revert, not halt"
    );
}

#[test]
fn test_child_exceed_followed_by_sibling_success() {
    // Child A exceeds (reverted), Child B succeeds within budget.
    // TX limit = 100, each child budget = 98.
    let child_a_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let child_b_code = write_n_slots(BytecodeBuilder::default(), 50).stop().build();

    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP);
    let parent_code = append_call(parent_code, CONTRACT2, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_a_code)
        .account_code(CONTRACT2, child_b_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 50, "Only Child B's 50 slots should persist");
}

// ============================================================================
// 3. REVERT DATA ENCODING
// ============================================================================

#[test]
fn test_revert_data_contains_mega_limit_exceeded() {
    // TX limit = 100 → child budget = 98.
    // Child exceeds → parent captures revert data → TX returns it.
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();

    // Parent: CALL child, copy return data to memory, RETURN it.
    let parent_code =
        append_call_and_return_revert_data(BytecodeBuilder::default(), CONTRACT, 50_000_000)
            .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    let output = match &result.result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        _ => panic!("Expected success"),
    };

    // Decode the revert data
    let decoded = MegaLimitExceeded::abi_decode(&output).expect("should decode MegaLimitExceeded");
    assert_eq!(decoded.kind, 3, "kind should be 3 (StateGrowth)");
    assert_eq!(decoded.limit, 98, "limit should be the child's per-frame budget (98)");
}

#[test]
fn test_revert_data_limit_value_matches_frame_budget() {
    // TX limit = 1000, parent uses 200, child budget = 800 * 98/100 = 784
    // Child exceeds → revert data says limit = 784.
    let child_code = write_n_slots(BytecodeBuilder::default(), 785).stop().build();

    let parent_code = write_n_slots(BytecodeBuilder::default(), 200);
    let parent_code = append_call_and_return_revert_data(parent_code, CONTRACT, 50_000_000).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success());
    let output = match &result.result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        _ => panic!("Expected success"),
    };

    let decoded = MegaLimitExceeded::abi_decode(&output).expect("should decode MegaLimitExceeded");
    assert_eq!(decoded.kind, 3, "kind should be 3 (StateGrowth)");
    assert_eq!(decoded.limit, 784, "limit should be 784 (800 * 98/100)");
}

#[test]
fn test_revert_data_empty_for_tx_level_exceed() {
    // REX3 (pre-Rex4): child exceeds TX-level state growth → Halt, no revert data.
    let code = write_n_slots(BytecodeBuilder::default(), 11).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact(MegaSpecId::REX3, &mut db, 10, tx).unwrap();

    assert!(result.result.is_halt());
    assert!(is_state_growth_limit_exceeded(&result));
}

// ============================================================================
// 4. NESTED FRAME SCENARIOS
// ============================================================================

#[test]
fn test_deeply_nested_budget_propagation() {
    // TX limit = 1000
    // Level 0 (parent): budget = 1000
    // Level 1 (A): budget = 1000 * 98/100 = 980
    // Level 2 (B): budget = 980 * 98/100 = 960
    // Level 3 (C): budget = 960 * 98/100 = 940
    // C creates 940 slots → succeeds
    let level3_code = write_n_slots(BytecodeBuilder::default(), 940).stop().build();

    let level2_code =
        append_call(BytecodeBuilder::default(), CONTRACT3, 80_000_000).append(POP).stop().build();

    let level1_code =
        append_call(BytecodeBuilder::default(), CONTRACT2, 85_000_000).append(POP).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 90_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, level1_code)
        .account_code(CONTRACT2, level2_code)
        .account_code(CONTRACT3, level3_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 940, "Level 3 should succeed with 940 slots");
}

#[test]
fn test_deeply_nested_exceed() {
    // Same setup but C creates 941 → exceeds 940 budget → reverted at level 3.
    let level3_code = write_n_slots(BytecodeBuilder::default(), 941).stop().build();

    let level2_code =
        append_call(BytecodeBuilder::default(), CONTRACT3, 80_000_000).append(POP).stop().build();

    let level1_code =
        append_call(BytecodeBuilder::default(), CONTRACT2, 85_000_000).append(POP).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 90_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, level1_code)
        .account_code(CONTRACT2, level2_code)
        .account_code(CONTRACT3, level3_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success(), "Parent chain should succeed");
    assert_eq!(state_growth, 0, "Level 3's growth discarded on revert");
}

#[test]
fn test_nested_child_reverts_growth_discarded() {
    // Parent (3 slots) → Child (2 slots, succeeds) → Grandchild (creates 5, REVERTs normally)
    let grandchild_code = write_n_slots(BytecodeBuilder::default(), 5).revert().build();

    let child_code = write_n_slots(BytecodeBuilder::default(), 2);
    let child_code = append_call(child_code, CONTRACT2, 50_000_000).append(POP).stop().build();

    let parent_code = write_n_slots(BytecodeBuilder::default(), 3);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code)
        .account_code(CONTRACT2, grandchild_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 5, "Parent (3) + child (2) = 5; grandchild's 5 discarded");
}

#[test]
fn test_nested_exceed_absorbed_at_correct_level() {
    // Parent → Child → Grandchild exceeds per-frame budget
    // Grandchild reverted, Child continues (writes 2 slots), Parent succeeds.
    // TX limit = 1000
    let grandchild_code = write_n_slots(BytecodeBuilder::default(), 961).stop().build();

    let child_code = append_call(BytecodeBuilder::default(), CONTRACT2, 80_000_000)
        .append(POP) // grandchild reverted
        .sstore(U256::from(0), U256::from(1)) // child continues
        .sstore(U256::from(1), U256::from(2))
        .stop()
        .build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 90_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code)
        .account_code(CONTRACT2, grandchild_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1000, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 2, "Only child's 2 slots should persist");
}

#[test]
fn test_multiple_nested_exceeds() {
    // Parent calls Child A (exceeds), then Child B (exceeds), then Child C (within budget).
    // TX limit = 100, each child budget = 98.
    let child_a_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let child_b_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let child_c_code = write_n_slots(BytecodeBuilder::default(), 50).stop().build();

    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 30_000_000).append(POP);
    let parent_code = append_call(parent_code, CONTRACT2, 30_000_000).append(POP);
    let parent_code = append_call(parent_code, CONTRACT3, 30_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_a_code)
        .account_code(CONTRACT2, child_b_code)
        .account_code(CONTRACT3, child_c_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 50, "Only Child C's 50 slots should persist");
}

// ============================================================================
// 5. BACKWARD COMPATIBILITY
// ============================================================================

#[test]
fn test_rex3_no_per_frame_limits() {
    // REX3 spec: no per-frame limits. Child creates 99 slots with TX limit = 100 → succeeds
    // (only TX-level check: 99 <= 100).
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX3, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success(), "REX3 has no per-frame limits");
    assert_eq!(state_growth, 99);
}

#[test]
fn test_rex3_exceed_halts_not_reverts() {
    // REX3 spec, TX limit = 10, creates 11 slots → Halt (TX-level exceed).
    let code = write_n_slots(BytecodeBuilder::default(), 11).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact(MegaSpecId::REX3, &mut db, 10, tx).unwrap();

    assert!(result.result.is_halt(), "REX3 should halt on TX-level exceed");
    assert!(is_state_growth_limit_exceeded(&result));
}

// ============================================================================
// 6. REFUND AND NET GROWTH INTERACTIONS
// ============================================================================

#[test]
fn test_refund_increases_remaining_budget() {
    // TX limit = 100, child budget = 98
    // Child creates 90 slots, clears 50 back to zero.
    // Net = 90 - 50 = 40, within budget (98) → succeeds.
    let child_code = write_n_clear_m_slots(BytecodeBuilder::default(), 90, 50).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 40, "Net growth = 90 - 50 = 40");
}

#[test]
fn test_child_within_budget_after_refunds() {
    // TX limit = 50, child budget = 49
    // Child creates 40 slots, clears 10 → net = 30, within budget (49) → succeeds.
    // Note: the frame-local check is eager — `used` must not exceed `limit` at any point
    // during execution. Here the peak usage (40) is within the budget (49).
    let child_code = write_n_clear_m_slots(BytecodeBuilder::default(), 40, 10).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 50, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 30, "Net growth = 40 - 10 = 30");
}

#[test]
fn test_child_exceed_with_insufficient_refunds() {
    // TX limit = 50, child budget = 49
    // Child creates 60, clears 10 → net = 50 > 49 → exceeds → reverted.
    let child_code = write_n_clear_m_slots(BytecodeBuilder::default(), 60, 10).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 50, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed after child revert");
    assert_eq!(state_growth, 0, "Child's growth discarded on revert");
}

#[test]
fn test_eager_check_reverts_despite_future_refunds() {
    // TX limit = 50, child budget = 49
    // Child creates 50 slots, then would clear 10 → final net would be 40.
    // But the eager check fires when net reaches 50 (at slot 50), exceeding budget 49.
    // The child reverts before reaching the clearing step.
    let child_code = write_n_clear_m_slots(BytecodeBuilder::default(), 50, 10).stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 50, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed after child revert");
    assert_eq!(state_growth, 0, "Child's growth discarded — eager check reverted before refunds");
}

#[test]
fn test_rex3_eager_check_halts_despite_future_refunds() {
    // REX3: TX limit = 50, no per-frame limits.
    // Code creates 51 slots, then would clear 10 → final net would be 41.
    // But the eager TX-level check fires when net reaches 51, exceeding limit 50.
    // The TX halts before reaching the clearing step.
    let code = write_n_clear_m_slots(BytecodeBuilder::default(), 51, 10).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact(MegaSpecId::REX3, &mut db, 50, tx).unwrap();

    assert!(result.result.is_halt(), "REX3 should halt on TX-level exceed");
    assert!(is_state_growth_limit_exceeded(&result));
}

#[test]
fn test_create_clear_create_within_budget() {
    // TX limit = 52 → child budget = 52 * 98/100 = 50
    // Child: create 40 slots (net=40), clear 10 (net=30), create 20 more (net=50).
    // Net never exceeds 50 at any point → child succeeds.
    let mut b = write_n_slots(BytecodeBuilder::default(), 40);
    for i in 0..10_u64 {
        b = b.sstore(U256::from(i), U256::from(0));
    }
    for i in 40..60_u64 {
        b = b.sstore(U256::from(i), U256::from(i + 1));
    }
    let child_code = b.stop().build();

    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 52, tx).unwrap();

    assert!(result.result.is_success(), "Child should succeed — net never exceeds 50");
    assert_eq!(state_growth, 50, "Net growth = 40 - 10 + 20 = 50");
}

#[test]
fn test_rex3_create_clear_create_within_budget() {
    // REX3: TX limit = 50, no per-frame limits.
    // Code: create 40 slots (net=40), clear 10 (net=30), create 20 more (net=50).
    // Net never exceeds 50 → TX succeeds.
    let mut b = write_n_slots(BytecodeBuilder::default(), 40);
    for i in 0..10_u64 {
        b = b.sstore(U256::from(i), U256::from(0));
    }
    for i in 40..60_u64 {
        b = b.sstore(U256::from(i), U256::from(i + 1));
    }
    let code = b.stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX3, &mut db, 50, tx).unwrap();

    assert!(result.result.is_success(), "TX should succeed — net never exceeds 50");
    assert_eq!(state_growth, 50, "Net growth = 40 - 10 + 20 = 50");
}

#[test]
fn test_child_refund_reflected_in_sibling_budget() {
    // TX limit = 100
    // Child A: creates 10 slots, clears 5 → net = 5, succeeds.
    // Parent remaining after Child A = 100 - 5 = 95.
    // Child B budget = 95 * 98/100 = 93.
    // Child B creates 93 slots → succeeds.
    // This proves Child A's refunds flow back to the parent, giving Child B a larger budget
    // than if the gross usage (10) were used (which would yield 90 * 98/100 = 88).
    let child_a_code = write_n_clear_m_slots(BytecodeBuilder::default(), 10, 5).stop().build();
    let child_b_code = write_n_slots(BytecodeBuilder::default(), 93).stop().build();

    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP);
    let parent_code = append_call(parent_code, CONTRACT2, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_a_code)
        .account_code(CONTRACT2, child_b_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 5 + 93, "Child A net (5) + Child B (93) = 98");
}
