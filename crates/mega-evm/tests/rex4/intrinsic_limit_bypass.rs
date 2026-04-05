//! Regression tests for Finding 1: REX4 per-transaction resource limit bypass
//! via intrinsic usage inflation.
//!
//! Before the fix, `FrameLimitTracker::max_forward_limit()` returned `tx_entry.limit`
//! when the frame stack was empty, giving the first frame the full raw budget
//! instead of the remaining budget after intrinsic charges. This allowed a transaction
//! to exceed the configured limit while still succeeding.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaLimitControl, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionError, ACCOUNT_INFO_WRITE_SIZE, BASE_TX_SIZE,
    LIMIT_CONTROL_ADDRESS, STORAGE_SLOT_WRITE_SIZE,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        ContextTr, TxEnv,
    },
    handler::EvmTr,
    inspector::Inspector,
    interpreter::{interpreter_types::InterpreterTypes, CallInputs, CallOutcome, Gas},
};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");

// ============================================================================
// HELPERS
// ============================================================================

fn transact_data_kv(
    db: &mut MemoryDatabase,
    data_limit: u64,
    kv_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, MegaTransactionError>>
{
    let mut context = MegaContext::new(db, MegaSpecId::REX4).with_tx_runtime_limits(
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

fn default_tx_builder(to: Address) -> TxEnvBuilder {
    TxEnvBuilder::default().caller(CALLER).call(to).gas_limit(100_000_000)
}

fn write_n_slots(mut builder: BytecodeBuilder, n: u64) -> BytecodeBuilder {
    for i in 0..n {
        builder = builder.sstore(U256::from(i), U256::from(i + 1));
    }
    builder
}

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

/// Intrinsic data size of a simple transaction: base TX + caller account update.
fn tx_intrinsic_data_size() -> u64 {
    BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE
}

// ============================================================================
// TEST 1: Intrinsic + execution overflow (DataSize)
// ============================================================================

/// A REX4 transaction whose intrinsic data size plus execution-generated data
/// exceeds the configured `tx_data_size_limit` must not succeed.
///
/// Before the fix, the first frame received the full raw budget, ignoring
/// intrinsic charges already applied to `tx_entry`. The frame-local check
/// in Rex4 would pass even though total TX data exceeded the limit.
#[test]
fn test_intrinsic_plus_execution_data_size_overflow() {
    // Set tx_data_size_limit just above the intrinsic so that a few SSTOREs push it over.
    // intrinsic = BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE = 150
    // Each SSTORE writes STORAGE_SLOT_WRITE_SIZE = 40 bytes.
    // Allow room for exactly 2 SSTOREs: limit = intrinsic + 2 * 40 = 230
    let intrinsic = tx_intrinsic_data_size();
    let limit = intrinsic + 2 * STORAGE_SLOT_WRITE_SIZE;

    // Callee writes 3 SSTOREs (120 bytes) — intrinsic(150) + 120 = 270 > 230.
    let code = write_n_slots(BytecodeBuilder::default(), 3).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) = transact_data_kv(&mut db, limit, u64::MAX, tx).unwrap();

    // The transaction must not succeed: total data would be 270 > 230.
    assert!(
        !result.result.is_success(),
        "TX with intrinsic + execution data exceeding limit should not succeed, \
         data_size_used={data_size}, limit={limit}"
    );
}

// ============================================================================
// TEST 2: Intrinsic + execution overflow (KVUpdate)
// ============================================================================

/// A REX4 transaction whose intrinsic KV count plus execution KV writes
/// exceeds the configured `tx_kv_updates_limit` must not succeed.
///
/// Intrinsic KV = 1 (caller account update). Writing N storage slots adds N KVs.
#[test]
fn test_intrinsic_plus_execution_kv_update_overflow() {
    // Intrinsic KV = 1 (caller update). Allow 3 total → execution budget = 2.
    let kv_limit = 3;

    // Callee writes 3 SSTOREs → intrinsic(1) + 3 = 4 > 3.
    let code = write_n_slots(BytecodeBuilder::default(), 3).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, kv_updates) = transact_data_kv(&mut db, u64::MAX, kv_limit, tx).unwrap();

    assert!(
        !result.result.is_success(),
        "TX with intrinsic + execution KV exceeding limit should not succeed, \
         kv_updates_used={kv_updates}, limit={kv_limit}"
    );
}

// ============================================================================
// TEST 3: Intrinsic-only overflow (DataSize)
// ============================================================================

/// A transaction whose intrinsic data size alone exceeds the configured limit
/// should produce a normal execution failure (Halt with `DataLimitExceeded`),
/// not a top-level EVM error.
#[test]
fn test_intrinsic_only_data_size_overflow() {
    // Set limit to something smaller than intrinsic data size.
    // Even a minimal transaction with empty calldata has intrinsic = 150.
    let limit = 100; // Less than BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE

    let code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, _) = transact_data_kv(&mut db, limit, u64::MAX, tx).unwrap();

    // Should be a normal execution failure on the additional-limit path.
    assert!(
        result.result.is_halt(),
        "Intrinsic-only DataSize overflow should halt, got {:?}",
        result.result
    );
    assert!(matches!(
        result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::DataLimitExceeded { .. }, .. }
    ));
}

// ============================================================================
// TEST 4: Intrinsic-only overflow (KVUpdate)
// ============================================================================

/// A transaction whose intrinsic KV count exceeds the configured limit
/// should produce a normal execution failure (Halt with `KVUpdateLimitExceeded`),
/// not a top-level EVM error.
#[test]
fn test_intrinsic_only_kv_update_overflow() {
    // Intrinsic KV = 1 (caller update). Set limit to 0.
    let kv_limit = 0;

    let code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, _) = transact_data_kv(&mut db, u64::MAX, kv_limit, tx).unwrap();

    assert!(
        result.result.is_halt(),
        "Intrinsic-only KVUpdate overflow should halt, got {:?}",
        result.result
    );
    assert!(matches!(
        result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::KVUpdateLimitExceeded { .. }, .. }
    ));
}

// ============================================================================
// TEST 5: Correct first-frame budget (positive test)
// ============================================================================

/// After the fix, the first frame should receive exactly `tx_limit - intrinsic`
/// as its budget, not the full `tx_limit`.
///
/// We set the limit so that intrinsic + exactly N SSTOREs fits, and verify
/// that N SSTOREs succeed but N+1 would fail (via frame-local revert in child).
#[test]
fn test_first_frame_budget_accounts_for_intrinsic() {
    let intrinsic = tx_intrinsic_data_size();
    // Allow exactly 5 SSTOREs after intrinsic.
    let limit = intrinsic + 5 * STORAGE_SLOT_WRITE_SIZE;

    // Callee writes 5 SSTOREs — should exactly fit.
    let code = write_n_slots(BytecodeBuilder::default(), 5).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) = transact_data_kv(&mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success(), "5 SSTOREs should fit in first frame budget");
    assert_eq!(
        data_size,
        intrinsic + 5 * STORAGE_SLOT_WRITE_SIZE,
        "Total data should be intrinsic + 5 SSTOREs"
    );

    // Now try 6 SSTOREs — should exceed the first frame budget.
    let code = write_n_slots(BytecodeBuilder::default(), 6).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, _, _) = transact_data_kv(&mut db, limit, u64::MAX, tx).unwrap();

    assert!(
        !result.result.is_success(),
        "6 SSTOREs should exceed first frame budget after intrinsic deduction"
    );
}

// ============================================================================
// TEST 6: Nested frame non-regression
// ============================================================================

/// A child frame that exceeds its own budget should revert frame-locally,
/// allowing the parent to continue when the parent budget permits it.
/// This ensures the fix did not break nested frame behavior.
#[test]
fn test_nested_frame_local_revert_non_regression() {
    let intrinsic = tx_intrinsic_data_size();
    // Set limit large enough that parent has plenty of budget.
    // Parent budget = limit - intrinsic. Child gets 98% of that.
    // We pick limit so child gets ~98 SSTORE slots of budget.
    let limit = intrinsic + 100 * STORAGE_SLOT_WRITE_SIZE;

    // Child writes 99 SSTOREs — exceeds child's 98% budget → frame-local revert.
    let child_code = write_n_slots(BytecodeBuilder::default(), 99).stop().build();
    // Parent calls child, pops result (ignores revert), then stops.
    let parent_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, parent_code)
        .account_code(CONTRACT, child_code);

    let tx = default_tx_builder(CALLEE).build_fill();
    let (result, data_size, _) = transact_data_kv(&mut db, limit, u64::MAX, tx).unwrap();

    assert!(result.result.is_success(), "Parent should succeed after child frame-local revert");
    // Child's discardable data should be dropped on revert.
    assert_eq!(
        data_size, intrinsic,
        "Child's discardable data dropped on revert, only intrinsic remains"
    );
}

// ============================================================================
// TEST 7: Intrinsic-only DataSize overflow + intercepted system contract
// ============================================================================

/// When a TX targets an intercepted system contract (e.g., `MegaLimitControl`)
/// and intrinsic data size exceeds the limit, the TX must still fail.
/// This tests that the pending exceeded limit is checked before the
/// interceptor dispatch in `frame_init()`.
#[test]
fn test_intrinsic_data_size_overflow_with_intercepted_system_contract() {
    let limit = 100; // Less than intrinsic data size (~150)

    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    // Direct TX to MegaLimitControl.remainingComputeGas()
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(LIMIT_CONTROL_ADDRESS)
        .gas_limit(100_000_000)
        .data(Bytes::copy_from_slice(&IMegaLimitControl::remainingComputeGasCall::SELECTOR))
        .build_fill();

    let (result, _, _) = transact_data_kv(&mut db, limit, u64::MAX, tx).unwrap();

    assert!(
        !result.result.is_success(),
        "Intrinsic DataSize overflow must not succeed even when targeting intercepted system contract, got {:?}",
        result.result
    );
    assert!(matches!(
        result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::DataLimitExceeded { .. }, .. }
    ));
}

// ============================================================================
// TEST 8: Intrinsic-only KVUpdate overflow + intercepted system contract
// ============================================================================

/// When a TX targets an intercepted system contract and intrinsic KV count
/// exceeds the limit, the TX must still fail.
#[test]
fn test_intrinsic_kv_update_overflow_with_intercepted_system_contract() {
    let kv_limit = 0; // Intrinsic KV = 1 (caller update), exceeds limit of 0

    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(LIMIT_CONTROL_ADDRESS)
        .gas_limit(100_000_000)
        .data(Bytes::copy_from_slice(&IMegaLimitControl::remainingComputeGasCall::SELECTOR))
        .build_fill();

    let (result, _, _) = transact_data_kv(&mut db, u64::MAX, kv_limit, tx).unwrap();

    assert!(
        !result.result.is_success(),
        "Intrinsic KVUpdate overflow must not succeed even when targeting intercepted system contract, got {:?}",
        result.result
    );
    assert!(matches!(
        result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::KVUpdateLimitExceeded { .. }, .. }
    ));
}

// ============================================================================
// TEST 9: Inspector early-return + intrinsic overflow (DataSize)
// ============================================================================

/// An inspector that unconditionally intercepts every CALL, returning early
/// with a successful synthetic result. This triggers `inspect_frame_init`'s
/// early-return path, skipping both `check_pending_exceeded_limit` and
/// `before_frame_init`.
struct SkipAllCallsInspector;

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipAllCallsInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        Some(CallOutcome {
            result: revm::interpreter::InterpreterResult {
                result: revm::interpreter::InstructionResult::Stop,
                output: Bytes::new(),
                gas: Gas::new(inputs.gas_limit),
            },
            memory_offset: 0..0,
        })
    }
}

/// When an inspector intercepts the top-level call and intrinsic data size
/// exceeds the limit, the TX must still fail. Without the
/// `check_pending_exceeded_limit` check in `inspect_frame_init`, the pending
/// exceeded limit would be silently ignored and gas rescue would be missed.
#[test]
fn test_intrinsic_data_size_overflow_with_inspector_early_return() {
    let limit = 100; // Less than intrinsic data size (~150)

    let code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(limit)
            .with_tx_kv_updates_limit(u64::MAX),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let mut inspector = SkipAllCallsInspector;
    let mut evm = MegaEvm::new(context).with_inspector(&mut inspector);
    let mut tx = MegaTransaction::new(default_tx_builder(CALLEE).build_fill());
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    assert!(
        result.result.is_halt(),
        "Intrinsic DataSize overflow must halt even with inspector early-return, got {:?}",
        result.result
    );
    assert!(matches!(
        result.result,
        ExecutionResult::Halt { reason: MegaHaltReason::DataLimitExceeded { .. }, .. }
    ));

    // Verify gas rescue: most gas should be refunded since no execution happened.
    let gas_remaining = 100_000_000 - result.result.gas_used();
    assert!(
        gas_remaining > 99_000_000,
        "Expected >99M gas remaining from rescue, got {gas_remaining}"
    );
}
