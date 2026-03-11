//! Tests for keyless deploy salt environment fix in Rex4.
//!
//! Rex4 fixes the keyless deploy sandbox to share the parent's salt env and oracle env,
//! ensuring correct dynamic gas pricing for storage operations in keyless deploy constructors.
//! Pre-Rex4 specs retain the `EmptyExternalEnv` behavior for backward compatibility.

use std::vec::Vec;

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::{sol, SolCall};
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::ExecutionResult,
    sandbox::{
        calculate_keyless_deploy_address,
        tests::{CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER, CREATE2_FACTORY_TX},
    },
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, SaltEnv,
    TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS, MIN_BUCKET_SIZE, ORACLE_CONTRACT_ADDRESS,
    ORACLE_CONTRACT_CODE_REX2,
};
use revm::{
    bytecode::opcode::{CALL, GAS, MSTORE, POP, PUSH0, RETURN},
    context::TxEnv,
    inspector::NoOpInspector,
};

sol! {
    function sendHint(bytes32 topic, bytes calldata data) external;
}

// ============================================================================
// TEST CONSTANTS
// ============================================================================

/// Standard test caller address.
const TEST_CALLER: Address = address!("0000000000000000000000000000000000100000");

/// Large gas limit override for tests (10 billion gas).
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Executes a keyless deploy with the given spec and external envs.
///
/// Returns the execution result and the sandbox `gas_used` (from the ABI-encoded return data).
fn execute_keyless_deploy_with_envs(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: &TestExternalEnvs<std::convert::Infallible>,
    keyless_deployment_transaction: Bytes,
) -> (ExecutionResult<MegaHaltReason>, u64) {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_deployment_transaction,
        gasLimitOverride: U256::from(LARGE_GAS_LIMIT_OVERRIDE),
    }
    .abi_encode();

    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
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

    // Extract sandbox gas_used from the ABI-encoded return data
    let sandbox_gas_used = match &result {
        ExecutionResult::Success { output, .. } => {
            let output_data = output.data();
            let decoded = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output_data)
                .expect("decode return data");
            decoded.gasUsed
        }
        _ => 0,
    };

    (result, sandbox_gas_used)
}

/// Computes the bucket ID for the CREATE2 factory contract (the deployed address).
fn contract_bucket_id() -> u32 {
    <TestExternalEnvs as SaltEnv>::bucket_id_for_account(CREATE2_FACTORY_CONTRACT)
}

/// Computes the bucket ID for the CREATE2 factory deployer signer.
fn deployer_bucket_id() -> u32 {
    <TestExternalEnvs as SaltEnv>::bucket_id_for_account(CREATE2_FACTORY_DEPLOYER)
}

/// Creates a pre-EIP-155 keyless deploy transaction with deterministic signature.
fn create_pre_eip155_deploy_tx(init_code: Bytes) -> (Bytes, Address) {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: 1_000_000,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
        chain_id: None,
    };

    let r = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let s = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let sig = Signature::new(r, s, false);
    let signed = Signed::new_unchecked(tx, sig, B256::ZERO);

    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    let tx_bytes = Bytes::from(buf);
    let signer = signed.recover_signer().expect("should recover signer");

    (tx_bytes, signer)
}

/// Encodes calldata for `sendHint(bytes32 topic, bytes data)`.
fn encode_send_hint_calldata(topic: B256, data: &[u8]) -> Vec<u8> {
    sendHintCall { topic, data: Bytes::copy_from_slice(data) }.abi_encode()
}

/// Creates init code that emits an oracle hint during construction, then returns `STOP`.
fn create_constructor_send_hint_init_code(topic: B256, data: &[u8]) -> Bytes {
    let calldata = encode_send_hint_calldata(topic, data);
    let mut builder = BytecodeBuilder::default();

    for (i, chunk) in calldata.chunks(32).enumerate() {
        let mut padded = [0u8; 32];
        padded[..chunk.len()].copy_from_slice(chunk);
        builder = builder.push_bytes(padded).push_number((i * 32) as u8).append(MSTORE);
    }

    builder = builder
        .append(PUSH0)
        .append(PUSH0)
        .push_number(calldata.len() as u16)
        .append(PUSH0)
        .append(PUSH0)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP);

    let runtime_offset = (builder.len() + 12) as u8;
    builder
        .push_number(1_u8)
        .push_number(runtime_offset)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .stop()
        .build()
}

// ============================================================================
// TESTS
// ============================================================================

/// Rex4 keyless deploy sandbox uses the parent's salt env.
///
/// With a high bucket capacity, the salt multiplier increases, causing higher gas for the
/// contract creation inside the sandbox.
/// We compare sandbox `gas_used` with default (1x) vs elevated bucket capacity to verify
/// the sandbox actually uses the salt env.
#[test]
fn test_rex4_keyless_deploy_uses_salt_env() {
    // Run with default bucket capacity (1x multiplier)
    let mut db_default = MemoryDatabase::default();
    db_default.set_account_balance(
        CREATE2_FACTORY_DEPLOYER,
        U256::from(1_000_000_000_000_000_000_000u128),
    );
    let env_default = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result_default, gas_default) = execute_keyless_deploy_with_envs(
        MegaSpecId::REX4,
        &mut db_default,
        &env_default,
        Bytes::from_static(CREATE2_FACTORY_TX),
    );
    assert!(
        result_default.is_success(),
        "Default deploy should succeed, got: {:?}",
        result_default
    );

    // Run with elevated bucket capacity (2x multiplier) for both deployer and contract buckets
    let mut db_high = MemoryDatabase::default();
    db_high.set_account_balance(
        CREATE2_FACTORY_DEPLOYER,
        U256::from(1_000_000_000_000_000_000_000u128),
    );
    let high_capacity = (MIN_BUCKET_SIZE as u64) * 2;
    let env_high = TestExternalEnvs::<std::convert::Infallible>::new()
        .with_bucket_capacity(contract_bucket_id(), high_capacity)
        .with_bucket_capacity(deployer_bucket_id(), high_capacity);
    let (result_high, gas_high) = execute_keyless_deploy_with_envs(
        MegaSpecId::REX4,
        &mut db_high,
        &env_high,
        Bytes::from_static(CREATE2_FACTORY_TX),
    );
    assert!(
        result_high.is_success(),
        "High-capacity deploy should succeed, got: {:?}",
        result_high
    );

    // REX4 sandbox shares salt env, so elevated bucket capacity should increase gas
    assert!(
        gas_high > gas_default,
        "REX4 with higher bucket capacity should use more sandbox gas: default={}, high={}",
        gas_default,
        gas_high,
    );
}

/// Rex3 keyless deploy ignores the salt env (backward compatibility).
///
/// Pre-Rex4 sandbox always uses `EmptyExternalEnv`, so changing bucket capacity
/// should not affect sandbox `gas_used`.
#[test]
fn test_rex3_keyless_deploy_ignores_salt_env() {
    // Run with default bucket capacity
    let mut db_default = MemoryDatabase::default();
    db_default.set_account_balance(
        CREATE2_FACTORY_DEPLOYER,
        U256::from(1_000_000_000_000_000_000_000u128),
    );
    let env_default = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result_default, gas_default) = execute_keyless_deploy_with_envs(
        MegaSpecId::REX3,
        &mut db_default,
        &env_default,
        Bytes::from_static(CREATE2_FACTORY_TX),
    );
    assert!(
        result_default.is_success(),
        "Default deploy should succeed, got: {:?}",
        result_default
    );

    // Run with elevated bucket capacity
    let mut db_high = MemoryDatabase::default();
    db_high.set_account_balance(
        CREATE2_FACTORY_DEPLOYER,
        U256::from(1_000_000_000_000_000_000_000u128),
    );
    let high_capacity = (MIN_BUCKET_SIZE as u64) * 2;
    let env_high = TestExternalEnvs::<std::convert::Infallible>::new()
        .with_bucket_capacity(contract_bucket_id(), high_capacity)
        .with_bucket_capacity(deployer_bucket_id(), high_capacity);
    let (result_high, gas_high) = execute_keyless_deploy_with_envs(
        MegaSpecId::REX3,
        &mut db_high,
        &env_high,
        Bytes::from_static(CREATE2_FACTORY_TX),
    );
    assert!(
        result_high.is_success(),
        "High-capacity deploy should succeed, got: {:?}",
        result_high
    );

    // Pre-REX4 sandbox uses EmptyExternalEnv, so gas should be the same regardless of capacity
    assert_eq!(
        gas_default, gas_high,
        "REX3 sandbox gas should be the same regardless of bucket capacity: default={}, high={}",
        gas_default, gas_high,
    );
}

/// Rex4 keyless deploy sandbox forwards oracle hints emitted in constructors.
#[test]
fn test_rex4_keyless_deploy_constructor_uses_oracle_env() {
    let topic = B256::repeat_byte(0x11);
    let hint_data = b"rex4-sandbox-hint";
    let init_code = create_constructor_send_hint_init_code(topic, hint_data);
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000_000u128));
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _) =
        execute_keyless_deploy_with_envs(MegaSpecId::REX4, &mut db, &external_envs, tx_bytes);

    assert!(result.is_success(), "keyless deploy should succeed, got: {:?}", result);

    let hints = external_envs.recorded_hints();
    let deploy_address = calculate_keyless_deploy_address(signer);
    assert_eq!(hints.len(), 1, "constructor should emit exactly one oracle hint");
    assert_eq!(hints[0].from, deploy_address, "hint sender should be the constructor address");
    assert_eq!(hints[0].topic, topic, "hint topic should match constructor input");
    assert_eq!(hints[0].data, Bytes::copy_from_slice(hint_data), "hint data should match");
}

/// Rex3 keyless deploy sandbox keeps the historical empty oracle env behavior.
#[test]
fn test_rex3_keyless_deploy_constructor_ignores_oracle_env() {
    let topic = B256::repeat_byte(0x22);
    let hint_data = b"rex3-sandbox-hint";
    let init_code = create_constructor_send_hint_init_code(topic, hint_data);
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000_000u128));
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _) =
        execute_keyless_deploy_with_envs(MegaSpecId::REX3, &mut db, &external_envs, tx_bytes);

    assert!(result.is_success(), "keyless deploy should succeed, got: {:?}", result);
    assert!(
        external_envs.recorded_hints().is_empty(),
        "pre-REX4 sandbox should not forward oracle hints",
    );
}
