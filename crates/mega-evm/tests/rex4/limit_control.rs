//! Tests for the `MegaLimitControl` system contract.
//!
//! When a contract calls `LIMIT_CONTROL_ADDRESS.remainingComputeGas()`,
//! the interceptor returns the remaining compute gas for the current call.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::mini_rex::BLOCK_ENV_ACCESS_COMPUTE_GAS,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaLimitControl, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionError, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
};
use revm::{
    bytecode::opcode::{CALL, CALLCODE, DELEGATECALL, MSTORE, POP, RETURN, STATICCALL, TIMESTAMP},
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        ContextTr, TxEnv,
    },
    handler::EvmTr,
    interpreter::{CallInputs, CallOutcome, InterpreterTypes},
    Inspector,
};

// Test addresses
const CALLER: Address = address!("0000000000000000000000000000000000300000");
const CONTRACT: Address = address!("0000000000000000000000000000000000300001");
const CONTRACT2: Address = address!("0000000000000000000000000000000000300002");
const CONTRACT3: Address = address!("0000000000000000000000000000000000300003");

/// The 4-byte selector for `remainingComputeGas()`.
const REMAINING_COMPUTE_GAS_SELECTOR: [u8; 4] =
    IMegaLimitControl::remainingComputeGasCall::SELECTOR;
const NOT_INTERCEPTED_SELECTOR: [u8; 4] = IMegaLimitControl::NotIntercepted::SELECTOR;
const NON_ZERO_TRANSFER_SELECTOR: [u8; 4] = IMegaLimitControl::NonZeroTransfer::SELECTOR;

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Executes a transaction on the given spec.
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec);
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

/// Builds a direct transaction calling `remainingComputeGas()` on the target.
fn direct_query_tx(to: Address) -> TxEnv {
    direct_query_tx_with_value(to, U256::ZERO)
}

/// Builds a direct transaction calling `remainingComputeGas()` with a custom call value.
fn direct_query_tx_with_value(to: Address, value: U256) -> TxEnv {
    TxEnvBuilder::default()
        .caller(CALLER)
        .call(to)
        .value(value)
        .gas_limit(100_000_000)
        .data(Bytes::copy_from_slice(&REMAINING_COMPUTE_GAS_SELECTOR))
        .build_fill()
}

/// Builds a direct transaction calling `to` with custom calldata.
fn direct_tx_with_data(to: Address, data: &[u8]) -> TxEnv {
    TxEnvBuilder::default()
        .caller(CALLER)
        .call(to)
        .gas_limit(100_000_000)
        .data(Bytes::copy_from_slice(data))
        .build_fill()
}

/// Executes a transaction on the given spec with custom tx compute gas limit.
fn transact_with_compute_limit(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_compute_gas_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, MegaTransactionError>>
{
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(tx_compute_gas_limit),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx)?;
    let additional_limit = evm.ctx_ref().additional_limit.borrow();
    let usage = additional_limit.get_usage().compute_gas;
    let effective_limit = additional_limit.compute_gas_limit();
    Ok((result, usage, effective_limit))
}

/// Decodes return data as `remainingComputeGas()` output.
fn decode_remaining_compute_gas(result: &ResultAndState<MegaHaltReason>) -> u64 {
    let output = match &result.result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        _ => panic!("expected success output, got: {:?}", result.result),
    };

    IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(&output)
        .expect("should decode remainingComputeGas output")
}

/// Builds bytecode that calls `remainingComputeGas()` and returns the ABI result.
fn call_remaining_compute_gas_and_return(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR)
        .push_number(32_u64) // retSize
        .push_number(0x20_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP)
        .push_number(32_u64)
        .push_number(0x20_u64)
        .append(RETURN)
}

/// Builds bytecode that STATICCALLs `remainingComputeGas()` and returns the ABI result.
fn staticcall_remaining_compute_gas_and_return(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR)
        .push_number(32_u64) // retSize
        .push_number(0x20_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(STATICCALL)
        .append(POP)
        .push_number(32_u64)
        .push_number(0x20_u64)
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

/// Builds bytecode that DELEGATECALLs a target address with the given gas.
fn append_delegatecall(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(target)
        .push_number(gas)
        .append(DELEGATECALL)
}

/// Builds bytecode that CALLCODEs a target address with the given gas.
fn append_callcode(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(target)
        .push_number(gas)
        .append(CALLCODE)
}

/// Appends a configurable amount of pure compute work (`PUSH1; POP`).
fn burn_compute(mut builder: BytecodeBuilder, n: u64) -> BytecodeBuilder {
    for _ in 0..n {
        builder = builder.push_number(1_u64).append(POP);
    }
    builder
}

// ============================================================================
// 1. BASIC remainingComputeGas() BEHAVIOR
// ============================================================================

/// Direct transaction call to system contract should return a positive remaining value.
#[test]
fn test_direct_tx_remaining_compute_gas() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let result =
        transact(MegaSpecId::REX4, &mut db, direct_query_tx(LIMIT_CONTROL_ADDRESS)).unwrap();
    assert!(result.result.is_success());

    let remaining = decode_remaining_compute_gas(&result);
    assert!(remaining > 0, "remaining compute gas should be positive");
}

/// Unknown selector should not be intercepted and should fall through to contract fallback revert.
#[test]
fn test_direct_tx_unknown_selector_falls_through_and_reverts_not_intercepted() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE);

    let result = transact(
        MegaSpecId::REX4,
        &mut db,
        direct_tx_with_data(LIMIT_CONTROL_ADDRESS, &[0xde, 0xad, 0xbe, 0xef]),
    )
    .unwrap();
    assert!(
        !result.result.is_success(),
        "unknown selector should fall through and revert, got: {:?}",
        result.result
    );

    let output = result.result.output().expect("revert should include output");
    assert_eq!(
        output.len(),
        4,
        "fallback should return only NotIntercepted selector, got {} bytes",
        output.len()
    );
    assert_eq!(
        &output[..4],
        &NOT_INTERCEPTED_SELECTOR,
        "unknown selector should revert with NotIntercepted()"
    );
}

/// Direct transaction query with non-zero value should revert.
#[test]
fn test_direct_tx_remaining_compute_gas_with_value_reverts() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let result = transact(
        MegaSpecId::REX4,
        &mut db,
        direct_query_tx_with_value(LIMIT_CONTROL_ADDRESS, U256::from(1_u64)),
    )
    .unwrap();
    assert!(
        !result.result.is_success(),
        "Direct TX query with non-zero value should revert, got: {:?}",
        result.result
    );
    let output = result.result.output().expect("revert should include output");
    assert_eq!(output.len(), 4, "non-zero transfer revert should return selector only");
    assert_eq!(
        &output[..4],
        &NON_ZERO_TRANSFER_SELECTOR,
        "non-zero transfer should revert with NonZeroTransfer()"
    );
}

/// STATICCALL query should also return a positive remaining value.
#[test]
fn test_remaining_compute_gas_staticcall() {
    let code = staticcall_remaining_compute_gas_and_return(BytecodeBuilder::default()).build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, code);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx(CONTRACT)).unwrap();
    assert!(result.result.is_success());

    let remaining = decode_remaining_compute_gas(&result);
    assert!(remaining > 0, "remaining compute gas should be positive for STATICCALL query");
}

// ============================================================================
// 2. COMPUTE ACCOUNTING
// ============================================================================

/// More compute work should reduce `remainingComputeGas()`.
#[test]
fn test_remaining_compute_gas_decreases_after_compute_work() {
    let base_contract_code =
        call_remaining_compute_gas_and_return(BytecodeBuilder::default()).build();

    let heavy_contract_code =
        call_remaining_compute_gas_and_return(burn_compute(BytecodeBuilder::default(), 20_000))
            .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, base_contract_code)
        .account_code(CONTRACT2, heavy_contract_code);

    let base_result = transact(MegaSpecId::REX4, &mut db, default_tx(CONTRACT)).unwrap();
    let heavy_result = transact(MegaSpecId::REX4, &mut db, default_tx(CONTRACT2)).unwrap();

    assert!(base_result.result.is_success());
    assert!(heavy_result.result.is_success());

    let base_remaining = decode_remaining_compute_gas(&base_result);
    let heavy_remaining = decode_remaining_compute_gas(&heavy_result);
    assert!(
        heavy_remaining < base_remaining,
        "remaining compute gas should decrease after more compute work: heavy={}, base={}",
        heavy_remaining,
        base_remaining
    );
}

/// Two sequential queries in the same frame should return decreasing values,
/// and the difference should be small (only the overhead of the query + glue instructions).
#[test]
fn test_remaining_compute_gas_sequential_queries_decrease() {
    // Contract: query remainingComputeGas twice, return both values as (first, second).
    // First query result stored at 0x20, second at 0x40, return 64 bytes from 0x20.
    let code = BytecodeBuilder::default()
        // First query
        .mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR)
        .push_number(32_u64) // retSize
        .push_number(0x20_u64) // retOffset — store first result at 0x20
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(100_000_u64)
        .append(CALL)
        .append(POP)
        // Second query (selector still at 0x00)
        .push_number(32_u64) // retSize
        .push_number(0x40_u64) // retOffset — store second result at 0x40
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(100_000_u64)
        .append(CALL)
        .append(POP)
        // Return both (64 bytes from 0x20)
        .push_number(64_u64)
        .push_number(0x20_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, code);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx(CONTRACT)).unwrap();
    assert!(result.result.is_success());

    let output = result.result.output().expect("should have output");
    assert_eq!(output.len(), 64, "should return two 32-byte values");

    let first = U256::from_be_slice(&output[..32]).saturating_to::<u64>();
    let second = U256::from_be_slice(&output[32..64]).saturating_to::<u64>();

    assert!(
        second < first,
        "second query should return less than first (first={first}, second={second})"
    );

    // The difference should be small — only the overhead of the glue instructions
    // between the two queries (a few PUSHes + CALL + POP ≈ tens of gas).
    let diff = first - second;
    assert!(
        diff < 1000,
        "difference between sequential queries should be small (diff={diff}), \
         confirming the query itself doesn't consume significant compute gas"
    );
}

/// Direct top-level query should return `tx_limit` minus intrinsic compute gas.
#[test]
fn test_remaining_compute_gas_exact_value_matches_tracker() {
    let tx_limit = 1_000_000_u64;
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let (result, used_compute, _effective_limit) = transact_with_compute_limit(
        MegaSpecId::REX4,
        &mut db,
        tx_limit,
        direct_query_tx(LIMIT_CONTROL_ADDRESS),
    )
    .unwrap();
    assert!(result.result.is_success(), "direct query should succeed");

    let remaining = decode_remaining_compute_gas(&result);
    assert!(
        remaining < tx_limit,
        "top-level remaining should be less than tx_limit due to intrinsic compute gas \
         (remaining={remaining}, tx_limit={tx_limit})"
    );
    assert_eq!(
        remaining,
        tx_limit - used_compute,
        "top-level remaining should equal tx_limit minus intrinsic compute gas \
         (tx_limit={tx_limit}, used={used_compute})"
    );
}

/// Inner-call query returns the caller's frame remaining, not the 98/100 forwarded child budget.
///
/// A contract calling `remainingComputeGas()` with minimal overhead should get a value
/// above `tx_limit * 98/100`.  If the implementation mistakenly used `max_forward_limit()`
/// (which applies the 98/100 forwarding ratio), the returned value would be at most
/// `tx_limit * 98/100`, and this assertion would catch it.
#[test]
fn test_remaining_compute_gas_inner_call_returns_frame_remaining() {
    let tx_limit = 100_000_000_u64;
    let forwarded_budget = tx_limit / 100 * 98; // 98_000_000

    // Contract with minimal bytecode: just query remainingComputeGas and return.
    // The small instruction overhead (MSTORE, PUSH, CALL, POP, RETURN) is negligible
    // compared to the 2% margin between frame remaining and forwarded budget.
    let contract_code = call_remaining_compute_gas_and_return(BytecodeBuilder::default()).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, contract_code);

    let (result, _, _) =
        transact_with_compute_limit(MegaSpecId::REX4, &mut db, tx_limit, default_tx(CONTRACT))
            .unwrap();

    assert!(result.result.is_success());

    let remaining = decode_remaining_compute_gas(&result);

    // The returned value must be strictly above the forwarded budget.
    // A correct implementation returns frame.remaining() ≈ tx_limit - small_overhead,
    // while a wrong implementation (max_forward_limit) would return ≈ forwarded_budget - overhead.
    assert!(
        remaining > forwarded_budget,
        "inner-call remaining ({remaining}) should exceed forwarded budget ({forwarded_budget}); \
         if not, the implementation is likely returning max_forward_limit (98/100) instead of frame remaining"
    );
    assert!(
        remaining < tx_limit,
        "inner-call remaining ({remaining}) should be less than tx_limit ({tx_limit}) due to instruction overhead"
    );
}

/// Two-level nesting: TX → A → B → `remainingComputeGas()`.
///
/// A burns significant compute before calling B. B queries `remainingComputeGas()`.
/// The returned value should reflect B's frame remaining (derived from A's remaining at
/// call time, with the 98/100 forwarding ratio applied), NOT A's frame remaining.
#[test]
fn test_remaining_compute_gas_two_level_nesting_returns_inner_frame() {
    let tx_limit = 100_000_000_u64;

    // B: minimal contract that queries remainingComputeGas and returns it.
    let b_code = call_remaining_compute_gas_and_return(BytecodeBuilder::default()).build();

    // A: burns significant compute, then CALLs B, then returns B's result.
    // After burning compute, A's frame remaining is reduced. B gets 98/100 of A's remaining.
    let a_code = burn_compute(BytecodeBuilder::default(), 50_000);
    // CALL B with retSize=32, retOffset=0x20
    let a_code = a_code
        .push_number(32_u64) // retSize
        .push_number(0x20_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(CONTRACT2)
        .push_number(50_000_000_u64) // gas
        .append(CALL)
        .append(POP)
        // Return B's result
        .push_number(32_u64)
        .push_number(0x20_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, a_code)
        .account_code(CONTRACT2, b_code);

    let (result, _, _) =
        transact_with_compute_limit(MegaSpecId::REX4, &mut db, tx_limit, default_tx(CONTRACT))
            .unwrap();
    assert!(result.result.is_success());

    let remaining = decode_remaining_compute_gas(&result);

    // B's frame budget = 98/100 of (A's frame remaining at call time).
    // A's frame remaining ≈ tx_limit - overhead - burn_compute_cost.
    // If the implementation incorrectly returned A's frame remaining, we'd see a value
    // much higher than B's budget (close to A's remaining ≈ tx_limit - burn overhead).
    // B's budget should be strictly less than 98/100 of tx_limit due to A's compute overhead.
    let max_b_budget = tx_limit / 100 * 98; // 98_000_000
    assert!(
        remaining < max_b_budget,
        "B's frame remaining ({remaining}) should be less than max possible B budget \
         ({max_b_budget}), proving it reflects B's frame, not A's"
    );
    assert!(remaining > 0, "B's frame remaining should be positive");
}

/// Compute gas in reverted inner calls is still persistent and should reduce remaining value.
#[test]
fn test_remaining_compute_gas_persistent_after_inner_revert() {
    // Base contract: just queries remainingComputeGas and returns it.
    let base_contract_code =
        call_remaining_compute_gas_and_return(BytecodeBuilder::default()).build();

    // Reverting child: burns compute then reverts.
    let reverting_child_code = burn_compute(BytecodeBuilder::default(), 20_000).revert().build();

    // CONTRACT3: call reverting child, then query remainingComputeGas.
    let caller_code = append_call(BytecodeBuilder::default(), CONTRACT2, 50_000_000).append(POP);
    let caller_code = call_remaining_compute_gas_and_return(caller_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, base_contract_code)
        .account_code(CONTRACT2, reverting_child_code)
        .account_code(CONTRACT3, caller_code);

    let base_result = transact(MegaSpecId::REX4, &mut db, default_tx(CONTRACT)).unwrap();
    let after_revert_result = transact(MegaSpecId::REX4, &mut db, default_tx(CONTRACT3)).unwrap();

    assert!(base_result.result.is_success());
    assert!(after_revert_result.result.is_success());

    let base_remaining = decode_remaining_compute_gas(&base_result);
    let after_revert_remaining = decode_remaining_compute_gas(&after_revert_result);

    assert!(
        after_revert_remaining < base_remaining,
        "remaining should be lower after reverted inner call (compute gas is persistent): \
         after_revert={}, base={}",
        after_revert_remaining,
        base_remaining
    );
}

// ============================================================================
// 3. DETENTION INTERACTION
// ============================================================================

/// After volatile data access (TIMESTAMP), the returned value should be capped by the TX
/// detained limit, reflecting the actual compute gas available before execution halts.
#[test]
fn test_remaining_compute_gas_clamped_by_detention_limit() {
    let tx_limit = 100_000_000_u64;
    let contract_code = call_remaining_compute_gas_and_return(
        BytecodeBuilder::default().append(TIMESTAMP).append(POP),
    )
    .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, contract_code);

    let limits = EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(tx_limit);
    // Set the block env access detention cap so TIMESTAMP triggers detention.
    let limits = EvmTxRuntimeLimits {
        block_env_access_compute_gas_limit: BLOCK_ENV_ACCESS_COMPUTE_GAS,
        ..limits
    };

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4).with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(default_tx(CONTRACT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    assert!(result.result.is_success(), "query transaction should succeed");

    let remaining = decode_remaining_compute_gas(&result);
    assert!(remaining > 0, "remaining compute gas should be positive");
    assert!(
        remaining <= BLOCK_ENV_ACCESS_COMPUTE_GAS,
        "remaining should be capped by detained limit after TIMESTAMP access \
         (remaining={remaining}, cap={BLOCK_ENV_ACCESS_COMPUTE_GAS})"
    );
}

// ============================================================================
// 4. INSPECTOR VISIBILITY
// ============================================================================

/// An inspector that records all `call` and `call_end` invocations.
#[derive(Default)]
struct CallTrackingInspector {
    calls: Vec<Address>,
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

/// System contract interception should still be visible to inspectors.
#[test]
fn test_inspector_sees_remaining_compute_gas_system_call() {
    let contract_code = call_remaining_compute_gas_and_return(BytecodeBuilder::default()).build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, contract_code);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut inspector = CallTrackingInspector::default();
    let mut evm = MegaEvm::new(context).with_inspector(&mut inspector);
    let mut tx = MegaTransaction::new(default_tx(CONTRACT));
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    assert!(result.result.is_success(), "transaction should succeed");

    assert_eq!(inspector.calls.len(), 2, "inspector should see two call hooks");
    assert_eq!(inspector.calls[0], CONTRACT, "first call should be top-level contract");
    assert_eq!(
        inspector.calls[1], LIMIT_CONTROL_ADDRESS,
        "second call should target remaining compute gas system contract"
    );

    assert_eq!(inspector.call_ends.len(), 2, "inspector should see two call_end hooks");
    assert_eq!(
        inspector.call_ends[0], LIMIT_CONTROL_ADDRESS,
        "first call_end should be remaining compute gas system contract"
    );
    assert_eq!(inspector.call_ends[1], CONTRACT, "second call_end should be top-level contract");
}

// ============================================================================
// 5. CALL VARIANTS
// ============================================================================

/// DELEGATECALL to the contract should NOT be intercepted.
/// The scheme guard in `frame_init` rejects `DELEGATECALL` and `CALLCODE` before any
/// interceptor sees the call; the unit-level coverage lives in `src/system/intercept.rs`.
#[test]
fn test_delegatecall_not_intercepted() {
    // Parent: DELEGATECALL to LIMIT_CONTROL_ADDRESS with selector,
    // then return success flag as 32-byte word.
    let parent_code = BytecodeBuilder::default().mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR);
    let parent_code = append_delegatecall(parent_code, LIMIT_CONTROL_ADDRESS, 100_000_u64)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, parent_code)
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx(CONTRACT)).unwrap();
    assert!(result.result.is_success(), "parent tx should succeed");

    let output = result.result.output().expect("should have output");
    let success_flag = U256::from_be_slice(output);
    assert_eq!(
        success_flag,
        U256::from(0),
        "DELEGATECALL should fail because it is not intercepted"
    );
}

/// CALLCODE to the contract should NOT be intercepted.
/// Same scheme-guard policy as `DELEGATECALL` above.
#[test]
fn test_callcode_not_intercepted() {
    // Parent: CALLCODE to LIMIT_CONTROL_ADDRESS with selector,
    // then return success flag as 32-byte word.
    let parent_code = BytecodeBuilder::default().mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR);
    let parent_code = append_callcode(parent_code, LIMIT_CONTROL_ADDRESS, 100_000_u64)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, parent_code)
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx(CONTRACT)).unwrap();
    assert!(result.result.is_success(), "parent tx should succeed");

    let output = result.result.output().expect("should have output");
    let success_flag = U256::from_be_slice(output);
    assert_eq!(success_flag, U256::from(0), "CALLCODE should fail because it is not intercepted");
}

// ============================================================================
// 6. BACKWARD COMPATIBILITY
// ============================================================================

/// On Rex3, calling the address has no special interception behavior.
#[test]
fn test_pre_rex4_no_interception() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));

    let result =
        transact(MegaSpecId::REX3, &mut db, direct_query_tx(LIMIT_CONTROL_ADDRESS)).unwrap();

    assert!(result.result.is_success());
    let output = result.result.output().expect("success must include output");
    assert!(output.is_empty(), "REX3 should not intercept this system contract call");
}
