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

// ============================================================================
// KV UPDATE PER-FRAME LIMITS
// ============================================================================

// ============================================================================
// COMPUTE GAS PER-FRAME LIMITS
// ============================================================================
//
// Note:
// Unlike `DataSize` / `KVUpdate`, reverted compute gas remains persistent.
// But in Rex4 the top-level compute frame still uses the remaining TX budget after intrinsic
// charges, so top-level compute-budget exhaustion is also surfaced as frame-local `Revert`.

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
