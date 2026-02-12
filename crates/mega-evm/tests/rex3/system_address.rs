//! Tests for `MEGA_SYSTEM_ADDRESS` exemption from Rex3 SLOAD-based oracle gas detention.

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, TestExternalEnvs, MEGA_SYSTEM_ADDRESS,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALL, GAS, POP, PUSH0, SLOAD, STOP},
    context::TxEnv,
    handler::EvmTr,
    inspector::NoOpInspector,
};

const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Build bytecode for oracle contract that performs SLOAD(0) and returns.
fn build_oracle_sload_code() -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // key = 0
        .append(SLOAD) // SLOAD from oracle storage
        .append(POP)
        .append(STOP)
        .build()
}

/// Test that transactions from `MEGA_SYSTEM_ADDRESS` are exempted from Rex3 SLOAD-based
/// oracle gas detention.
///
/// In Rex3, oracle gas detention triggers on SLOAD from oracle storage. However,
/// system address transactions should be exempted. This test verifies that the
/// volatile data tracker does not record oracle access for `MEGA_SYSTEM_ADDRESS` transactions.
///
/// The `MEGA_SYSTEM_ADDRESS` must call the oracle contract directly (it's in the whitelist).
#[test]
fn test_mega_system_address_exempted_from_rex3_sload_detention() {
    // Deploy oracle contract code that does SLOAD (this would normally trigger detention)
    let oracle_code = build_oracle_sload_code();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context =
        MegaContext::new(&mut db, MegaSpecId::REX3).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    // MEGA_SYSTEM_ADDRESS calls oracle directly (oracle is in the whitelist)
    let tx = TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
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

    // Verify transaction succeeded
    assert!(result.is_success(), "Transaction from mega system address should succeed");

    // Key assertion: Oracle access should NOT be tracked for mega system address
    let oracle_accessed = evm
        .ctx
        .volatile_data_tracker
        .try_borrow()
        .map(|tracker| tracker.has_accessed_oracle())
        .unwrap_or(false);
    assert!(
        !oracle_accessed,
        "Oracle access should NOT be tracked for transactions from MEGA_SYSTEM_ADDRESS in Rex3"
    );

    // Verify compute gas limit was NOT set (no gas detention)
    let compute_gas_limit = evm
        .ctx
        .volatile_data_tracker
        .try_borrow()
        .map(|tracker| tracker.get_compute_gas_limit())
        .unwrap_or(None);
    assert!(
        compute_gas_limit.is_none(),
        "Compute gas limit should NOT be set for mega system address transactions"
    );
}

/// Test that a non-system address transaction IS subject to Rex3 SLOAD-based oracle
/// gas detention (contrast with the system address exemption).
#[test]
fn test_non_system_address_subject_to_rex3_sload_detention() {
    let oracle_code = build_oracle_sload_code();

    // Callee calls oracle which does SLOAD
    let callee_bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);
    db.set_account_code(CALLEE, callee_bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context =
        MegaContext::new(&mut db, MegaSpecId::REX3).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let regular_caller = address!("2000000000000000000000000000000000000002");
    let tx = TxEnv {
        caller: regular_caller,
        kind: TxKind::Call(CALLEE),
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

    assert!(result.is_success(), "Transaction should succeed");

    // Oracle access SHOULD be tracked for non-system address
    let oracle_accessed = evm
        .ctx
        .volatile_data_tracker
        .try_borrow()
        .map(|tracker| tracker.has_accessed_oracle())
        .unwrap_or(false);
    assert!(
        oracle_accessed,
        "Oracle access SHOULD be tracked for regular address transactions in Rex3"
    );

    // Compute gas limit should be set to 20M
    let compute_gas_limit = evm.ctx_ref().additional_limit.borrow().compute_gas_limit;
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex3::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "Compute gas limit should be 20M for regular transactions accessing oracle in Rex3"
    );
}
