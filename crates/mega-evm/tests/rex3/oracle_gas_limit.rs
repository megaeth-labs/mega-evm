//! Tests for the Rex3 oracle access compute gas limit increase (1M -> 10M).

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, TestExternalEnvs,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALL, GAS, POP, PUSH0, SSTORE, STOP},
    context::{result::ExecutionResult, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
};

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to execute a transaction with the given spec and database.
fn execute_transaction(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    target: alloy_primitives::Address,
) -> (ExecutionResult<MegaHaltReason>, u64) {
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, spec).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(target),
        data: Default::default(),
        value: U256::ZERO,
        gas_limit: 1_000_000_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let result = result_envelope.result;
    let compute_gas_limit = evm.ctx_ref().additional_limit.borrow().compute_gas_limit;

    (result, compute_gas_limit)
}

/// Checks if the result is a volatile data access out of gas error.
fn is_volatile_data_access_oog(result: &ExecutionResult<MegaHaltReason>) -> bool {
    matches!(
        result,
        &ExecutionResult::Halt { reason: MegaHaltReason::VolatileDataAccessOutOfGas { .. }, .. }
    )
}

/// Test that the compute gas limit is set to 10M after oracle access under REX3.
#[test]
fn test_rex3_oracle_access_sets_10m_compute_gas_limit() {
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let (result, compute_gas_limit) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(result.is_success(), "Transaction should succeed");
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex3::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "REX3 compute gas limit should be 10M after oracle access"
    );
}

/// Test that REX2 still uses the old 1M oracle access compute gas limit.
#[test]
fn test_rex2_oracle_access_still_uses_1m_compute_gas_limit() {
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let (result, compute_gas_limit) = execute_transaction(MegaSpecId::REX2, &mut db, CALLEE);

    assert!(result.is_success(), "Transaction should succeed");
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::mini_rex::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "REX2 compute gas limit should be 1M after oracle access"
    );
}

/// Test that a transaction consuming >1M but <10M compute gas after oracle access succeeds
/// under REX3 but fails under REX2.
///
/// The test constructs a contract that:
/// 1. Calls the oracle contract (triggering gas detention)
/// 2. Performs ~200 SSTOREs (each ~22,100 compute gas, total ~4.4M compute gas)
///
/// Under REX2 (1M limit): the 4.4M compute gas exceeds the 1M limit -> fails with OOG
/// Under REX3 (10M limit): the 4.4M compute gas is within the 10M limit -> succeeds
#[test]
fn test_oracle_access_succeeds_rex3_fails_rex2() {
    // Build bytecode: call oracle, then do ~200 SSTOREs (~4.4M compute gas)
    let mut builder = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP);

    // 200 SSTOREs to unique slots: ~200 * 22,100 = ~4.4M compute gas
    for i in 1..=200u32 {
        builder = builder.push_number(i).push_number(i).append(SSTORE);
    }
    let bytecode = builder.append(STOP).build();

    // REX3: should succeed (4.4M < 10M limit)
    let mut db_rex3 = MemoryDatabase::default();
    db_rex3.set_account_code(CALLEE, bytecode.clone());
    let (result_rex3, _) = execute_transaction(MegaSpecId::REX3, &mut db_rex3, CALLEE);
    assert!(
        result_rex3.is_success(),
        "REX3 transaction should succeed: ~4.4M compute gas is within the 10M oracle access limit"
    );

    // REX2: should fail (4.4M > 1M limit)
    let mut db_rex2 = MemoryDatabase::default();
    db_rex2.set_account_code(CALLEE, bytecode);
    let (result_rex2, _) = execute_transaction(MegaSpecId::REX2, &mut db_rex2, CALLEE);
    assert!(
        !result_rex2.is_success(),
        "REX2 transaction should fail: ~4.4M compute gas exceeds the 1M oracle access limit"
    );
    assert!(
        is_volatile_data_access_oog(&result_rex2),
        "REX2 should fail with VolatileDataAccessOutOfGas"
    );
}

/// Test that REX3 still enforces the 10M limit (not unlimited).
/// A transaction consuming >10M compute gas after oracle access should still fail.
#[test]
fn test_rex3_oracle_access_still_enforces_10m_limit() {
    // Build bytecode: call oracle, then do ~500 SSTOREs (~11M compute gas)
    let mut builder = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP);

    // 500 SSTOREs to unique slots: ~500 * 22,100 = ~11M compute gas
    for i in 1..=500u32 {
        builder = builder.push_number(i).push_number(i).append(SSTORE);
    }
    let bytecode = builder.append(STOP).build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);
    let (result, _) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(
        !result.is_success(),
        "REX3 transaction should fail: ~11M compute gas exceeds the 10M oracle access limit"
    );
    assert!(
        is_volatile_data_access_oog(&result),
        "Should fail with VolatileDataAccessOutOfGas"
    );
}
