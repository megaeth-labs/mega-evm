//! Tests for the compute gas limit feature of the `MegaETH` EVM.
//!
//! Tests the compute gas limit functionality that tracks computational work
//! separately from storage and data costs.

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
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    precompile::{
        bn128::pair,
        hash::{RIPEMD160, SHA256},
        secp256k1::ECRECOVER,
    },
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
// TRANSACTION RESET TESTS
// ============================================================================

/// Test that compute gas limit is reset between transactions after volatile data access in Rex1.
///
/// Starting from Rex1, the compute gas limit is reset between transactions. This verifies the
/// fix for the bug where `set_compute_gas_limit()` would lower the limit when volatile data
/// (like oracle) was accessed, but the lowered limit would persist incorrectly to subsequent
/// transactions on the SAME EVM instance.
///
/// The test uses a contract that consumes >1M compute gas for TX2. Without the fix (i.e.,
/// pre-Rex1), TX2 would fail with `ComputeGasLimitExceeded` because the limit would be stuck at 1M
/// from the oracle access in TX1.
#[test]
fn test_compute_gas_limit_resets_after_volatile_access_rex1() {
    use mega_evm::{
        constants::mini_rex::ORACLE_ACCESS_COMPUTE_GAS, MegaTransaction, ORACLE_CONTRACT_ADDRESS,
    };
    use revm::ExecuteEvm;

    // Contract 1: Calls the oracle, which lowers compute_gas_limit to 1M
    let oracle_caller = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();

    // Contract 2: Expensive contract that uses >1M compute gas via SHA3 operations.
    // Each SHA3 with 32 bytes costs 36 gas + 6 per word = 42 gas. But the memory expansion
    // and iterations add up. We use a loop of 30000 SHA3 operations to exceed 1M compute gas.
    // Without the fix, this would fail because the limit would be stuck at 1M.
    let mut expensive_builder = BytecodeBuilder::default();
    // Store a value in memory first
    expensive_builder =
        expensive_builder.push_number(0xdeadbeefu32).push_number(0u8).append(MSTORE);
    // Do many SHA3 operations on the same memory region
    for _ in 0..30000 {
        expensive_builder = expensive_builder
            .push_number(32u8) // size
            .push_number(0u8) // offset
            .append(KECCAK256)
            .append(POP); // discard result
    }
    let expensive_contract = expensive_builder.append(STOP).build();

    let compute_gas_limit: u64 = 10_000_000; // 10M

    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT, oracle_caller)
        .account_code(CONTRACT2, expensive_contract);

    // Create a SINGLE EVM instance that will be used for both transactions
    // Use REX1 spec where limits are reset between transactions
    let mut context = MegaContext::new(db, MegaSpecId::REX1).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_compute_gas_limit(compute_gas_limit)
            .with_oracle_access_compute_gas_limit(ORACLE_ACCESS_COMPUTE_GAS),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);

    // TX1: Call oracle contract - this lowers compute_gas_limit to 1M
    let tx1 = MegaTransaction {
        base: TxEnvBuilder::new().caller(CALLER).call(CONTRACT).build_fill(),
        enveloped_tx: Some(Bytes::new()),
        ..Default::default()
    };
    let result1 = alloy_evm::Evm::transact_raw(&mut evm, tx1).unwrap();
    assert!(result1.result.is_success(), "TX1 should succeed");

    // Verify TX1 lowered the compute_gas_limit to oracle limit
    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit(),
        ORACLE_ACCESS_COMPUTE_GAS,
        "TX1 should have lowered compute_gas_limit to oracle access limit"
    );

    // TX2: Expensive contract that uses >1M compute gas on the SAME EVM instance.
    // If the limit wasn't reset, this would fail with ComputeGasLimitExceeded.
    let tx2 = MegaTransaction {
        base: TxEnvBuilder::new().caller(CALLER).call(CONTRACT2).build_fill(),
        enveloped_tx: Some(Bytes::new()),
        ..Default::default()
    };
    let result2 = evm.transact_one(tx2).unwrap();

    // Get the compute gas used by TX2
    let compute_gas_used = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;

    // Verify TX2 used more than the oracle limit (1M)
    assert!(
        compute_gas_used > ORACLE_ACCESS_COMPUTE_GAS,
        "TX2 should use more compute gas than oracle limit: {} > {}",
        compute_gas_used,
        ORACLE_ACCESS_COMPUTE_GAS
    );

    // TX2 should succeed because the limit was reset to 10M (Rex1 behavior)
    assert!(
        result2.is_success(),
        "TX2 should succeed because compute_gas_limit was reset to {}. Used {} gas. \
         Without Rex1, it would fail because the limit would be stuck at {}",
        compute_gas_limit,
        compute_gas_used,
        ORACLE_ACCESS_COMPUTE_GAS
    );

    // Verify the limit is at the original value (not stuck at oracle limit)
    let actual_limit = evm.ctx_ref().additional_limit.borrow().compute_gas_limit();
    assert_eq!(
        actual_limit, compute_gas_limit,
        "compute_gas_limit should be reset to original value ({}), not stuck at oracle limit ({})",
        compute_gas_limit, ORACLE_ACCESS_COMPUTE_GAS
    );
}

// ============================================================================
// EDGE CASE TESTS
// ============================================================================

#[test]
fn test_volatile_data_access_with_non_restrictive_detention_reports_compute_gas_limit() {
    // When volatile data is accessed but the detention limit is NOT more restrictive than the
    // per-tx compute gas limit, exceeding the per-tx compute gas limit should report
    // ComputeGasLimitExceeded, NOT VolatileDataAccessOutOfGas.
    //
    // The `transact` helper uses `EvmTxRuntimeLimits::no_limits()` which sets volatile access
    // limits to u64::MAX, so detention is never restrictive.

    // Contract that accesses TIMESTAMP (volatile data) then does expensive work
    let mut builder = BytecodeBuilder::default()
        .append(TIMESTAMP) // Access volatile data
        .append(POP);
    // Do enough SSTOREs to exceed the compute gas limit
    // Each SSTORE (zero -> non-zero) costs ~22,100 compute gas
    // 1000 SSTOREs x 22,100 = 22.1M compute gas
    for i in 1..=1000u32 {
        builder = builder.push_number(i).push_number(i).append(SSTORE);
    }
    let bytecode = builder.append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT, bytecode);

    let tx =
        TxEnvBuilder::new().caller(CALLER).call(CONTRACT).gas_limit(1_000_000_000_000).build_fill();

    // Set compute gas limit to 20M (will be exceeded by 22.1M of SSTOREs)
    let compute_gas_limit = 19_000_000;
    let (result, _) = transact(MegaSpecId::MINI_REX, &mut db, compute_gas_limit, tx).unwrap();

    // Should halt with ComputeGasLimitExceeded, NOT VolatileDataAccessOutOfGas
    assert!(
        is_compute_gas_limit_exceeded(&result),
        "Expected ComputeGasLimitExceeded when detention is not restrictive, got {:?}",
        result.result
    );

    let (limit, actual) = get_compute_gas_limit_info(&result).unwrap();
    assert_eq!(limit, compute_gas_limit);
    assert!(actual > limit);
}
