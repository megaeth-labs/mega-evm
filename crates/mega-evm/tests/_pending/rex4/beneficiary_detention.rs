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

// ============================================================================
// TEST 1b: CALL to beneficiary where child reverts — detention persists
// ============================================================================

// ============================================================================
// TEST 2: STATICCALL to beneficiary triggers detention
// ============================================================================

// ============================================================================
// TEST 3: DELEGATECALL to beneficiary triggers detention
// ============================================================================

// ============================================================================
// TEST 4: CALLCODE to beneficiary triggers detention
// ============================================================================

// ============================================================================
// TEST 5: Caller is beneficiary — eager detention from TX start
// ============================================================================

// ============================================================================
// TEST 5b: Caller is beneficiary — pre-REX4 no eager detention
// ============================================================================

// ============================================================================
// TEST 6: Recipient is beneficiary — eager detention from TX start
// ============================================================================

// ============================================================================
// TEST 6b: Recipient is beneficiary — pre-REX4 no eager detention
// ============================================================================

// ============================================================================
// TEST 7: SELFBALANCE in beneficiary contract triggers detention (integration)
// ============================================================================

// ============================================================================
// TEST 8: CALL to non-beneficiary does NOT trigger detention
// ============================================================================

// ============================================================================
// TEST 9: SELFBALANCE in non-beneficiary contract does NOT trigger detention
// ============================================================================

// ============================================================================
// TEST 10: disableVolatileDataAccess + SELFBALANCE at beneficiary reverts
// ============================================================================

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
