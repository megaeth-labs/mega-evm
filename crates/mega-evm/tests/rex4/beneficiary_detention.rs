//! Regression tests for Finding 2: Gas Detention Enforcement Gap via CALL to Beneficiary.
//!
//! Before the fix:
//! - `wrap_call_volatile_check` never called `apply_compute_gas_limit!`, so CALL/STATICCALL/
//!   DELEGATECALL/CALLCODE to the beneficiary marked the tracker but never propagated the detained
//!   limit into `AdditionalLimit`.
//! - `on_new_tx()` called `check_tx_beneficiary_access()` before `additional_limit.reset()`, so
//!   eager beneficiary detention was immediately cleared.
//! - SELFBALANCE had no volatile detention wrapper, so a beneficiary contract executing SELFBALANCE
//!   never triggered gas detention.
//!
//! ## Beneficiary Detention Path Checklist
//!
//! | Path | Trigger | Wrapper / Hook | Test |
//! |---|---|---|---|
//! | CALL to beneficiary | `load_account_delegated` | `wrap_call_volatile_check!` + `apply_compute_gas_limit!` | test 1, 1b |
//! | STATICCALL to beneficiary | `load_account_delegated` | `wrap_call_volatile_check!` + `apply_compute_gas_limit!` | test 2 |
//! | DELEGATECALL to beneficiary | `load_account_delegated` | `wrap_call_volatile_check!` + `apply_compute_gas_limit!` | test 3 |
//! | CALLCODE to beneficiary | `load_account_delegated` | `wrap_call_volatile_check!` + `apply_compute_gas_limit!` | test 4 |
//! | TX sender = beneficiary | `on_new_tx` eager | `check_tx_beneficiary_access` + sync (REX4) | test 5, 5b |
//! | TX recipient = beneficiary | `on_new_tx` eager | `check_tx_beneficiary_access` + sync (REX4) | test 6, 6b |
//! | SELFBALANCE in beneficiary | `host.balance()` | `volatile_data_ext::selfbalance` | test 7 (integration), 9 (address sensitivity) |
//! | BALANCE(beneficiary) | `host.balance()` | `wrap_op_detain_gas_conditional!` | (covered in `block_env_gas_limit.rs`) |
//! | CALL to non-beneficiary | — | no trigger | test 8 (negative) |
//! | SELFBALANCE in non-beneficiary | — | no trigger | test 9 (negative) |
//! | Child reverts after CALL to beneficiary | `load_account_delegated` | detention persists | test 1b |
//! | disableVolatileDataAccess + CALL beneficiary | — | CALL blocked by `wrap_call_volatile_check` | (covered in `access_control.rs`) |
//! | disableVolatileDataAccess + SELFBALANCE beneficiary | — | revert before exec | test 10 |
//! | Detention + intrinsic DataSize overflow | `on_new_tx` eager | halt with DataLimitExceeded | test 11 |
//! | Detention + execution data limit | `wrap_call_volatile_check` | data limit independent of detention | test 12 |

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaAccessControl, IMegaLimitControl, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError, ACCESS_CONTROL_ADDRESS,
    LIMIT_CONTROL_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
        BlockEnv, TxEnv,
    },
    handler::EvmTr,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000400000");
const CALLEE: Address = address!("0000000000000000000000000000000000400001");
/// Used as the block beneficiary in these tests.
const BENEFICIARY: Address = address!("0000000000000000000000000000000000400099");

/// The gas detention cap for beneficiary access (same as block env access: 20M).
const DETENTION_CAP: u64 = 20_000_000;

// ============================================================================
// HELPERS
// ============================================================================

/// Executes a transaction with the given spec, database, beneficiary, and limits.
fn transact_with_spec(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    beneficiary: Address,
    compute_gas_limit: u64,
    block_env_access_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64), EVMError<Infallible, MegaTransactionError>> {
    let block = BlockEnv { beneficiary, ..Default::default() };

    let mut context = MegaContext::new(db, spec).with_block(block).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_compute_gas_limit(compute_gas_limit)
            .with_block_env_access_compute_gas_limit(block_env_access_limit),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx)?;

    let detained_limit = evm.ctx_ref().additional_limit.borrow().detained_compute_gas_limit();
    Ok((r, detained_limit))
}

/// Shorthand: executes with REX4 spec.
fn transact(
    db: &mut MemoryDatabase,
    beneficiary: Address,
    compute_gas_limit: u64,
    block_env_access_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64), EVMError<Infallible, MegaTransactionError>> {
    transact_with_spec(
        MegaSpecId::REX4,
        db,
        beneficiary,
        compute_gas_limit,
        block_env_access_limit,
        tx,
    )
}

fn default_tx(to: Address) -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(to).gas_limit(1_000_000_000).build_fill()
}

/// The 4-byte selector for `remainingComputeGas()`.
const REMAINING_COMPUTE_GAS_SELECTOR: [u8; 4] =
    IMegaLimitControl::remainingComputeGasCall::SELECTOR;

/// Builds bytecode that CALLs `remainingComputeGas()` on `MegaLimitControl` and RETURNs the result.
fn query_remaining_compute_gas(builder: BytecodeBuilder) -> BytecodeBuilder {
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

/// Decodes `remainingComputeGas()` return data.
fn decode_remaining(result: &ResultAndState<MegaHaltReason>) -> u64 {
    let output = match &result.result {
        revm::context::result::ExecutionResult::Success { output, .. } => output.data().clone(),
        _ => panic!("expected success, got: {:?}", result.result),
    };
    IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(&output)
        .expect("should decode remainingComputeGas output")
}

/// Builds bytecode for a CALL to `target` with given gas.
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

/// Builds bytecode for a STATICCALL to `target`.
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

/// Builds bytecode for a DELEGATECALL to `target`.
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

/// Builds bytecode for a CALLCODE to `target`.
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

// ============================================================================
// TEST 1: CALL to beneficiary triggers detention
// ============================================================================

/// After a CALL to the beneficiary, `remainingComputeGas()` should be capped.
#[test]
fn test_call_to_beneficiary_triggers_detention() {
    // Callee: CALL beneficiary (empty account, immediate return), then query remaining gas.
    let beneficiary_code = BytecodeBuilder::default().stop().build();
    let callee_code = append_call(BytecodeBuilder::default(), BENEFICIARY, 10_000).append(POP);
    let callee_code = query_remaining_compute_gas(callee_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(BENEFICIARY, beneficiary_code);

    let tx = default_tx(CALLEE);
    let (result, detained_limit) =
        transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining <= DETENTION_CAP,
        "After CALL to beneficiary, remaining compute gas should be ≤ {DETENTION_CAP}, got {remaining}"
    );
    assert!(
        detained_limit < 200_000_000,
        "Detained limit should be lowered from TX limit, got {detained_limit}"
    );
}

// ============================================================================
// TEST 1b: CALL to beneficiary where child reverts — detention persists
// ============================================================================

/// The tracker is marked during CALL setup (before child execution), so even if
/// the child frame reverts, the detained limit must remain in effect for the parent.
#[test]
fn test_call_to_beneficiary_child_reverts_detention_persists() {
    // Beneficiary contract: immediately reverts.
    let beneficiary_code = BytecodeBuilder::default().append(REVERT).build();
    // Callee: CALL beneficiary (will revert), POP result, then query remaining gas.
    let callee_code = append_call(BytecodeBuilder::default(), BENEFICIARY, 10_000).append(POP);
    let callee_code = query_remaining_compute_gas(callee_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(BENEFICIARY, beneficiary_code);

    let tx = default_tx(CALLEE);
    let (result, detained_limit) =
        transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success(), "Parent should succeed despite child revert");
    assert!(
        remaining <= DETENTION_CAP,
        "After reverted CALL to beneficiary, detention should persist, remaining={remaining}"
    );
    assert!(
        detained_limit < 200_000_000,
        "Detained limit should be active after child revert, got {detained_limit}"
    );
}

// ============================================================================
// TEST 2: STATICCALL to beneficiary triggers detention
// ============================================================================

#[test]
fn test_staticcall_to_beneficiary_triggers_detention() {
    let beneficiary_code = BytecodeBuilder::default().stop().build();
    let callee_code =
        append_staticcall(BytecodeBuilder::default(), BENEFICIARY, 10_000).append(POP);
    let callee_code = query_remaining_compute_gas(callee_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(BENEFICIARY, beneficiary_code);

    let tx = default_tx(CALLEE);
    let (result, _) = transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining <= DETENTION_CAP,
        "After STATICCALL to beneficiary, remaining should be ≤ {DETENTION_CAP}, got {remaining}"
    );
}

// ============================================================================
// TEST 3: DELEGATECALL to beneficiary triggers detention
// ============================================================================

#[test]
fn test_delegatecall_to_beneficiary_triggers_detention() {
    let beneficiary_code = BytecodeBuilder::default().stop().build();
    let callee_code =
        append_delegatecall(BytecodeBuilder::default(), BENEFICIARY, 10_000).append(POP);
    let callee_code = query_remaining_compute_gas(callee_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(BENEFICIARY, beneficiary_code);

    let tx = default_tx(CALLEE);
    let (result, _) = transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining <= DETENTION_CAP,
        "After DELEGATECALL to beneficiary, remaining should be ≤ {DETENTION_CAP}, got {remaining}"
    );
}

// ============================================================================
// TEST 4: CALLCODE to beneficiary triggers detention
// ============================================================================

#[test]
fn test_callcode_to_beneficiary_triggers_detention() {
    let beneficiary_code = BytecodeBuilder::default().stop().build();
    let callee_code = append_callcode(BytecodeBuilder::default(), BENEFICIARY, 10_000).append(POP);
    let callee_code = query_remaining_compute_gas(callee_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(BENEFICIARY, beneficiary_code);

    let tx = default_tx(CALLEE);
    let (result, _) = transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining <= DETENTION_CAP,
        "After CALLCODE to beneficiary, remaining should be ≤ {DETENTION_CAP}, got {remaining}"
    );
}

// ============================================================================
// TEST 5: Caller is beneficiary — eager detention from TX start
// ============================================================================

/// When the TX sender is the beneficiary, detention should be active from the start.
/// No volatile opcode is needed; `on_new_tx` handles it eagerly.
#[test]
fn test_caller_is_beneficiary_eager_detention() {
    // Callee: query remaining compute gas immediately (no volatile opcodes).
    let callee_code = query_remaining_compute_gas(BytecodeBuilder::default()).build();

    let mut db = MemoryDatabase::default()
        .account_balance(BENEFICIARY, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    // TX sender = BENEFICIARY
    let tx = TxEnvBuilder::default()
        .caller(BENEFICIARY)
        .call(CALLEE)
        .gas_limit(1_000_000_000)
        .build_fill();

    let (result, detained_limit) =
        transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining <= DETENTION_CAP,
        "Caller=beneficiary: remaining should be ≤ {DETENTION_CAP} from start, got {remaining}"
    );
    assert!(
        detained_limit < 200_000_000,
        "Caller=beneficiary: detained limit should be active, got {detained_limit}"
    );
}

// ============================================================================
// TEST 5b: Caller is beneficiary — pre-REX4 no eager detention
// ============================================================================

/// Pre-REX4 specs never had eager beneficiary detention at TX start.
/// Same scenario as test 5 but with REX3: `detained_limit` should remain at TX limit.
#[test]
fn test_caller_is_beneficiary_pre_rex4_no_eager_detention() {
    // Simple code: just STOP. We check detained_limit, not remainingComputeGas
    // (MegaLimitControl interception is REX4-only).
    let callee_code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(BENEFICIARY, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = TxEnvBuilder::default()
        .caller(BENEFICIARY)
        .call(CALLEE)
        .gas_limit(1_000_000_000)
        .build_fill();

    let (result, detained_limit) =
        transact_with_spec(MegaSpecId::REX3, &mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx)
            .unwrap();

    assert!(result.result.is_success());
    assert_eq!(
        detained_limit, 200_000_000,
        "Pre-REX4 caller=beneficiary: detained limit should remain at TX limit (no eager detention)"
    );
}

// ============================================================================
// TEST 6: Recipient is beneficiary — eager detention from TX start
// ============================================================================

/// When the TX recipient is the beneficiary, detention should be active from the start.
#[test]
fn test_recipient_is_beneficiary_eager_detention() {
    // BENEFICIARY itself is the callee. Its code queries remaining compute gas.
    let beneficiary_code = query_remaining_compute_gas(BytecodeBuilder::default()).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(BENEFICIARY, beneficiary_code);

    // TX recipient = BENEFICIARY
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(1_000_000_000)
        .build_fill();

    let (result, detained_limit) =
        transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining <= DETENTION_CAP,
        "Recipient=beneficiary: remaining should be ≤ {DETENTION_CAP} from start, got {remaining}"
    );
    assert!(
        detained_limit < 200_000_000,
        "Recipient=beneficiary: detained limit should be active, got {detained_limit}"
    );
}

// ============================================================================
// TEST 6b: Recipient is beneficiary — pre-REX4 no eager detention
// ============================================================================

/// Pre-REX4 specs never had eager beneficiary detention at TX start.
/// Same scenario as test 6 but with REX3: `detained_limit` should remain at TX limit.
#[test]
fn test_recipient_is_beneficiary_pre_rex4_no_eager_detention() {
    let beneficiary_code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(BENEFICIARY, beneficiary_code);

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(1_000_000_000)
        .build_fill();

    let (result, detained_limit) =
        transact_with_spec(MegaSpecId::REX3, &mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx)
            .unwrap();

    assert!(result.result.is_success());
    assert_eq!(
        detained_limit, 200_000_000,
        "Pre-REX4 recipient=beneficiary: detained limit should remain at TX limit (no eager detention)"
    );
}

// ============================================================================
// TEST 7: SELFBALANCE in beneficiary contract triggers detention (integration)
// ============================================================================

/// Integration test: CALL to beneficiary whose code executes SELFBALANCE.
///
/// NOTE: The outer CALL to the beneficiary already triggers detention through
/// `wrap_call_volatile_check`, so this test does not isolate the SELFBALANCE
/// wrapper. It verifies that the combination works correctly.
/// See `test_selfbalance_in_non_beneficiary_no_detention` for address sensitivity.
#[test]
fn test_selfbalance_in_beneficiary_triggers_detention() {
    // Beneficiary contract: SELFBALANCE, POP, then query remaining compute gas.
    let beneficiary_code = BytecodeBuilder::default().append(SELFBALANCE).append(POP);
    let beneficiary_code = query_remaining_compute_gas(beneficiary_code).build();

    // Callee: CALL beneficiary, capture return data, RETURN it.
    let callee_code = BytecodeBuilder::default()
        .push_number(32_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(BENEFICIARY)
        .push_number(500_000_u64) // gas
        .append(CALL)
        .append(POP)
        // Copy return data
        .append(RETURNDATASIZE) // size
        .push_number(0_u64) // srcOffset
        .push_number(0_u64) // destOffset
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .push_number(0_u64)
        .append(RETURN);
    let callee_code = callee_code.build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(BENEFICIARY, beneficiary_code);

    let tx = default_tx(CALLEE);
    let (result, detained_limit) =
        transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining <= DETENTION_CAP,
        "SELFBALANCE in beneficiary: remaining should be ≤ {DETENTION_CAP}, got {remaining}"
    );
    assert!(
        detained_limit < 200_000_000,
        "SELFBALANCE in beneficiary: detained limit should be active, got {detained_limit}"
    );
}

// ============================================================================
// TEST 8: CALL to non-beneficiary does NOT trigger detention
// ============================================================================

/// Sanity check: CALL to a non-beneficiary address should not trigger gas detention.
#[test]
fn test_call_to_non_beneficiary_no_detention() {
    let target = address!("0000000000000000000000000000000000400002");
    let target_code = BytecodeBuilder::default().stop().build();
    let callee_code = append_call(BytecodeBuilder::default(), target, 10_000).append(POP);
    let callee_code = query_remaining_compute_gas(callee_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(target, target_code);

    let tx = default_tx(CALLEE);
    let (result, detained_limit) =
        transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining > DETENTION_CAP,
        "CALL to non-beneficiary should NOT trigger detention, remaining={remaining}"
    );
    assert_eq!(
        detained_limit, 200_000_000,
        "Detained limit should remain at TX limit when no volatile access"
    );
}

// ============================================================================
// TEST 9: SELFBALANCE in non-beneficiary contract does NOT trigger detention
// ============================================================================

/// Proves that the SELFBALANCE volatile wrapper is address-sensitive:
/// SELFBALANCE at a non-beneficiary address must not trigger gas detention.
/// Combined with test 7 (which shows detention IS triggered when the beneficiary
/// executes SELFBALANCE), this pair confirms the wrapper's address check.
#[test]
fn test_selfbalance_in_non_beneficiary_no_detention() {
    // CALLEE (not the beneficiary) executes SELFBALANCE, then queries remaining gas.
    let callee_code = BytecodeBuilder::default().append(SELFBALANCE).append(POP);
    let callee_code = query_remaining_compute_gas(callee_code).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = default_tx(CALLEE);
    let (result, detained_limit) =
        transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();
    let remaining = decode_remaining(&result);

    assert!(result.result.is_success());
    assert!(
        remaining > DETENTION_CAP,
        "SELFBALANCE in non-beneficiary should NOT trigger detention, remaining={remaining}"
    );
    assert_eq!(
        detained_limit, 200_000_000,
        "Detained limit should remain at TX limit when SELFBALANCE is at non-beneficiary"
    );
}

// ============================================================================
// TEST 10: disableVolatileDataAccess + SELFBALANCE at beneficiary reverts
// ============================================================================

/// When volatile data access is disabled, SELFBALANCE at the beneficiary address
/// must revert. This tests the revert branch of `volatile_data_ext::selfbalance`.
///
/// Approach: TX sends to BENEFICIARY directly (so code runs at beneficiary address).
/// The beneficiary code disables volatile access, then executes SELFBALANCE.
/// SELFBALANCE should revert the frame because the wrapper detects target == beneficiary
/// with volatile access disabled.
#[test]
fn test_selfbalance_at_beneficiary_reverts_when_volatile_disabled() {
    let disable_selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

    // Beneficiary code:
    // 1. disableVolatileDataAccess()
    // 2. SELFBALANCE (should revert because volatile access is disabled)
    // 3. STOP (unreachable if SELFBALANCE reverts the frame)
    let beneficiary_code = BytecodeBuilder::default()
        .mstore(0x0, disable_selector)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64)
        .append(CALL)
        .append(POP)
        // SELFBALANCE — should revert the frame
        .append(SELFBALANCE)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(BENEFICIARY, beneficiary_code);

    // TX directly to BENEFICIARY so code executes at beneficiary address.
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(1_000_000_000)
        .build_fill();

    let (result, _) = transact(&mut db, BENEFICIARY, 200_000_000, DETENTION_CAP, tx).unwrap();

    // The frame should revert because SELFBALANCE at beneficiary is blocked.
    assert!(
        matches!(result.result, revm::context::result::ExecutionResult::Revert { .. }),
        "SELFBALANCE at beneficiary with volatile access disabled should revert, got {:?}",
        result.result
    );
}

// ============================================================================
// TEST 11: Detention + intrinsic DataSize overflow interaction
// ============================================================================

/// Cross-concern test: when both gas detention (via beneficiary sender) and
/// intrinsic `DataSize` overflow are active, the TX must still fail with the
/// correct halt reason (`DataLimitExceeded`), not succeed or produce a
/// gas rescue that incorrectly reflects the detained cap.
#[test]
fn test_detention_plus_intrinsic_data_size_overflow() {
    // Set data size limit to something too small for even intrinsic data.
    // Also configure beneficiary detention by making CALLER == BENEFICIARY.
    let data_limit = 100_u64; // Less than BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE (~150)

    let callee_code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(BENEFICIARY, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    // TX sender = BENEFICIARY → triggers eager detention in on_new_tx.
    let tx = TxEnvBuilder::default()
        .caller(BENEFICIARY)
        .call(CALLEE)
        .gas_limit(1_000_000_000)
        .build_fill();

    let block = revm::context::BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };

    let mut context =
        MegaContext::new(&mut db, MegaSpecId::REX4).with_block(block).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_compute_gas_limit(200_000_000)
                .with_block_env_access_compute_gas_limit(DETENTION_CAP)
                .with_tx_data_size_limit(data_limit),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    // Must halt with DataLimitExceeded despite detention being active.
    assert!(
        result.result.is_halt(),
        "Detention + intrinsic DataSize overflow should halt, got {:?}",
        result.result
    );
    assert!(
        matches!(
            result.result,
            revm::context::result::ExecutionResult::Halt {
                reason: MegaHaltReason::DataLimitExceeded { .. },
                ..
            }
        ),
        "Should halt with DataLimitExceeded, got {:?}",
        result.result
    );

    // Gas rescue should have returned most gas since no execution happened.
    let gas_remaining = 1_000_000_000 - result.result.gas_used();
    assert!(
        gas_remaining > 900_000_000,
        "Expected >900M gas remaining from rescue (not inflated by detention), got {gas_remaining}"
    );
}

// ============================================================================
// TEST 12: Detention + execution data limit — detained compute gas does not
//          interfere with data size enforcement
// ============================================================================

/// When detention caps compute gas, data size enforcement must still function
/// independently. This tests that a TX under the detained compute gas cap
/// can still fail on data size limits.
#[test]
fn test_detention_does_not_interfere_with_data_size_limit() {
    // Set up: beneficiary detention active, compute gas limit generous,
    // but data size limit tight. Callee CALLs beneficiary to trigger detention,
    // then writes SSTOREs that exceed data limit.
    let data_limit = 200_u64; // ~intrinsic(150) + 1 SSTORE(40) = 190, fits 1, not 2.

    // Callee: CALL beneficiary (triggers detention), then do SSTOREs that exceed data limit.
    let callee_code = BytecodeBuilder::default()
        // CALL beneficiary to trigger detention
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(BENEFICIARY)
        .push_number(10_000_u64)
        .append(CALL)
        .append(POP)
        // Now do SSTOREs that exceed data limit
        .sstore(U256::from(0), U256::from(1))
        .sstore(U256::from(1), U256::from(2))
        .stop()
        .build();

    let beneficiary_code = BytecodeBuilder::default().stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(BENEFICIARY, beneficiary_code);

    let tx = default_tx(CALLEE);

    let block = revm::context::BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };

    let mut context =
        MegaContext::new(&mut db, MegaSpecId::REX4).with_block(block).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_compute_gas_limit(200_000_000)
                .with_block_env_access_compute_gas_limit(DETENTION_CAP)
                .with_tx_data_size_limit(data_limit),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    // Detained compute gas cap is active, but the TX should fail on data size.
    assert!(
        !result.result.is_success(),
        "Detention active but data size exceeded should not succeed, got {:?}",
        result.result
    );
}
