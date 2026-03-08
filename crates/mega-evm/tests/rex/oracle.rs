//! Tests for oracle contract access detection in Rex spec.
//!
//! Rex fixes STATICCALL oracle access detection that was bypassed in `MiniRex`.
//! CALLCODE and DELEGATECALL remain undetected because they execute in the caller's
//! state context (not the oracle's), so they don't constitute oracle access.

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, TestExternalEnvs,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALLCODE, DELEGATECALL, GAS, PUSH0, STATICCALL},
    context::{result::ExecutionResult, TxEnv},
    inspector::NoOpInspector,
    Inspector,
};

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to execute a transaction with the given spec and database.
/// Returns a tuple of `(ExecutionResult, oracle_accessed: bool)`.
fn execute_transaction<
    'a,
    INSP: Inspector<MegaContext<&'a mut MemoryDatabase, &'a TestExternalEnvs<std::convert::Infallible>>>,
>(
    spec: MegaSpecId,
    db: &'a mut MemoryDatabase,
    external_envs: &'a TestExternalEnvs<std::convert::Infallible>,
    inspector: INSP,
    target: alloy_primitives::Address,
) -> (ExecutionResult<MegaHaltReason>, bool) {
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
    let oracle_accessed = evm
        .ctx
        .volatile_data_tracker
        .try_borrow()
        .map(|tracker| tracker.has_accessed_oracle())
        .unwrap_or(false);

    (result, oracle_accessed)
}

/// Test that STATICCALL to oracle IS detected in Rex.
///
/// Rex fixes the `MiniRex` bypass: STATICCALL to oracle now triggers oracle access detection.
#[test]
fn test_rex_staticcall_oracle_access_detected() {
    // STATICCALL: gas, addr, argsOff, argsLen, retOff, retLen (6 args, no value)
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // retLen, retOff, argsLen, argsOff
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(STATICCALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, oracle_accessed) =
        execute_transaction(MegaSpecId::REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    assert!(result.is_success(), "Transaction should succeed");
    assert!(oracle_accessed, "STATICCALL to oracle should be detected in Rex");
}

/// Test that DELEGATECALL to oracle is NOT detected in Rex.
///
/// DELEGATECALL executes in the caller's state context, not the oracle's, so it does not
/// constitute oracle access.
#[test]
fn test_rex_delegatecall_oracle_access_not_detected() {
    // DELEGATECALL: gas, addr, argsOff, argsLen, retOff, retLen (6 args, no value)
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // retLen, retOff, argsLen, argsOff
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(DELEGATECALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, oracle_accessed) =
        execute_transaction(MegaSpecId::REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    assert!(result.is_success(), "Transaction should succeed");
    assert!(!oracle_accessed, "DELEGATECALL to oracle should not be detected in Rex");
}

/// Test that CALLCODE to oracle is NOT detected in Rex.
///
/// CALLCODE executes in the caller's state context, not the oracle's, so it does not
/// constitute oracle access.
#[test]
fn test_rex_callcode_oracle_access_not_detected() {
    // CALLCODE: gas, addr, value, argsOff, argsLen, retOff, retLen (7 args)
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]) // retLen, retOff, argsLen, argsOff, value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALLCODE)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, oracle_accessed) =
        execute_transaction(MegaSpecId::REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    assert!(result.is_success(), "Transaction should succeed");
    assert!(!oracle_accessed, "CALLCODE to oracle should not be detected in Rex");
}
