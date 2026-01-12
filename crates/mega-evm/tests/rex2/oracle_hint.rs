//! Tests for the oracle hint mechanism introduced in Rex2.
//!
//! The hint mechanism allows on-chain contracts to send signals to the off-chain oracle
//! service backend via the `sendHint(bytes32 topic, bytes data)` function on the oracle
//! contract. The EVM intercepts these calls and forwards them to the oracle service via
//! `OracleEnv::on_hint`.

use alloy_primitives::{address, bytes, Bytes, TxKind, B256, U256};
use alloy_sol_types::{sol, SolCall};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, TestExternalEnvs, ORACLE_CONTRACT_ADDRESS,
    ORACLE_CONTRACT_CODE_REX2,
};
use revm::{
    bytecode::opcode::{CALL, GAS, MSTORE, PUSH0},
    context::TxEnv,
    inspector::NoOpInspector,
};

sol! {
    function sendHint(bytes32 topic, bytes calldata data) external;
}

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to execute a transaction and return the result along with recorded hints.
fn execute_transaction(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: &TestExternalEnvs<std::convert::Infallible>,
    target: alloy_primitives::Address,
) -> revm::context::result::ExecutionResult<mega_evm::MegaHaltReason> {
    execute_transaction_with_data(spec, db, external_envs, target, Bytes::new())
}

/// Helper function to execute a transaction with calldata.
fn execute_transaction_with_data(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: &TestExternalEnvs<std::convert::Infallible>,
    target: alloy_primitives::Address,
    data: Bytes,
) -> revm::context::result::ExecutionResult<mega_evm::MegaHaltReason> {
    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(target),
        data,
        value: U256::ZERO,
        gas_limit: 1_000_000_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    result_envelope.result
}

/// Encodes calldata for `sendHint(bytes32 topic, bytes calldata data)`.
fn encode_send_hint_calldata(topic: B256, data: &[u8]) -> Vec<u8> {
    sendHintCall { topic, data: Bytes::copy_from_slice(data) }.abi_encode()
}

/// Creates bytecode for a contract that calls `sendHint` on the oracle contract.
fn create_call_send_hint_bytecode(topic: B256, data: &[u8]) -> Bytes {
    let calldata = encode_send_hint_calldata(topic, data);
    let mut builder = BytecodeBuilder::default();

    // Store calldata in memory at offset 0
    for (i, chunk) in calldata.chunks(32).enumerate() {
        let mut padded = [0u8; 32];
        padded[..chunk.len()].copy_from_slice(chunk);
        builder = builder.push_bytes(padded).push_number((i * 32) as u8).append(MSTORE);
    }

    // CALL(gas, addr, value, argsOffset, argsSize, retOffset, retSize)
    builder
        .append(PUSH0) // retSize: 0
        .append(PUSH0) // retOffset: 0
        .push_number(calldata.len() as u16) // argsSize
        .append(PUSH0) // argsOffset: 0
        .append(PUSH0) // value: 0
        .push_address(ORACLE_CONTRACT_ADDRESS) // addr: oracle contract
        .append(GAS) // gas: all available
        .append(CALL)
        .stop()
        .build()
}

/// Creates bytecode for a contract that calls `sendHint` twice on the oracle contract.
fn create_call_send_hint_twice_bytecode(
    topic1: B256,
    data1: &[u8],
    topic2: B256,
    data2: &[u8],
) -> Bytes {
    let calldata1 = encode_send_hint_calldata(topic1, data1);
    let calldata2 = encode_send_hint_calldata(topic2, data2);
    let mut builder = BytecodeBuilder::default();

    // First call: Store calldata1 in memory at offset 0
    for (i, chunk) in calldata1.chunks(32).enumerate() {
        let mut padded = [0u8; 32];
        padded[..chunk.len()].copy_from_slice(chunk);
        builder = builder.push_bytes(padded).push_number((i * 32) as u8).append(MSTORE);
    }

    // First CALL
    builder = builder
        .append(PUSH0) // retSize: 0
        .append(PUSH0) // retOffset: 0
        .push_number(calldata1.len() as u16) // argsSize
        .append(PUSH0) // argsOffset: 0
        .append(PUSH0) // value: 0
        .push_address(ORACLE_CONTRACT_ADDRESS) // addr: oracle contract
        .append(GAS) // gas: all available
        .append(CALL);

    // Second call: Store calldata2 in memory at offset 0 (overwrite)
    for (i, chunk) in calldata2.chunks(32).enumerate() {
        let mut padded = [0u8; 32];
        padded[..chunk.len()].copy_from_slice(chunk);
        builder = builder.push_bytes(padded).push_number((i * 32) as u8).append(MSTORE);
    }

    // Second CALL
    builder
        .append(PUSH0) // retSize: 0
        .append(PUSH0) // retOffset: 0
        .push_number(calldata2.len() as u16) // argsSize
        .append(PUSH0) // argsOffset: 0
        .append(PUSH0) // value: 0
        .push_address(ORACLE_CONTRACT_ADDRESS) // addr: oracle contract
        .append(GAS) // gas: all available
        .append(CALL)
        .stop()
        .build()
}

/// Test that `on_hint` is called when Rex2 is enabled and oracle contract emits a Hint event.
#[test]
fn test_on_hint_called_on_rex2() {
    let user_topic = B256::from_slice(&[0x42u8; 32]);
    let hint_data = bytes!("deadbeef");

    // Main contract that calls sendHint on the oracle contract
    let main_code = create_call_send_hint_bytecode(user_topic, &hint_data);

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    // Deploy the actual v1.1.0 oracle contract
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    let result = execute_transaction(MegaSpecId::REX2, &mut db, &external_envs, CALLEE);

    assert!(result.is_success(), "Transaction should succeed");

    // Verify that on_hint was called with the correct from, topic and data
    let hints = external_envs.recorded_hints();
    assert_eq!(hints.len(), 1, "Should have recorded exactly one hint");
    assert_eq!(hints[0].from, CALLEE, "Hint from should be the caller contract");
    assert_eq!(hints[0].topic, user_topic, "Hint topic should match");
    assert_eq!(hints[0].data, hint_data, "Hint data should match");
}

/// Test that `on_hint` is NOT called when Rex1 is enabled (pre-Rex2).
#[test]
fn test_on_hint_not_called_on_rex1() {
    let user_topic = B256::from_slice(&[0x42u8; 32]);
    let hint_data = bytes!("deadbeef");

    // Main contract that calls sendHint on the oracle contract
    let main_code = create_call_send_hint_bytecode(user_topic, &hint_data);

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    // Deploy the actual v1.1.0 oracle contract
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    let result = execute_transaction(MegaSpecId::REX1, &mut db, &external_envs, CALLEE);

    assert!(result.is_success(), "Transaction should succeed");

    // Verify that on_hint was NOT called
    let hints = external_envs.recorded_hints();
    assert!(hints.is_empty(), "Should NOT have recorded any hints on Rex1");
}

/// Test that `on_hint` is NOT called for calls to non-oracle contracts.
/// Even if a contract has a sendHint function, it must be the oracle contract
/// for the hint to be intercepted.
#[test]
fn test_on_hint_not_called_for_non_oracle_contract() {
    let user_topic = B256::from_slice(&[0x42u8; 32]);
    let hint_data = bytes!("deadbeef");
    let fake_oracle = address!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

    // Deploy the oracle contract bytecode at a non-oracle address
    // This fake oracle has the same sendHint function, but is not at ORACLE_CONTRACT_ADDRESS
    let mut db = MemoryDatabase::default();
    db.set_account_code(fake_oracle, ORACLE_CONTRACT_CODE_REX2);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    // Call sendHint on the fake oracle contract directly
    let calldata = encode_send_hint_calldata(user_topic, &hint_data);
    let result = execute_transaction_with_data(
        MegaSpecId::REX2,
        &mut db,
        &external_envs,
        fake_oracle,
        Bytes::from(calldata),
    );

    assert!(result.is_success(), "Transaction should succeed");

    // Verify that on_hint was NOT called (call was not to the oracle contract address)
    let hints = external_envs.recorded_hints();
    assert!(hints.is_empty(), "Should NOT have recorded hints from non-oracle contract address");
}

/// Test that multiple hints can be recorded from a single transaction.
#[test]
fn test_multiple_hints_recorded() {
    let topic1 = B256::from_slice(&[0x11u8; 32]);
    let topic2 = B256::from_slice(&[0x22u8; 32]);
    let data1 = bytes!("aabb");
    let data2 = bytes!("ccdd");

    // Main contract that calls sendHint twice on the oracle contract
    let main_code = create_call_send_hint_twice_bytecode(topic1, &data1, topic2, &data2);

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    // Deploy the actual v1.1.0 oracle contract
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    let result = execute_transaction(MegaSpecId::REX2, &mut db, &external_envs, CALLEE);

    assert!(result.is_success(), "Transaction should succeed");

    // Verify that both hints were recorded with correct from addresses
    let hints = external_envs.recorded_hints();
    assert_eq!(hints.len(), 2, "Should have recorded two hints");
    assert_eq!(hints[0].from, CALLEE, "First hint from should be the caller contract");
    assert_eq!(hints[0].topic, topic1, "First hint topic should match");
    assert_eq!(hints[0].data, data1, "First hint data should match");
    assert_eq!(hints[1].from, CALLEE, "Second hint from should be the caller contract");
    assert_eq!(hints[1].topic, topic2, "Second hint topic should match");
    assert_eq!(hints[1].data, data2, "Second hint data should match");
}

/// Test that `on_hint` is called when transaction directly targets the oracle contract.
#[test]
fn test_on_hint_direct_oracle_call() {
    let user_topic = B256::from_slice(&[0x42u8; 32]);
    let hint_data = bytes!("deadbeef");

    let mut db = MemoryDatabase::default();
    // Deploy the actual v1.1.0 oracle contract
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    // Call sendHint on the oracle contract directly
    let calldata = encode_send_hint_calldata(user_topic, &hint_data);
    let result = execute_transaction_with_data(
        MegaSpecId::REX2,
        &mut db,
        &external_envs,
        ORACLE_CONTRACT_ADDRESS,
        Bytes::from(calldata),
    );

    assert!(result.is_success(), "Transaction should succeed");

    // Verify that on_hint was called with CALLER as the from address (direct call)
    let hints = external_envs.recorded_hints();
    assert_eq!(hints.len(), 1, "Should have recorded exactly one hint");
    assert_eq!(hints[0].from, CALLER, "Hint from should be the transaction caller");
    assert_eq!(hints[0].topic, user_topic, "Hint topic should match");
    assert_eq!(hints[0].data, hint_data, "Hint data should match");
}
