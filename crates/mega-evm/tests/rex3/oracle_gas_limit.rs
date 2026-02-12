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

/// Test that the compute gas limit is set to 20M after oracle SLOAD under REX3.
#[test]
fn test_rex3_oracle_sload_sets_20m_compute_gas_limit() {
    // Deploy oracle contract code that does SLOAD
    let oracle_code = build_oracle_sload_code();
    let callee_bytecode = build_call_oracle_bytecode();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);
    db.set_account_code(CALLEE, callee_bytecode);

    let (result, compute_gas_limit) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex3::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "REX3 compute gas limit should be 20M after oracle SLOAD"
    );
}

/// Test that REX3 CALL to oracle contract WITHOUT reading storage does NOT trigger gas detention.
#[test]
fn test_rex3_call_oracle_without_sload_no_detention() {
    // Oracle contract has code that just STOPs (no SLOAD)
    let oracle_code = BytecodeBuilder::default().append(STOP).build();

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

    let (result, compute_gas_limit) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);
    // Compute gas limit should remain at the default (200M), NOT the oracle detention limit.
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex::TX_COMPUTE_GAS_LIMIT,
        "REX3: CALL to oracle without SLOAD should NOT trigger gas detention"
    );
}

/// Test that REX2 still uses CALL-based oracle access detection (1M limit).
#[test]
fn test_rex2_oracle_access_still_uses_1m_compute_gas_limit() {
    // For Rex2, just calling oracle (no code needed) triggers detention
    let bytecode = build_call_oracle_bytecode();

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

/// Test that a transaction consuming >1M but <20M compute gas after oracle access succeeds
/// under REX3 but fails under REX2.
///
/// Rex3 path: CALL oracle (with SLOAD code) triggers 20M detention, then ~4.4M SSTOREs succeed.
/// Rex2 path: CALL oracle triggers 1M detention, then ~4.4M SSTOREs exceed the limit.
#[test]
fn test_oracle_access_succeeds_rex3_fails_rex2() {
    // Oracle contract code for Rex3 - performs SLOAD to trigger detention
    let oracle_code = build_oracle_sload_code();

    // Build callee bytecode: call oracle, then do ~200 SSTOREs (~4.4M compute gas)
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

    // REX3: should succeed (4.4M < 20M limit)
    let mut db_rex3 = MemoryDatabase::default();
    db_rex3.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);
    db_rex3.set_account_code(CALLEE, bytecode.clone());
    let (result_rex3, _) = execute_transaction(MegaSpecId::REX3, &mut db_rex3, CALLEE);
    assert!(
        result_rex3.is_success(),
        "REX3 transaction should succeed: ~4.4M compute gas is within the 20M oracle access limit"
    );

    // REX2: should fail (4.4M > 1M limit). No oracle code needed - CALL triggers detention.
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

/// Test that SLOAD from oracle contract triggers gas detention in Rex3
/// (direct SLOAD test without CALL wrapper).
#[test]
fn test_rex3_direct_oracle_sload_triggers_detention() {
    // The callee IS the oracle contract itself.
    // Code: SLOAD(0) + POP + STOP
    let oracle_code = build_oracle_sload_code();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let (result, compute_gas_limit) =
        execute_transaction(MegaSpecId::REX3, &mut db, ORACLE_CONTRACT_ADDRESS);

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex3::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "REX3: SLOAD from oracle contract should trigger 20M gas detention"
    );
}

// =============================================================================
// Rex3: Additional oracle SLOAD detention tests
// =============================================================================

/// Test that SLOAD from a regular (non-oracle) contract does NOT trigger gas detention.
/// Only SLOAD from oracle contract storage should trigger detention.
#[test]
fn test_rex3_sload_from_non_oracle_contract_no_detention() {
    // Regular contract does SLOAD(0) - this should NOT trigger oracle detention
    let regular_code = BytecodeBuilder::default()
        .append(PUSH0) // key = 0
        .append(SLOAD) // SLOAD from regular contract's own storage
        .append(POP)
        .append(STOP)
        .build();

    // Callee calls the regular contract
    let callee_bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8) // value: 0 wei
        .push_address(REGULAR_CONTRACT)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(REGULAR_CONTRACT, regular_code);
    db.set_account_code(CALLEE, callee_bytecode);

    let (result, compute_gas_limit) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);
    // Compute gas limit should remain at the default (200M) - no detention triggered
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex::TX_COMPUTE_GAS_LIMIT,
        "SLOAD from non-oracle contract should NOT trigger gas detention"
    );
}

/// Test that nested calls through an intermediate contract still trigger detention when
/// the oracle's SLOAD is reached.
/// Call chain: CALLEE -> `CONTRACT_B` -> ORACLE (SLOAD) -> detention at 20M
#[test]
fn test_rex3_nested_call_oracle_sload_triggers_detention() {
    // Oracle contract code: SLOAD(0) + POP + STOP
    let oracle_code = build_oracle_sload_code();

    // Contract B: calls oracle contract
    let contract_b_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();

    // Callee: calls Contract B
    let callee_bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8) // value: 0 wei
        .push_address(CONTRACT_B)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);
    db.set_account_code(CONTRACT_B, contract_b_code);
    db.set_account_code(CALLEE, callee_bytecode);

    let (result, compute_gas_limit) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex3::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "Nested call through intermediate contract should still trigger 20M oracle detention"
    );
}

/// Test that accessing both block env (TIMESTAMP) and oracle SLOAD results in the combined
/// (minimum) compute gas limit. In Rex3, both limits are 20M, so the combined limit is 20M.
#[test]
fn test_rex3_block_env_and_oracle_sload_combined_limit() {
    // Oracle contract code: SLOAD(0) + POP + STOP
    let oracle_code = build_oracle_sload_code();

    // Callee: TIMESTAMP (block env access) + CALL oracle (SLOAD access)
    let callee_bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP) // Block env access -> 20M limit
        .append(POP)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL) // Oracle SLOAD access -> 20M limit
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);
    db.set_account_code(CALLEE, callee_bytecode);

    let (result, compute_gas_limit) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);
    // Both block env (20M) and oracle (20M) limits are equal, so combined = min(20M, 20M) = 20M
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex3::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "Combined block env + oracle access should result in 20M limit in Rex3"
    );
}

/// Test that calling oracle with sendHint (intercepted in `frame_init`) does NOT trigger
/// SLOAD-based detention in Rex3, since sendHint is intercepted before the oracle code
/// executes any SLOAD.
#[test]
fn test_rex3_send_hint_call_no_detention() {
    // Oracle contract code that has sendHint ABI entry but no SLOAD.
    // In practice, sendHint calls are intercepted in frame_init before the oracle code runs,
    // so the oracle bytecode doesn't matter. We deploy an oracle with just STOP to confirm.
    let oracle_code = BytecodeBuilder::default().append(STOP).build();

    // Build sendHint(bytes32,bytes) calldata
    use alloy_sol_types::SolCall;
    let hint_call =
        mega_evm::IOracle::sendHintCall { topic: alloy_primitives::B256::ZERO, data: Bytes::new() };
    let hint_calldata: Vec<u8> = hint_call.abi_encode();

    // Callee: store sendHint calldata in memory, then CALL oracle with it
    let callee_bytecode = BytecodeBuilder::default()
        .mstore(0, &hint_calldata)
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(hint_calldata.len() as u64) // argsLength
        .push_number(0u8) // argsOffset
        .push_number(0u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS) // gas
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);
    db.set_account_code(CALLEE, callee_bytecode);

    let (result, compute_gas_limit) = execute_transaction(MegaSpecId::REX3, &mut db, CALLEE);

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);
    // sendHint is intercepted before oracle code runs, so no SLOAD -> no detention
    assert_eq!(
        compute_gas_limit,
        mega_evm::constants::rex::TX_COMPUTE_GAS_LIMIT,
        "sendHint call should NOT trigger SLOAD-based gas detention in Rex3"
    );
}
