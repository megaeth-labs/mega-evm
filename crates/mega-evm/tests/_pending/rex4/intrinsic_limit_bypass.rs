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

/// Intrinsic data size of a transaction with `n` bytes of calldata.
fn tx_intrinsic_data_size_with_calldata(n: u64) -> u64 {
    BASE_TX_SIZE + n + ACCOUNT_INFO_WRITE_SIZE
}

// ============================================================================
// TEST 2: Intrinsic + execution overflow (KVUpdate)
// ============================================================================

// ============================================================================
// TEST 4: Intrinsic-only overflow (KVUpdate)
// ============================================================================

// ============================================================================
// TEST 8: Intrinsic-only KVUpdate overflow + intercepted system contract
// ============================================================================

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
