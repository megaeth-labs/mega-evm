//! Tests for Rex4 per-frame limits on `DataSize`, `KVUpdate`, and `ComputeGas`.
//!
//! Rex4 extends per-frame budgets to all four resource dimensions (`DataSize`, `KVUpdate`,
//! `ComputeGas`, and `StateGrowth` — the last already tested in `frame_state_growth.rs`).
//!
//! Each inner call frame receives `remaining * 98 / 100` of the parent's remaining budget.
//! When a frame exceeds its per-frame budget, it reverts (not halts) with ABI-encoded
//! `MegaLimitExceeded(uint8 kind, uint64 limit)` revert data.
//!
//! Behavior differences from `StateGrowth`:
//! - **`DataSize` / `KVUpdate`**: The reverted child's discardable usage is dropped, protecting the
//!   parent's budget (same semantics as `StateGrowth`).
//! - **`ComputeGas`**: Gas is always persistent — even after a child frame reverts due to exceeding
//!   its per-frame compute gas budget, the parent's total compute gas still increases by the
//!   child's actual gas used. Per-frame limits act as "early termination guardrails", not budget
//!   protection.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaLimitExceeded, MegaSpecId,
    MegaTransaction, MegaTransactionError, ACCOUNT_INFO_WRITE_SIZE, BASE_TX_SIZE,
    STORAGE_SLOT_WRITE_SIZE,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
    handler::EvmTr,
    DatabaseCommit, DatabaseRef,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");
const CONTRACT2: Address = address!("0000000000000000000000000000000000100003");

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Executes a transaction with specified data size and KV update limits (compute gas unlimited).
fn transact_data_kv(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    data_limit: u64,
    kv_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, MegaTransactionError>>
{
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(data_limit)
            .with_tx_kv_updates_limit(kv_limit),
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
    let usage = ctx.additional_limit.borrow().get_usage();
    Ok((r, usage.data_size, usage.kv_updates))
}

/// Executes a transaction with specified compute gas limit only (data/kv unlimited).
fn transact_compute(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    compute_gas_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64), EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(compute_gas_limit),
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
    let compute_gas = ctx.additional_limit.borrow().get_usage().compute_gas;
    Ok((r, compute_gas))
}

fn default_tx_builder(to: Address) -> TxEnvBuilder {
    TxEnvBuilder::default().caller(CALLER).call(to).gas_limit(100_000_000)
}

/// Builds bytecode that writes `n` distinct storage slots to non-zero values.
fn write_n_slots(mut builder: BytecodeBuilder, n: u64) -> BytecodeBuilder {
    for i in 0..n {
        builder = builder.sstore(U256::from(i), U256::from(i + 1));
    }
    builder
}

/// Appends a CALL to `target` with the given gas amount.
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

/// Appends a CALL that captures the revert data and RETURNs it.
fn append_call_and_return_revert_data(
    builder: BytecodeBuilder,
    target: Address,
    gas: u64,
) -> BytecodeBuilder {
    append_call(builder, target, gas)
        .append(POP) // discard CALL success flag
        .append(RETURNDATASIZE)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .push_number(0_u64)
        .append(RETURN)
}

// ============================================================================
// DATA SIZE PER-FRAME LIMITS
// ============================================================================

/// Each SSTORE that writes a new slot costs `STORAGE_SLOT_WRITE_SIZE` (40) bytes of data size.
/// This helper computes n SSTORE intrinsic data cost.
fn n_sstore_data(n: u64) -> u64 {
    n * STORAGE_SLOT_WRITE_SIZE
}

fn tx_intrinsic_data_size() -> u64 {
    BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE
}

#[test]
fn test_data_size_child_gets_98_percent_budget() {
    // TX data limit large enough; set it so child (98% of remaining) can create n-1 slots.
    // Child budget = TX_LIMIT * 98/100.  We use TX_LIMIT = n_sstore_data(100) so child budget
    // = n_sstore_data(98).  Child writes 98 slots → succeeds.
    let limit = n_sstore_data(100);
    let child_code = write_n_slots(BytecodeBuilder::default(), 98).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success());
    // intrinsic tx data + child's 98 slots
    assert_eq!(
        data_size,
        tx_intrinsic_data_size() + n_sstore_data(98),
        "Child should succeed within 98% budget"
    );
}

#[test]
fn test_data_size_child_exceeds_budget_frame_local_revert() {
    // Child budget = n_sstore_data(98), child writes 99 slots → exceeds → frame-local Revert.
    // Parent succeeds, child's discardable data is dropped.
    let limit = n_sstore_data(100);
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed after child frame-local revert");
    assert_eq!(data_size, tx_intrinsic_data_size(), "Child's discardable data dropped on revert");
}

#[test]
fn test_data_size_child_exceed_reverts_not_halts() {
    let limit = n_sstore_data(100);
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, _) = transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success(), "TX should succeed, not halt");
    assert!(!result.result.is_halt());
}

#[test]
fn test_data_size_parent_budget_protected_after_child_revert() {
    // Child exceeds budget (data dropped), parent then writes own slots successfully.
    let limit = n_sstore_data(100);
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000)
        .append(POP)
        .sstore(U256::from(0), U256::from(42)) // parent writes after child revert
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(
        data_size,
        tx_intrinsic_data_size() + n_sstore_data(1),
        "Only parent's 1 slot should persist"
    );

    db.commit(result.state);
    let parent_val = db.storage_ref(CALLEE, U256::from(0)).unwrap();
    assert_eq!(parent_val, U256::from(42), "Parent storage should persist");
    let child_val = db.storage_ref(CONTRACT, U256::from(0)).unwrap();
    assert_eq!(child_val, U256::ZERO, "Child storage should be reverted");
}

#[test]
fn test_data_size_revert_data_encodes_mega_limit_exceeded() {
    // kind=0 (DataSize), limit=frame_budget
    let limit = n_sstore_data(100);
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let parent_code =
        append_call_and_return_revert_data(BytecodeBuilder::default(), CONTRACT, 50_000_000)
            .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, _) = transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success());
    let output = match &result.result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        _ => panic!("Expected success"),
    };

    let decoded = MegaLimitExceeded::abi_decode(&output).expect("should decode MegaLimitExceeded");
    assert_eq!(decoded.kind, 0, "kind should be 0 (DataSize)");
    // child budget = limit * 98/100
    assert_eq!(decoded.limit, limit * 98 / 100, "limit should be child's per-frame budget");
}

#[test]
fn test_data_size_top_level_exceed_is_frame_local_revert() {
    // Top-level frame exceeds its own frame budget in Rex4: should Revert, not Halt.
    let limit = n_sstore_data(100);
    let code = write_n_slots(BytecodeBuilder::default(), 101).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(matches!(result.result, ExecutionResult::Revert { .. }));
    assert!(!result.result.is_halt());
    assert_eq!(data_size, tx_intrinsic_data_size(), "Top-level discardable data should be dropped");
}

#[test]
fn test_data_size_child_budget_accounts_for_parent_usage() {
    // TX data limit = 100 slots -> 4000 bytes.
    // Parent writes 20 slots (800), remaining = 3200, child budget = 3136.
    // Child writes 78 slots (3120) -> succeeds.
    let limit = n_sstore_data(100);
    let child_code = write_n_slots(BytecodeBuilder::default(), 78).stop().build();
    let parent_code = write_n_slots(BytecodeBuilder::default(), 20);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(
        data_size,
        tx_intrinsic_data_size() + n_sstore_data(20 + 78),
        "Child budget should be computed from parent's remaining budget"
    );
}

#[test]
fn test_data_size_child_exceeds_budget_after_parent_usage() {
    // TX data limit = 100 slots -> 4000 bytes.
    // Parent writes 20 slots (800), remaining = 3200, child budget = 3136.
    // Child writes 79 slots (3160) -> exceeds and reverts.
    let limit = n_sstore_data(100);
    let child_code = write_n_slots(BytecodeBuilder::default(), 79).stop().build();
    let parent_code = write_n_slots(BytecodeBuilder::default(), 20);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(
        data_size,
        tx_intrinsic_data_size() + n_sstore_data(20),
        "Exceeded child frame should be discarded; parent usage should remain"
    );
}

#[test]
fn test_data_size_grandchild_budget_progressive_reduction() {
    // TX limit = 4000.
    // Child budget = 3920.
    // Grandchild budget = floor(3920 * 98 / 100) = 3841.
    // Grandchild writes 96 slots (3840) -> succeeds.
    let limit = n_sstore_data(100);
    let grandchild_code = write_n_slots(BytecodeBuilder::default(), 96).stop().build();
    let child_code =
        append_call(BytecodeBuilder::default(), CONTRACT2, 50_000_000).append(POP).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code)
        .account_code(CONTRACT2, grandchild_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(data_size, tx_intrinsic_data_size() + n_sstore_data(96));
}

#[test]
fn test_data_size_grandchild_exceeds_progressive_budget() {
    // TX limit = 4000.
    // Child budget = 3920.
    // Grandchild budget = floor(3920 * 98 / 100) = 3841.
    // Grandchild writes 97 slots (3880) -> exceeds and reverts.
    let limit = n_sstore_data(100);
    let grandchild_code = write_n_slots(BytecodeBuilder::default(), 97).stop().build();
    let child_code =
        append_call(BytecodeBuilder::default(), CONTRACT2, 50_000_000).append(POP).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code)
        .account_code(CONTRACT2, grandchild_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(data_size, tx_intrinsic_data_size());
}

#[test]
fn test_data_size_child_exceed_followed_by_sibling_success() {
    // Child A exceeds and reverts, Child B should still get fresh budget and succeed.
    let limit = n_sstore_data(100);
    let child_a_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let child_b_code = write_n_slots(BytecodeBuilder::default(), 98).stop().build();
    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000)
        .append(POP)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(CONTRACT2)
        .push_number(50_000_000_u64)
        .append(CALL)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_a_code)
        .account_code(CONTRACT2, child_b_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(data_size, tx_intrinsic_data_size() + n_sstore_data(98));
}

#[test]
fn test_data_size_rex3_no_per_frame_limits() {
    // REX3: child writes 99 slots with TX limit = 100 slots → succeeds (TX-level only).
    let limit = tx_intrinsic_data_size() + n_sstore_data(99);
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) =
        transact_data_kv(MegaSpecId::REX3, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success(), "REX3 has no per-frame data size limits");
    assert_eq!(data_size, tx_intrinsic_data_size() + n_sstore_data(99));
}

#[test]
fn test_data_size_rex3_tx_exceed_halts() {
    // REX3: child exceeds TX-level limit → Halt (not Revert).
    let limit = n_sstore_data(10);
    let child_code = write_n_slots(BytecodeBuilder::default(), 11).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, _) = transact_data_kv(MegaSpecId::REX3, &mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_halt(), "REX3 should halt on TX-level data size exceed");
    assert!(matches!(
        result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::DataLimitExceeded { .. }, .. }
    ));
}

// ============================================================================
// KV UPDATE PER-FRAME LIMITS
// ============================================================================

#[test]
fn test_kv_update_child_gets_98_percent_budget() {
    // TX KV limit = 100; child budget = 98. Child writes 98 slots (98 KV ops) → succeeds.
    let child_code = write_n_slots(BytecodeBuilder::default(), 98).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(result.result.is_success());
    // 1 (caller nonce from before_tx_start) + 98 sstores
    assert_eq!(kv_updates, 1 + 98, "Caller (1) + child's 98 sstores");
}

#[test]
fn test_kv_update_child_exceeds_budget_frame_local_revert() {
    // Child budget = 98. Child writes 99 slots → exceeds → Revert. Parent succeeds.
    // Child's discardable KV ops are dropped.
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed after child frame-local revert");
    // Only the persistent caller nonce update remains; child's discardable ops dropped.
    assert_eq!(kv_updates, 1, "Only caller nonce KV update persists");
}

#[test]
fn test_kv_update_revert_data_encodes_mega_limit_exceeded() {
    // kind=1 (KVUpdate), limit=child's frame budget
    let kv_limit = 100_u64;
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let parent_code =
        append_call_and_return_revert_data(BytecodeBuilder::default(), CONTRACT, 50_000_000)
            .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, _) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, kv_limit, tx).unwrap();

    assert!(result.result.is_success());
    let output = match &result.result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        _ => panic!("Expected success"),
    };

    let decoded = MegaLimitExceeded::abi_decode(&output).expect("should decode MegaLimitExceeded");
    assert_eq!(decoded.kind, 1, "kind should be 1 (KVUpdate)");
    assert_eq!(decoded.limit, kv_limit * 98 / 100, "limit should be child's per-frame budget");
}

#[test]
fn test_kv_update_top_level_exceed_is_frame_local_revert() {
    // Top-level frame exceeds in Rex4: should Revert, not Halt.
    let code = write_n_slots(BytecodeBuilder::default(), 101).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(matches!(result.result, ExecutionResult::Revert { .. }));
    assert!(!result.result.is_halt());
    assert_eq!(kv_updates, 1, "Top-level discardable KV updates should be dropped");
}

#[test]
fn test_kv_update_child_budget_accounts_for_parent_usage() {
    // TX KV limit = 100.
    // Parent writes 20 slots, remaining = 80, child budget = 78.
    // Child writes 78 -> succeeds.
    let child_code = write_n_slots(BytecodeBuilder::default(), 78).stop().build();
    let parent_code = write_n_slots(BytecodeBuilder::default(), 20);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(kv_updates, 1 + 20 + 78);
}

#[test]
fn test_kv_update_child_exceeds_budget_after_parent_usage() {
    // TX KV limit = 100.
    // Parent writes 20 slots, remaining = 80, child budget = 78.
    // Child writes 79 -> exceeds and reverts.
    let child_code = write_n_slots(BytecodeBuilder::default(), 79).stop().build();
    let parent_code = write_n_slots(BytecodeBuilder::default(), 20);
    let parent_code = append_call(parent_code, CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(kv_updates, 1 + 20);
}

#[test]
fn test_kv_update_grandchild_budget_progressive_reduction() {
    // TX limit = 100.
    // Child budget = 98.
    // Grandchild budget = 96.
    // Grandchild writes 96 -> succeeds.
    let grandchild_code = write_n_slots(BytecodeBuilder::default(), 96).stop().build();
    let child_code =
        append_call(BytecodeBuilder::default(), CONTRACT2, 50_000_000).append(POP).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code)
        .account_code(CONTRACT2, grandchild_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(kv_updates, 1 + 96);
}

#[test]
fn test_kv_update_grandchild_exceeds_progressive_budget() {
    // TX limit = 100.
    // Child budget = 98.
    // Grandchild budget = 96.
    // Grandchild writes 97 -> exceeds and reverts.
    let grandchild_code = write_n_slots(BytecodeBuilder::default(), 97).stop().build();
    let child_code =
        append_call(BytecodeBuilder::default(), CONTRACT2, 50_000_000).append(POP).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code)
        .account_code(CONTRACT2, grandchild_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(kv_updates, 1);
}

#[test]
fn test_kv_update_child_exceed_followed_by_sibling_success() {
    // Child A exceeds and reverts, Child B should still get fresh budget and succeed.
    let child_a_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let child_b_code = write_n_slots(BytecodeBuilder::default(), 98).stop().build();
    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000)
        .append(POP)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(CONTRACT2)
        .push_number(50_000_000_u64)
        .append(CALL)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_a_code)
        .account_code(CONTRACT2, child_b_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX4, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(kv_updates, 1 + 98);
}

#[test]
fn test_kv_update_rex3_no_per_frame_limits() {
    // REX3: child writes 99 KV ops with TX limit = 100 → succeeds.
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) =
        transact_data_kv(MegaSpecId::REX3, &mut db, u64::MAX, 100, tx).unwrap();

    assert!(result.result.is_success(), "REX3 has no per-frame KV limits");
    assert_eq!(kv_updates, 1 + 99);
}

#[test]
fn test_kv_update_rex3_tx_exceed_halts() {
    // REX3: child exceeds TX-level KV limit → Halt.
    let child_code = write_n_slots(BytecodeBuilder::default(), 11).stop().build();
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    // KV limit = 10: caller(1) + 10 sstores = 11 > 10
    let (result, _, _) = transact_data_kv(MegaSpecId::REX3, &mut db, u64::MAX, 10, tx).unwrap();

    assert!(result.result.is_halt(), "REX3 should halt on TX-level KV exceed");
    assert!(matches!(
        result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::KVUpdateLimitExceeded { .. }, .. }
    ));
}

// ============================================================================
// COMPUTE GAS PER-FRAME LIMITS
// ============================================================================

/// Burns approximately `target_gas` of compute gas via repeated PUSH1/POP sequences.
/// Each PUSH1+POP pair costs 3+2=5 gas.
/// Returns raw bytecode as `Bytes`.
fn burn_gas_code(target_gas: u64) -> Bytes {
    let iterations = target_gas / 5;
    let mut code = Vec::new();
    for _ in 0..iterations {
        code.push(PUSH1);
        code.push(0x00);
        code.push(POP);
    }
    code.push(STOP);
    Bytes::from(code)
}

#[test]
fn test_compute_gas_child_exceeds_budget_frame_local_revert() {
    // TX limit = 2_000_000; child budget = 1_960_000.
    // Burn 1_970_000 in child (> frame budget). Keep total TX usage under 2_000_000
    // so this remains frame-local and parent can continue.
    let tx_limit = 2_000_000_u64;
    let child_code = burn_gas_code(1_970_000);
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _compute_gas) = transact_compute(MegaSpecId::REX4, &mut db, tx_limit, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed after child frame-local revert");
    assert!(!result.result.is_halt(), "Should NOT be halt");
}

#[test]
fn test_compute_gas_child_gas_still_counts_despite_revert() {
    // Unlike StateGrowth, ComputeGas is persistent: even after per-frame revert,
    // the child's gas is counted in the parent's total.
    // Set TX limit high (2_000_000). Child budget = 1_960_000. Child burns 1_970_000.
    // Parent's total compute gas should INCLUDE the child's gas.

    let child_code = burn_gas_code(1_970_000);
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    // Child budget = 2_000_000 * 98 / 100 = 1_960_000.
    // Child burns 1_970_000 > 1_960_000 → per-frame revert.
    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, compute_gas) = transact_compute(MegaSpecId::REX4, &mut db, 2_000_000, tx).unwrap();

    assert!(result.result.is_success());
    // Child's gas persists — total should be significantly more than just the parent's minimal ops.
    // The parent itself uses very little gas (just the CALL setup and POP/STOP).
    assert!(
        compute_gas > 1_900_000,
        "Child's gas should be counted despite revert: got {compute_gas}"
    );
}

#[test]
fn test_compute_gas_revert_data_encodes_mega_limit_exceeded() {
    // kind=2 (ComputeGas), limit=child's frame budget
    let tx_limit = 2_000_000_u64;
    let child_code = burn_gas_code(1_970_000);
    let parent_code =
        append_call_and_return_revert_data(BytecodeBuilder::default(), CONTRACT, 50_000_000)
            .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact_compute(MegaSpecId::REX4, &mut db, tx_limit, tx).unwrap();

    assert!(result.result.is_success());
    let output = match &result.result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        _ => panic!("Expected success"),
    };

    let decoded = MegaLimitExceeded::abi_decode(&output).expect("should decode MegaLimitExceeded");
    assert_eq!(decoded.kind, 2, "kind should be 2 (ComputeGas)");
    assert!(
        decoded.limit <= tx_limit * 98 / 100,
        "limit should be <= 98% of tx limit after parent pre-call gas; got {}",
        decoded.limit
    );
}

#[test]
fn test_compute_gas_rex3_no_per_frame_limits() {
    // REX3: no per-frame compute gas limits. Child with budget<tx burns between budget and tx.
    // With per-frame limits this would revert; without it should succeed.
    let tx_limit = 2_000_000_u64;
    let child_code = burn_gas_code(1_970_000); // < tx limit, but > 98% frame budget
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact_compute(MegaSpecId::REX3, &mut db, tx_limit, tx).unwrap();

    assert!(result.result.is_success(), "REX3 has no per-frame compute gas limits");
}

#[test]
fn test_compute_gas_sibling_frames_independent() {
    // After child A reverts (per-frame exceed), child B still gets a fresh budget.
    // TX limit = 2_500_000. Child A budget = 2_450_000, burns 2_460_000 → Revert.
    // Parent remaining after A's gas (persistent) is still enough for child B.
    // Child B burns 10 gas and succeeds.
    //
    // This test mainly verifies the TX doesn't halt after multiple children run.
    let tx_limit = 2_500_000_u64;
    let child_a_code = burn_gas_code(2_460_000); // > 2_450_000 (98% of 2_500_000)
    let child_b_code = burn_gas_code(10); // tiny gas, should succeed

    let parent_code = append_call(BytecodeBuilder::default(), CONTRACT, 5_000_000)
        .append(POP)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(CONTRACT2)
        .push_number(5_000_000_u64)
        .append(CALL)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_a_code)
        .account_code(CONTRACT2, child_b_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact_compute(MegaSpecId::REX4, &mut db, tx_limit, tx).unwrap();

    assert!(result.result.is_success(), "TX should succeed with multiple children");
}

#[test]
fn test_compute_gas_rex4_tx_exceed_halts() {
    // TX-level compute gas exceed should halt in Rex4.
    let code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _) = transact_compute(MegaSpecId::REX4, &mut db, 1, tx).unwrap();

    assert!(result.result.is_halt(), "REX4 should halt on TX-level compute exceed");
    assert!(matches!(
        result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::ComputeGasLimitExceeded { .. }, .. }
    ));
}
