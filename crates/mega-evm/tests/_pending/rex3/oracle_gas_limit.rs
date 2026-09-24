//! Tests for the Rex3 oracle access compute gas limit (1M -> 20M) and
//! the Rex3 change to trigger oracle gas detention on SLOAD instead of CALL.

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, TestExternalEnvs,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALL, GAS, POP, PUSH0, SLOAD, SSTORE, STOP, TIMESTAMP},
    context::{result::ExecutionResult, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
};

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");
const CONTRACT_B: alloy_primitives::Address = address!("3000000000000000000000000000000000000003");
const REGULAR_CONTRACT: alloy_primitives::Address =
    address!("4000000000000000000000000000000000000004");

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
    let compute_gas_limit = evm.ctx_ref().additional_limit.borrow().compute_gas_limit();

    (result, compute_gas_limit)
}

/// Checks if the result is a volatile data access out of gas error.
fn is_volatile_data_access_oog(result: &ExecutionResult<MegaHaltReason>) -> bool {
    matches!(
        result,
        &ExecutionResult::Halt { reason: MegaHaltReason::VolatileDataAccessOutOfGas { .. }, .. }
    )
}

/// Build bytecode that CALLs the oracle contract and then STOPs.
fn build_call_oracle_bytecode() -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// Build bytecode for oracle contract that performs SLOAD(0) and returns.
/// Deployed at `ORACLE_CONTRACT_ADDRESS` so that SLOAD reads oracle storage.
fn build_oracle_sload_code() -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // key = 0
        .append(SLOAD) // SLOAD from oracle storage (triggers Rex3 oracle access)
        .append(POP)
        .append(STOP)
        .build()
}

// =============================================================================
// Rex3: Oracle access via SLOAD tests
// =============================================================================

/// Test that REX3 still enforces the 20M limit (not unlimited).
/// A transaction consuming >20M compute gas after oracle SLOAD should still fail.
#[test]
fn test_rex3_oracle_access_still_enforces_20m_limit() {
    // Oracle contract code for Rex3 - performs SLOAD to trigger detention
    let oracle_code = build_oracle_sload_code();

    // Build bytecode: call oracle (with SLOAD), then do ~1000 SSTOREs (~22M compute gas)
    let mut builder = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP);

    // 1000 SSTOREs to unique slots: ~1000 * 22,100 = ~22M compute gas
    for i in 1..=1000u32 {
        builder = builder.push_number(i).push_number(i).append(SSTORE);
    }
    let bytecode = builder.append(STOP).build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);
    db.set_account_code(CALLEE, bytecode);
    let (result, _) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(
        !result.is_success(),
        "REX3 transaction should fail: ~22M compute gas exceeds the 20M oracle access limit"
    );
    assert!(is_volatile_data_access_oog(&result), "Should fail with VolatileDataAccessOutOfGas");
}

// =============================================================================
// Rex3: Additional oracle SLOAD detention tests
// =============================================================================
