//! Tests for oracle contract access detection.
#![allow(clippy::doc_markdown)]

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    constants::mini_rex::{ORACLE_ACCESS_COMPUTE_GAS, TX_COMPUTE_GAS_LIMIT},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, MegaContext, MegaEvm, MegaHaltReason, MegaHardforkConfig, MegaSpecId,
    MegaTransaction, TestExternalEnvs, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLCODE, DELEGATECALL, GAS, MSTORE, POP, PUSH0, RETURN, RETURNDATACOPY,
        RETURNDATASIZE, SLOAD, SSTORE, STATICCALL, TIMESTAMP,
    },
    context::{result::ExecutionResult, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    Inspector,
};

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to execute a transaction with the given database.
/// Returns a tuple of `(ExecutionResult, MegaEvm, oracle_accessed: bool)`.
fn execute_transaction<
    'a,
    INSP: Inspector<MegaContext<&'a mut MemoryDatabase, &'a TestExternalEnvs<std::convert::Infallible>>>,
>(
    spec: MegaSpecId,
    db: &'a mut MemoryDatabase,
    external_envs: &'a TestExternalEnvs<std::convert::Infallible>,
    inspector: INSP,
    target: alloy_primitives::Address,
) -> (
    ExecutionResult<MegaHaltReason>,
    MegaEvm<&'a mut MemoryDatabase, INSP, &'a TestExternalEnvs<std::convert::Infallible>>,
    bool,
) {
    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
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

    let mut evm = MegaEvm::new(context).with_inspector(inspector);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let result = result_envelope.result;
    // Get oracle_accessed before returning to avoid RefCell borrow conflicts
    let oracle_accessed = evm
        .ctx
        .volatile_data_tracker
        .try_borrow()
        .map(|tracker| tracker.has_accessed_oracle())
        .unwrap_or(false);

    (result, evm, oracle_accessed)
}

/// Checks if the result is a volatile data access out of gas error.
fn is_volatile_data_access_oog(result: &ExecutionResult<MegaHaltReason>) -> bool {
    matches!(
        result,
        &ExecutionResult::Halt { reason: MegaHaltReason::VolatileDataAccessOutOfGas { .. }, .. }
    )
}

/// Test that contract runs out of gas when trying to execute expensive operations after oracle
/// access.
#[test]
fn test_parent_runs_out_of_gas_after_oracle_access() {
    const INTERMEDIATE_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");

    // Create intermediate contract that calls the oracle
    let mut builder = BytecodeBuilder::default();
    builder = builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL);
    // After the call returns, the left gas is limited to 10k
    // Try to execute 1000000 SSTORE operations (each costs 5000 gas minimum)
    // This should run out of gas partway through
    for i in 1..=1000 {
        builder = builder
            .push_number(i as u32) // offset: varying offset to avoid optimization
            .push_number(i as u32) // size: 32 bytes
            .append(SSTORE);
    }
    let intermediate_code = builder.stop().build();

    // Create main contract that:
    // 1. Calls intermediate contract (which accesses oracle)
    // 2. After return, tries to execute many expensive KECCAK256 operations
    // Expected: Parent gas is limited to 10k after oracle access, can't complete all operations
    let mut builder = BytecodeBuilder::default();
    // Call intermediate contract
    builder = builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(INTERMEDIATE_CONTRACT)
        .append(GAS)
        .append(CALL);
    let main_code = builder.append_many([SLOAD, SLOAD, SLOAD, SLOAD, SLOAD, SLOAD]).stop().build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    db.set_account_storage(INTERMEDIATE_CONTRACT, U256::ZERO, U256::from(0x2333u64));
    db.set_account_code(INTERMEDIATE_CONTRACT, intermediate_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");

    // The transaction runs out of gas - the parent frame couldn't complete all expensive operations
    // because its gas was limited to 10k after oracle access
    assert!(!result.is_success(), "Transaction should run out of gas");
    assert!(
        is_volatile_data_access_oog(&result),
        "Transaction should fail due to volatile data access out of gas"
    );
}

#[test]
fn test_oracle_volatile_data_access_oog_does_not_consume_all_gas() {
    // This test verifies that when a transaction runs out of gas due to oracle access
    // (VolatileDataAccessOutOfGas with Oracle type), it does NOT consume all gas.
    // Instead, detained gas is refunded and gas_used reflects only actual work performed.
    let mut db = MemoryDatabase::default();
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    // Contract that calls the oracle then tries expensive work that exceeds the 1M oracle limit
    let mut builder = BytecodeBuilder::default()
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argSize
        .push_number(0u8) // argOffset
        .push_number(0u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS) // oracle address
        .push_number(0xffffu16) // gas
        .append(CALL) // Call oracle - limits gas to 1M
        .append(POP); // pop result

    // Try to do 1000 SSTOREs (2M gas needed, but only 1M available after oracle limiting)
    for i in 1..=1000 {
        builder = builder.push_number(i as u32).push_number(i as u32).append(SSTORE);
    }
    let bytecode = builder.stop().build();
    db.set_account_code(CALLEE, bytecode);

    let (result, _, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    assert!(oracle_accessed, "Oracle should have been accessed");
    // Should fail with VolatileDataAccessOutOfGas
    assert!(!result.is_success(), "Transaction should fail due to oracle volatile data access OOG");
    assert!(
        is_volatile_data_access_oog(&result),
        "Transaction should fail due to volatile data access out of gas"
    );

    let gas_used = result.gas_used();

    // Key assertion: gas_used should be much less than gas_limit
    assert!(
        gas_used < 1_000_000_000,
        "gas_used should be much less than gas_limit, proving detained gas was refunded. Got: {}",
        gas_used
    );
}

#[test]
fn test_both_volatile_data_access_oog_does_not_consume_all_gas() {
    // This test verifies that when BOTH block env and oracle are accessed, and the transaction
    // runs out of gas, the halt reason correctly identifies "Both" type with the most restrictive
    // limit (1M from oracle), and detained gas is properly refunded.

    let mut db = MemoryDatabase::default();
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    // Contract that accesses TIMESTAMP (20M limit), then calls oracle (1M limit),
    // then tries expensive work that exceeds the 1M limit
    let mut builder = BytecodeBuilder::default()
        .append(TIMESTAMP) // Limits gas to 20M
        .append(POP)
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argSize
        .push_number(0u8) // argOffset
        .push_number(0u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS) // oracle address
        .push_number(0xffffu16) // gas
        .append(CALL) // Call oracle - further limits gas to 1M
        .append(POP); // pop result

    // Try to do 1000 SSTOREs (2M gas needed, but only 1M available)
    for i in 1..=1000 {
        builder = builder.push_number(i as u32).push_number(i as u32).append(SSTORE);
    }
    let bytecode = builder.stop().build();
    db.set_account_code(CALLEE, bytecode);

    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    // Should fail with VolatileDataAccessOutOfGas
    assert!(oracle_accessed, "Oracle should have been accessed");
    assert!(!result.is_success(), "Transaction should fail due to volatile data access OOG");
    assert!(
        is_volatile_data_access_oog(&result),
        "Transaction should fail due to volatile data access out of gas"
    );

    let gas_used = result.gas_used();
    // Key assertion: gas_used should be much less than gas_limit
    assert!(
        gas_used < 1_000_000_000,
        "gas_used should be much less than gas_limit, proving detained gas was refunded. Got: {}",
        gas_used
    );
}
