//! Tests for Rex4 relative gas detention cap.
//!
//! Rex4 changes gas detention (volatile data compute gas limiting) from an **absolute** cap to a
//! **relative** cap. The effective limit becomes `current_usage + cap` at the point of access,
//! rather than just `cap`.
//!
//! This means a transaction that has already used >20M compute gas before accessing TIMESTAMP
//! will not halt immediately — instead it gets 20M more compute gas after the access.
//!
//! Key properties tested:
//! 1. REX4: Volatile access after heavy compute succeeds (relative cap allows more gas)
//! 2. REX4: Post-access cap is still enforced (can't use unlimited gas after access)
//! 3. REX4: Pre-access usage is not counted against the cap
//! 4. REX4: Multiple volatile accesses — first access anchors the cap
//! 5. Pre-REX4: Absolute cap behavior is preserved (backward compatibility)

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
    handler::EvmTr,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000300000");
const CALLEE: Address = address!("0000000000000000000000000000000000300001");

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// The gas detention cap for block env access (20M).
const BLOCK_ENV_CAP: u64 = 20_000_000;

/// Appends approximately `target_gas` worth of compute gas burn via repeated PUSH1/POP sequences.
/// Each PUSH1+POP pair costs 3+2=5 gas.
fn append_burn_gas(mut builder: BytecodeBuilder, target_gas: u64) -> BytecodeBuilder {
    let iterations = target_gas / 5;
    for _ in 0..iterations {
        builder = builder.push_number(0_u8).append(POP);
    }
    builder
}

/// Executes a transaction with the given spec and compute gas / detention limits.
fn transact_with_spec(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    compute_gas_limit: u64,
    block_env_access_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, MegaTransactionError>>
{
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
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

    let ctx = evm.ctx_ref();
    let additional_limit = ctx.additional_limit.borrow();
    let compute_gas = additional_limit.get_usage().compute_gas;
    let detained_limit = additional_limit.detained_compute_gas_limit();
    Ok((r, compute_gas, detained_limit))
}

fn default_tx() -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(CALLEE).gas_limit(1_000_000_000).build_fill()
}

/// Checks if the result is a volatile data access out of gas error.
fn is_volatile_data_access_oog(result: &ExecutionResult<MegaHaltReason>) -> bool {
    matches!(
        result,
        &ExecutionResult::Halt { reason: MegaHaltReason::VolatileDataAccessOutOfGas { .. }, .. }
    )
}

// ============================================================================
// TESTS
// ============================================================================

/// REX4: Transaction uses >20M compute gas before accessing TIMESTAMP, then does minimal work.
/// With relative cap, the effective limit becomes `usage_at_access` + 20M, so the TX succeeds.
/// Pre-REX4 would halt immediately because absolute 20M cap < actual usage.
#[test]
fn test_rex4_volatile_access_after_heavy_compute_succeeds() {
    // Burn 25M compute gas, then access TIMESTAMP, then STOP.
    // With relative cap: effective limit = 25M + 20M = 45M. Actual usage ~25M. Succeeds.
    let builder = append_burn_gas(BytecodeBuilder::default(), 25_000_000);
    let code = builder.append(TIMESTAMP).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx();
    let (result, compute_gas, detained_limit) =
        transact_with_spec(MegaSpecId::REX4, &mut db, 200_000_000, BLOCK_ENV_CAP, tx).unwrap();

    assert!(result.result.is_success(), "Should succeed with relative cap: {compute_gas} used");
    // Detained limit should be anchored at ~25M + 20M = ~45M
    assert!(
        detained_limit > BLOCK_ENV_CAP,
        "Detained limit should be > absolute 20M cap, got {detained_limit}"
    );
}

/// REX4: Transaction accesses TIMESTAMP early, then tries to use >20M compute gas after.
/// The post-access cap (20M) should still be enforced.
#[test]
fn test_rex4_volatile_access_post_access_cap_enforced() {
    // Access TIMESTAMP first (low usage), then try to burn 25M compute gas.
    // Effective limit = ~0 + 20M = ~20M. Trying to burn 25M after -> halts.
    let builder = BytecodeBuilder::default().append(TIMESTAMP).append(POP);
    let builder = append_burn_gas(builder, 25_000_000);
    let code = builder.stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx();
    let (result, _compute_gas, _detained_limit) =
        transact_with_spec(MegaSpecId::REX4, &mut db, 200_000_000, BLOCK_ENV_CAP, tx).unwrap();

    assert!(
        is_volatile_data_access_oog(&result.result),
        "Should halt with VolatileDataAccessOutOfGas when exceeding post-access cap"
    );
}

/// REX4: Transaction uses X compute gas before TIMESTAMP, then uses <20M after.
/// Total usage is X + <20M which exceeds the old absolute 20M cap, but should succeed.
#[test]
fn test_rex4_pre_access_usage_not_counted_against_cap() {
    // Burn 15M compute gas, then access TIMESTAMP, then burn 15M more.
    // Total = ~30M > absolute 20M cap. But relative cap = 15M + 20M = 35M. Actual ~30M. Succeeds.
    let builder = append_burn_gas(BytecodeBuilder::default(), 15_000_000);
    let builder = builder.append(TIMESTAMP).append(POP);
    let builder = append_burn_gas(builder, 15_000_000);
    let code = builder.stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx();
    let (result, compute_gas, detained_limit) =
        transact_with_spec(MegaSpecId::REX4, &mut db, 200_000_000, BLOCK_ENV_CAP, tx).unwrap();

    assert!(
        result.result.is_success(),
        "Should succeed: pre-access usage not counted against cap. \
         Compute gas: {compute_gas}, detained limit: {detained_limit}"
    );
    assert!(
        compute_gas > BLOCK_ENV_CAP,
        "Total compute gas ({compute_gas}) should exceed absolute 20M cap"
    );
}

/// REX4: Multiple volatile accesses — first access anchors the cap.
/// TX accesses TIMESTAMP at usage=5M, then NUMBER at usage=10M.
/// Cap should be anchored at first access: 5M + 20M = 25M.
#[test]
fn test_rex4_multiple_volatile_accesses_first_wins() {
    // Burn 5M, access TIMESTAMP, burn 5M more, access NUMBER, then STOP.
    let builder = append_burn_gas(BytecodeBuilder::default(), 5_000_000);
    let builder = builder.append(TIMESTAMP).append(POP);
    let builder = append_burn_gas(builder, 5_000_000);
    let code = builder.append(NUMBER).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx();
    let (result, _compute_gas, detained_limit) =
        transact_with_spec(MegaSpecId::REX4, &mut db, 200_000_000, BLOCK_ENV_CAP, tx).unwrap();

    assert!(result.result.is_success(), "Should succeed with both accesses within cap");
    // First access at ~5M: limit = 5M + 20M = 25M
    // Second access at ~10M: limit = min(25M, 10M + 20M) = min(25M, 30M) = 25M
    // First access wins.
    assert!(
        detained_limit < 30_000_000,
        "Detained limit should be anchored at first access (~25M), got {detained_limit}"
    );
}

/// Pre-REX4: Same scenario as `test_rex4_volatile_access_after_heavy_compute_succeeds`
/// but with `MINI_REX` spec — should halt immediately (backward compatibility).
#[test]
fn test_pre_rex4_absolute_cap_preserved() {
    // Burn 25M compute gas, then access TIMESTAMP.
    // Pre-REX4 absolute cap = 20M. 25M > 20M -> halts immediately.
    let builder = append_burn_gas(BytecodeBuilder::default(), 25_000_000);
    let code = builder.append(TIMESTAMP).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, code);

    let tx = default_tx();
    let (result, _compute_gas, detained_limit) =
        transact_with_spec(MegaSpecId::MINI_REX, &mut db, 1_000_000_000, BLOCK_ENV_CAP, tx)
            .unwrap();

    assert!(
        is_volatile_data_access_oog(&result.result),
        "Pre-REX4 should halt with absolute cap when usage exceeds 20M before access"
    );
    assert_eq!(
        detained_limit, BLOCK_ENV_CAP,
        "Pre-REX4 detained limit should be the absolute cap (20M)"
    );
}
