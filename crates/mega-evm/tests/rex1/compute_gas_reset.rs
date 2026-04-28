//! Tests for the Rex1 fix: detained compute gas limit resets between transactions.
//!
//! Before Rex1, volatile data access (oracle call, TIMESTAMP, etc.) in TX1 would lower the
//! detained compute gas limit, and that lowered limit persisted to subsequent transactions,
//! causing unrelated transactions to fail with `ComputeGasLimitExceeded`.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALL, GAS, KECCAK256, MSTORE, POP, PUSH0, STOP, TIMESTAMP},
    context::tx::TxEnvBuilder,
    handler::EvmTr,
    inspector::NoOpInspector,
};

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CONTRACT_VOLATILE: alloy_primitives::Address =
    address!("1000000000000000000000000000000000000001");
const CONTRACT_EXPENSIVE: alloy_primitives::Address =
    address!("1000000000000000000000000000000000000002");

// TX compute gas limits. Block-env limit must exceed BLOCK_ENV_DETENTION_CAP so the cap binds.
const TX_COMPUTE_GAS_LIMIT_ORACLE: u64 = 10_000_000;
const TX_COMPUTE_GAS_LIMIT_BLOCK_ENV: u64 = 30_000_000;

const ORACLE_DETENTION_CAP: u64 = 1_000_000; // pre-Rex3 oracle cap
const BLOCK_ENV_DETENTION_CAP: u64 = 20_000_000;

/// Build a contract that calls the oracle contract, triggering oracle detention (1M cap).
fn build_oracle_call_contract() -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// Build a contract that reads TIMESTAMP, triggering block-env detention (20M cap).
fn build_timestamp_access_contract() -> Bytes {
    BytecodeBuilder::default().append(TIMESTAMP).append(POP).append(STOP).build()
}

/// Build a contract that burns compute gas via KECCAK256 loops (~38 gas/iter).
/// 55_000 iters ≈ 2.1M gas; 600_000 iters ≈ 22.8M gas.
fn build_expensive_contract(iterations: u32) -> Bytes {
    let mut builder =
        BytecodeBuilder::default().push_number(0xdeadbeefu32).push_number(0u8).append(MSTORE);
    for _ in 0..iterations {
        builder = builder.push_number(32u8).push_number(0u8).append(KECCAK256).append(POP);
    }
    builder.append(STOP).build()
}

fn make_evm(
    spec: MegaSpecId,
    db: MemoryDatabase,
    tx_compute_gas_limit: u64,
    oracle_cap: u64,
    block_env_cap: u64,
) -> MegaEvm<MemoryDatabase, NoOpInspector, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_compute_gas_limit(tx_compute_gas_limit)
            .with_oracle_access_compute_gas_limit(oracle_cap)
            .with_block_env_access_compute_gas_limit(block_env_cap),
    );
    context.modify_chain(|c| {
        c.operator_fee_scalar = Some(U256::ZERO);
        c.operator_fee_constant = Some(U256::ZERO);
    });
    context.modify_cfg(|c| {
        c.disable_nonce_check = true;
    });
    MegaEvm::new(context).with_inspector(NoOpInspector)
}

fn make_tx(target: alloy_primitives::Address) -> MegaTransaction {
    MegaTransaction {
        base: TxEnvBuilder::new()
            .caller(CALLER)
            .call(target)
            .gas_limit(1_000_000_000_000)
            .gas_price(0)
            .build_fill(),
        enveloped_tx: Some(Bytes::new()),
        ..Default::default()
    }
}

fn compute_gas_limit(evm: &MegaEvm<MemoryDatabase, NoOpInspector, EmptyExternalEnv>) -> u64 {
    evm.ctx_ref().additional_limit.borrow().compute_gas_limit()
}

// =============================================================================
// Oracle detention tests
// =============================================================================

/// Rex1: TX2 succeeds after TX1 oracle detention — detained limit resets at TX start.
#[test]
fn test_rex1_oracle_detention_resets_between_txs() {
    // 55_000 iters ≈ 2.1M gas: above 1M oracle cap, below 10M TX limit
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT_VOLATILE, build_oracle_call_contract())
        .account_code(CONTRACT_EXPENSIVE, build_expensive_contract(55_000));

    let mut evm =
        make_evm(MegaSpecId::REX1, db, TX_COMPUTE_GAS_LIMIT_ORACLE, ORACLE_DETENTION_CAP, u64::MAX);

    let r1 = alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_VOLATILE)).unwrap();
    assert!(r1.result.is_success(), "TX1 should succeed, got: {:?}", r1.result);
    assert_eq!(compute_gas_limit(&evm), ORACLE_DETENTION_CAP, "TX1 should detain limit to 1M");

    let r2 = alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_EXPENSIVE)).unwrap();
    assert!(r2.result.is_success(), "TX2 should succeed after limit resets, got: {:?}", r2.result);
    assert_eq!(
        compute_gas_limit(&evm),
        TX_COMPUTE_GAS_LIMIT_ORACLE,
        "limit should reset to base TX limit after TX2"
    );
}

/// Pre-Rex1 (REX): detained limit from TX1 oracle access leaks into TX2, causing it to fail.
#[test]
fn test_pre_rex1_oracle_detention_leaks_to_next_tx() {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT_VOLATILE, build_oracle_call_contract())
        .account_code(CONTRACT_EXPENSIVE, build_expensive_contract(55_000));

    let mut evm =
        make_evm(MegaSpecId::REX, db, TX_COMPUTE_GAS_LIMIT_ORACLE, ORACLE_DETENTION_CAP, u64::MAX);

    let r1 = alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_VOLATILE)).unwrap();
    assert!(r1.result.is_success(), "TX1 should succeed, got: {:?}", r1.result);
    assert_eq!(compute_gas_limit(&evm), ORACLE_DETENTION_CAP, "TX1 should detain limit to 1M");

    let r2 = alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_EXPENSIVE)).unwrap();
    assert!(!r2.result.is_success(), "TX2 should fail: detained limit not reset in pre-Rex1");
    assert!(
        matches!(
            r2.result,
            revm::context::result::ExecutionResult::Halt {
                reason: MegaHaltReason::ComputeGasLimitExceeded { .. },
                ..
            }
        ),
        "expected ComputeGasLimitExceeded, got: {:?}",
        r2.result
    );
    assert_eq!(
        compute_gas_limit(&evm),
        ORACLE_DETENTION_CAP,
        "limit should still be stuck at oracle cap in pre-Rex1"
    );
}

// =============================================================================
// Block-env detention tests (TIMESTAMP opcode)
// =============================================================================

/// Rex1: TX2 succeeds after TX1 block-env detention — detained limit resets at TX start.
#[test]
fn test_rex1_block_env_detention_resets_between_txs() {
    // 600_000 iters ≈ 22.8M gas: above 20M block-env cap, below 30M TX limit
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT_VOLATILE, build_timestamp_access_contract())
        .account_code(CONTRACT_EXPENSIVE, build_expensive_contract(600_000));

    let mut evm = make_evm(
        MegaSpecId::REX1,
        db,
        TX_COMPUTE_GAS_LIMIT_BLOCK_ENV,
        u64::MAX, // oracle cap — not relevant here
        BLOCK_ENV_DETENTION_CAP,
    );

    let r1 = alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_VOLATILE)).unwrap();
    assert!(r1.result.is_success(), "TX1 should succeed, got: {:?}", r1.result);
    assert_eq!(
        compute_gas_limit(&evm),
        BLOCK_ENV_DETENTION_CAP,
        "TX1 should detain limit to 20M"
    );

    let r2 = alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_EXPENSIVE)).unwrap();
    assert!(r2.result.is_success(), "TX2 should succeed after limit resets, got: {:?}", r2.result);
    assert_eq!(
        compute_gas_limit(&evm),
        TX_COMPUTE_GAS_LIMIT_BLOCK_ENV,
        "limit should reset to base TX limit after TX2"
    );
}

/// Pre-Rex1 (REX): detained limit from TX1 block-env access leaks into TX2, causing it to fail.
#[test]
fn test_pre_rex1_block_env_detention_leaks_to_next_tx() {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT_VOLATILE, build_timestamp_access_contract())
        .account_code(CONTRACT_EXPENSIVE, build_expensive_contract(600_000));

    let mut evm = make_evm(
        MegaSpecId::REX,
        db,
        TX_COMPUTE_GAS_LIMIT_BLOCK_ENV,
        u64::MAX,
        BLOCK_ENV_DETENTION_CAP,
    );

    let r1 = alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_VOLATILE)).unwrap();
    assert!(r1.result.is_success(), "TX1 should succeed, got: {:?}", r1.result);
    assert_eq!(
        compute_gas_limit(&evm),
        BLOCK_ENV_DETENTION_CAP,
        "TX1 should detain limit to 20M"
    );

    let r2 = alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_EXPENSIVE)).unwrap();
    assert!(!r2.result.is_success(), "TX2 should fail: detained limit not reset in pre-Rex1");
    assert!(
        matches!(
            r2.result,
            revm::context::result::ExecutionResult::Halt {
                reason: MegaHaltReason::ComputeGasLimitExceeded { .. },
                ..
            }
        ),
        "expected ComputeGasLimitExceeded, got: {:?}",
        r2.result
    );
}

// =============================================================================
// Multi-transaction reset test
// =============================================================================

/// Rex1: detention resets on every transaction boundary, not just once.
///
/// Runs two rounds of (volatile TX → expensive TX) to verify the reset is persistent.
#[test]
fn test_rex1_detention_resets_across_multiple_txs() {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT_VOLATILE, build_oracle_call_contract())
        .account_code(CONTRACT_EXPENSIVE, build_expensive_contract(55_000));

    let mut evm =
        make_evm(MegaSpecId::REX1, db, TX_COMPUTE_GAS_LIMIT_ORACLE, ORACLE_DETENTION_CAP, u64::MAX);

    for round in 1..=2 {
        let r_volatile =
            alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_VOLATILE)).unwrap();
        assert!(r_volatile.result.is_success(), "round {round} volatile TX should succeed");
        assert_eq!(
            compute_gas_limit(&evm),
            ORACLE_DETENTION_CAP,
            "round {round}: limit should be detained after volatile TX"
        );

        let r_expensive =
            alloy_evm::Evm::transact_raw(&mut evm, make_tx(CONTRACT_EXPENSIVE)).unwrap();
        assert!(r_expensive.result.is_success(), "round {round} expensive TX should succeed");
        assert_eq!(
            compute_gas_limit(&evm),
            TX_COMPUTE_GAS_LIMIT_ORACLE,
            "round {round}: limit should reset after expensive TX"
        );
    }
}
