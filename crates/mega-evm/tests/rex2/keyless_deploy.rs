//! Tests for keyless deployment.
//!
//! This module provides comprehensive tests for the keyless deployment system contract,
//! including success cases, error conditions, and edge cases.

use alloy_primitives::{address, hex, keccak256, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::{ExecutionResult, ResultAndState},
    sandbox::{
        decode_error_result,
        tests::{
            CREATE2_FACTORY_CODE_HASH, CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER,
            CREATE2_FACTORY_TX, EIP1820_CODE_HASH, EIP1820_CONTRACT, EIP1820_DEPLOYER, EIP1820_TX,
            NON_CONTRACT_CREATION_TX, POST_EIP155_CHAIN_1_TX,
        },
        KeylessDeployError,
    },
    test_utils::{transact, BytecodeBuilder, MemoryDatabase},
    IKeylessDeploy, MegaSpecId, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE,
};
use revm::bytecode::opcode::{
    CALL, CALLDATACOPY, CALLDATASIZE, CODECOPY, CREATE, GAS, ISZERO, JUMPDEST, JUMPI, LOG0, MLOAD,
    MSTORE, POP, PUSH0, RETURN, RETURNDATACOPY, RETURNDATASIZE, REVERT, SELFDESTRUCT, SSTORE,
    STATICCALL, STOP,
};

// =============================================================================
// Test Constants
// =============================================================================

/// Standard test caller address.
const TEST_CALLER: Address = address!("0000000000000000000000000000000000100000");

/// Large gas limit override for tests (10 billion gas).
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;

// =============================================================================
// Helper Functions
// =============================================================================

/// Calls the keyless deploy precompile and returns the result.
fn call_keyless_deploy(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_bytes: Bytes,
    gas_limit_override: u64,
    value: U256,
) -> ResultAndState<mega_evm::MegaHaltReason> {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: tx_bytes,
        gasLimitOverride: U256::from(gas_limit_override),
    }
    .abi_encode();

    transact(spec, db, TEST_CALLER, Some(KEYLESS_DEPLOY_ADDRESS), call_data.into(), value).unwrap()
}

/// Checks if two `KeylessDeployErrors` match, ignoring the halt reason for `ExecutionHalted`.
fn errors_match(a: &KeylessDeployError, b: &KeylessDeployError) -> bool {
    match (a, b) {
        // For ExecutionHalted, only compare gas_used (reason can't be recovered from ABI)
        (
            KeylessDeployError::ExecutionHalted { gas_used: a_gas, .. },
            KeylessDeployError::ExecutionHalted { gas_used: b_gas, .. },
        ) => a_gas == b_gas,
        // For all other variants, use standard equality
        _ => a == b,
    }
}

/// Asserts that the result is a revert with the expected error.
fn assert_revert_with_error(
    result: &ResultAndState<mega_evm::MegaHaltReason>,
    expected: KeylessDeployError,
) {
    match &result.result {
        ExecutionResult::Revert { output, .. } => {
            let error = decode_error_result(output)
                .unwrap_or_else(|| panic!("Failed to decode error from output: {:?}", output));
            assert!(errors_match(&error, &expected), "Expected {:?}, got {:?}", expected, error);
        }
        other => panic!("Expected Revert, got {:?}", other),
    }
}

/// Creates a pre-EIP-155 signed transaction with custom init code and value.
///
/// Uses a deterministic signature (r=s=0x2222...2222, v=27) similar to Nick's Method.
/// The recovered signer address will be deterministic based on the transaction content.
fn create_pre_eip155_deploy_tx_with_value(init_code: Bytes, value: U256) -> (Bytes, Address) {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000, // 100 gwei
        gas_limit: 1_000_000,       // 1M gas for more complex init codes
        to: TxKind::Create,
        value,
        input: init_code,
        chain_id: None, // Pre-EIP-155
    };

    // Use the same deterministic signature as Nick's Method (r=s=0x2222...2222)
    let r = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let s = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let sig = Signature::new(r, s, false); // v=27 means y_parity=false for pre-EIP-155

    // Create signed transaction
    let signed = Signed::new_unchecked(tx, sig, B256::ZERO);

    // RLP encode the signed transaction
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    let tx_bytes = Bytes::from(buf);

    // Recover the signer address
    let signer = signed.recover_signer().expect("should recover signer");

    (tx_bytes, signer)
}

/// Creates a pre-EIP-155 signed transaction with custom init code and zero value.
///
/// Uses a deterministic signature (r=s=0x2222...2222, v=27) similar to Nick's Method.
/// The recovered signer address will be deterministic based on the transaction content.
fn create_pre_eip155_deploy_tx(init_code: Bytes) -> (Bytes, Address) {
    create_pre_eip155_deploy_tx_with_value(init_code, U256::ZERO)
}

/// Calculate the deployed contract address for a keyless deploy transaction.
fn calculate_deploy_address_for_tx(tx_bytes: &[u8]) -> Address {
    use mega_evm::sandbox::{calculate_keyless_deploy_address, decode_keyless_tx, recover_signer};
    let signed = decode_keyless_tx(tx_bytes).expect("should decode tx");
    let signer = recover_signer(&signed).expect("should recover signer");
    calculate_keyless_deploy_address(signer)
}

// =============================================================================
// Success Tests
// =============================================================================

#[test]
fn test_keyless_deploy_eip1820() {
    let mut db = MemoryDatabase::default();

    // Fund deployer with enough ETH for gas (100 gwei * 10B gas = 1000 ETH)
    db.set_account_balance(EIP1820_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from(EIP1820_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Decode return value
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();

    // Verify deployed address matches expected EIP-1820 address
    assert_eq!(ret.deployedAddress, EIP1820_CONTRACT);
    assert!(ret.gasUsed > 0, "Expected non-zero gas usage");

    // Verify contract code exists at deployed address
    let contract = state.get(&EIP1820_CONTRACT).expect("contract should exist in state");
    assert!(
        contract.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "contract should have non-empty code"
    );
    assert_eq!(contract.info.code_hash, EIP1820_CODE_HASH, "contract code hash should match");
}

#[test]
fn test_keyless_deploy_create2_factory() {
    // Deploy the canonical CREATE2 factory
    let mut db = MemoryDatabase::default();

    // Fund deployer with enough ETH (100 gwei * 10B gas = 1000 ETH)
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Decode return value
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();

    // Verify deployed address matches expected CREATE2 factory address
    assert_eq!(ret.deployedAddress, CREATE2_FACTORY_CONTRACT);
    assert!(ret.gasUsed > 0, "Expected non-zero gas usage");

    // Verify contract code exists at deployed address
    let contract = state.get(&CREATE2_FACTORY_CONTRACT).expect("contract should exist in state");
    assert!(
        contract.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "contract should have non-empty code"
    );
    assert_eq!(
        contract.info.code_hash, CREATE2_FACTORY_CODE_HASH,
        "contract code hash should match"
    );
}

// =============================================================================
// Error Condition Tests
// =============================================================================

#[test]
fn test_keyless_deploy_rejects_ether_transfer() {
    // Calling with value > 0 should revert with NoEtherTransfer
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));
    // Fund caller so they can send ether
    db.set_account_balance(TEST_CALLER, U256::from(1_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::from(1), // Non-zero value
    );

    assert_revert_with_error(&result, KeylessDeployError::NoEtherTransfer);
}

#[test]
fn test_keyless_deploy_gas_limit_too_low() {
    // Using gasLimitOverride < tx.gas_limit should revert with GasLimitTooLow
    // CREATE2 factory tx has gas_limit = 100,000
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let too_low_gas_limit = 99_999; // < 100,000

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        too_low_gas_limit,
        U256::ZERO,
    );

    assert_revert_with_error(
        &result,
        KeylessDeployError::GasLimitTooLow {
            tx_gas_limit: 100_000,
            provided_gas_limit: too_low_gas_limit,
        },
    );
}

#[test]
fn test_keyless_deploy_insufficient_balance_zero() {
    // Signer has zero balance - should revert with InsufficientBalance
    let mut db = MemoryDatabase::default();
    // Don't fund the deployer at all

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    assert_revert_with_error(&result, KeylessDeployError::InsufficientBalance);
}

#[test]
fn test_keyless_deploy_insufficient_balance_partial() {
    // Signer has some balance but not enough for gas cost
    let mut db = MemoryDatabase::default();
    // Required: gas_price * gas_limit_override = 100 gwei * 10B = 1000 ETH
    // Fund with just 1 ETH (way less than needed)
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    assert_revert_with_error(&result, KeylessDeployError::InsufficientBalance);
}

#[test]
fn test_keyless_deploy_contract_already_exists() {
    // Pre-deploy code at target address - should revert with ContractAlreadyExists
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    // Pre-deploy some code at the target address
    db.set_account_code(CREATE2_FACTORY_CONTRACT, Bytes::from_static(&[0x60, 0x00]));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    assert_revert_with_error(&result, KeylessDeployError::ContractAlreadyExists);
}

#[test]
fn test_keyless_deploy_invalid_signature() {
    // Corrupt the signature's r value - should revert with InvalidSignature
    let mut db = MemoryDatabase::default();

    // CREATE2_FACTORY_TX structure:
    // Total length: 167 bytes
    // r value is at bytes 102-133 (32 bytes, after a0 prefix at byte 101)
    // s value is at bytes 135-166 (32 bytes, after a0 prefix at byte 134)
    //
    // Set r to all 0xFF to make signature recovery fail while keeping valid RLP
    let mut corrupted_tx = CREATE2_FACTORY_TX.to_vec();
    // r value is at offset 102, 32 bytes
    corrupted_tx[102..134].fill(0xff);

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from(corrupted_tx),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    assert_revert_with_error(&result, KeylessDeployError::InvalidSignature);
}

#[test]
fn test_keyless_deploy_malformed_encoding() {
    // Pass invalid RLP bytes - should revert with MalformedEncoding
    let mut db = MemoryDatabase::default();

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(&hex!("deadbeef")),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    assert_revert_with_error(&result, KeylessDeployError::MalformedEncoding);
}

#[test]
fn test_keyless_deploy_not_contract_creation() {
    // Use tx with `to` address - should revert with NotContractCreation
    let mut db = MemoryDatabase::default();

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(NON_CONTRACT_CREATION_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    assert_revert_with_error(&result, KeylessDeployError::NotContractCreation);
}

#[test]
fn test_keyless_deploy_not_pre_eip155() {
    // Use post-EIP-155 tx with chain_id - should revert with NotPreEIP155
    let mut db = MemoryDatabase::default();

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(POST_EIP155_CHAIN_1_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    assert_revert_with_error(&result, KeylessDeployError::NotPreEIP155);
}

#[test]
fn test_keyless_deploy_execution_reverted() {
    // Init code that calls REVERT - should revert with ExecutionReverted
    let mut db = MemoryDatabase::default();

    // Init code: PUSH1 0x00 PUSH1 0x00 REVERT
    // Bytecode: 60 00 60 00 fd (offset=0, size=0, REVERT)
    let init_code = Bytes::from_static(&hex!("60006000fd"));
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);

    // Fund the recovered signer
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify it's an ExecutionReverted error
    match &result.result {
        ExecutionResult::Revert { output, .. } => {
            let error = decode_error_result(output)
                .unwrap_or_else(|| panic!("Failed to decode error from output: {:?}", output));
            match error {
                KeylessDeployError::ExecutionReverted { gas_used, .. } => {
                    assert!(gas_used > 0, "Expected non-zero gas usage");
                }
                other => panic!("Expected ExecutionReverted, got {:?}", other),
            }
        }
        other => panic!("Expected Revert, got {:?}", other),
    }
}

#[test]
fn test_keyless_deploy_execution_halted_invalid_opcode() {
    // Init code with INVALID (0xfe) opcode - should revert with ExecutionHalted
    let mut db = MemoryDatabase::default();

    // Init code: INVALID
    // Bytecode: fe
    let init_code = Bytes::from_static(&hex!("fe"));
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);

    // Fund the recovered signer
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify it's an ExecutionHalted error
    match &result.result {
        ExecutionResult::Revert { output, .. } => {
            let error = decode_error_result(output)
                .unwrap_or_else(|| panic!("Failed to decode error from output: {:?}", output));
            match error {
                KeylessDeployError::ExecutionHalted { gas_used, .. } => {
                    assert!(gas_used > 0, "Expected non-zero gas usage");
                }
                other => panic!("Expected ExecutionHalted, got {:?}", other),
            }
        }
        other => panic!("Expected Revert, got {:?}", other),
    }
}

// =============================================================================
// Edge Case / Boundary Tests
// =============================================================================

#[test]
fn test_keyless_deploy_gas_limit_exactly_equal() {
    // gasLimitOverride == tx.gas_limit should succeed (boundary case)
    // CREATE2 factory tx has gas_limit = 100,000
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    // Use exactly the tx gas limit (100,000)
    // Note: This test may still fail if MegaETH's gas model needs more gas,
    // but at least we verify the gas limit check passes
    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        100_000, // Exactly matches tx gas_limit
        U256::ZERO,
    );

    // Should NOT be GasLimitTooLow error - may be ExecutionHalted if gas runs out
    // Success is also acceptable
    if let ExecutionResult::Revert { output, .. } = &result.result {
        let error = decode_error_result(output)
            .unwrap_or_else(|| panic!("Failed to decode error from output: {:?}", output));
        assert!(
            !matches!(error, KeylessDeployError::GasLimitTooLow { .. }),
            "Should not get GasLimitTooLow with exact gas limit, got: {:?}",
            error
        );
    }
}

#[test]
fn test_keyless_deploy_balance_exactly_sufficient() {
    // Signer balance = exact cost needed - should succeed
    // CREATE2 factory tx has gas_price = 100 gwei (100_000_000_000 wei)
    let mut db = MemoryDatabase::default();

    // Calculate exact required balance: gas_price * gas_limit_override
    // For CREATE2 factory: 100 gwei * 10B gas = 1000 ETH
    let gas_price = 100_000_000_000u64; // 100 gwei
    let exact_balance = U256::from(gas_price) * U256::from(LARGE_GAS_LIMIT_OVERRIDE);
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, exact_balance);

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Should succeed with exact balance
    let ResultAndState { result, .. } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success with exact balance, got: {:?}",
        result
    );
}

#[test]
fn test_keyless_deploy_nonce_override_to_zero() {
    // Even if the signer has a non-zero nonce, keyless deploy should override it to 0
    // so the contract deploys at the expected address (CREATE address depends on nonce)
    let mut db = MemoryDatabase::default();

    // Fund deployer with enough ETH
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    // Set a non-zero nonce for the deployer - if nonce isn't overridden,
    // the contract would deploy at a different address
    db.set_account_nonce(CREATE2_FACTORY_DEPLOYER, 100);

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Decode return value and verify the address matches the expected one
    // (which is computed with nonce=0)
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();
    assert_eq!(
        ret.deployedAddress, CREATE2_FACTORY_CONTRACT,
        "Contract should deploy at expected address (nonce=0)"
    );

    // Verify contract code exists at the expected address
    let contract = state.get(&CREATE2_FACTORY_CONTRACT).expect("contract should exist in state");
    assert!(
        contract.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "contract should have non-empty code"
    );
    assert_eq!(contract.info.code_hash, CREATE2_FACTORY_CODE_HASH);
}

// =============================================================================
// Spec Version Tests
// =============================================================================

#[test]
fn test_keyless_deploy_not_available_before_rex2() {
    // In REX1, the keyless deploy contract is not deployed at the address.
    // Calling an address with no code succeeds with empty output (no deployment occurs).
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX1, // Before REX2
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // In REX1, there's no code at the keyless deploy address, so the call succeeds
    // with empty output (calling an address with no code returns success).
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success (no-op) in REX1, got: {:?}",
        result
    );

    // Verify that the contract was NOT deployed (no code at target address)
    assert!(
        !state.contains_key(&CREATE2_FACTORY_CONTRACT),
        "Contract should NOT be deployed in REX1"
    );
}

#[test]
fn test_keyless_deploy_not_intercepted_for_inner_calls() {
    // Keyless deploy should only work for top-level calls (depth == 0).
    // When a contract calls keylessDeploy, it should NOT be intercepted and
    // the actual Solidity contract should execute, reverting with NotIntercepted.
    let mut db = MemoryDatabase::default();

    // Deploy the keyless deploy contract code at KEYLESS_DEPLOY_ADDRESS.
    // This is normally done by the system during block processing, but test_utils::transact
    // doesn't do this. For inner calls, the actual Solidity contract needs to exist.
    assert!(!KEYLESS_DEPLOY_CODE.is_empty(), "KEYLESS_DEPLOY_CODE should not be empty");
    db.set_account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE);

    // Create a proxy contract that calls KEYLESS_DEPLOY_ADDRESS and propagates the revert
    // using BytecodeBuilder for cleaner code generation.
    //
    // The proxy:
    // 1. Copies calldata to memory
    // 2. CALLs KEYLESS_DEPLOY_ADDRESS
    // 3. If call succeeds: returns the return data
    // 4. If call fails: reverts with the revert data
    let proxy_code = BytecodeBuilder::default()
        // Copy calldata to memory at offset 0
        .append(CALLDATASIZE)
        .append(PUSH0) // srcOffset
        .append(PUSH0) // destOffset
        .append(CALLDATACOPY)
        // CALL args: retSize=0, retOffset=0, argsSize=CALLDATASIZE, argsOffset=0, value=0, addr,
        // gas
        .append(PUSH0) // retSize
        .append(PUSH0) // retOffset
        .append(CALLDATASIZE) // argsSize
        .append(PUSH0) // argsOffset
        .append(PUSH0) // value
        .push_address(KEYLESS_DEPLOY_ADDRESS)
        .append(GAS)
        .append(CALL)
        // Stack now has: [success (0 or 1)]
        // If success == 0 (call failed), propagate the revert
        .append(ISZERO)
        .push_number(0x2b_u8) // jump to revert if failed (JUMPDEST is at offset 43 = 0x2b)
        .append(JUMPI)
        // Success path: copy return data and return (7 bytes: 0x26-0x2c)
        .append(RETURNDATASIZE)
        .append(PUSH0)
        .append(PUSH0)
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .append(PUSH0)
        .append(RETURN)
        // Revert path (at offset 0x2d): copy return data and revert
        .append(JUMPDEST) // JUMPDEST
        .append(RETURNDATASIZE)
        .append(PUSH0)
        .append(PUSH0)
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .append(PUSH0)
        .append(REVERT)
        .build();

    // Set up the proxy contract
    let proxy_address = address!("0000000000000000000000000000000000300000");
    db.set_account_code(proxy_address, proxy_code);

    // Fund accounts
    db.set_account_balance(TEST_CALLER, U256::from(1_000_000_000_000_000_000_000u128));
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    // Create the keylessDeploy calldata
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(CREATE2_FACTORY_TX),
        gasLimitOverride: U256::from(LARGE_GAS_LIMIT_OVERRIDE),
    }
    .abi_encode();

    // Call the proxy contract - the proxy will make an INNER call to keylessDeploy
    // Since depth > 0, it should NOT be intercepted, and the Solidity contract
    // should execute and revert with NotIntercepted
    let result = transact(
        MegaSpecId::REX2,
        &mut db,
        TEST_CALLER,
        Some(proxy_address),
        call_data.into(),
        U256::ZERO,
    )
    .unwrap();

    // The proxy should revert because the inner call to keylessDeploy reverted with NotIntercepted
    assert_revert_with_error(&result, KeylessDeployError::NotIntercepted);
}

/// Test that the keyless deploy contract is deployed when Rex2 activates via block executor.
/// This exercises the deployment logic in block/executor.rs `pre_execution_changes`.
#[test]
fn test_keyless_deploy_contract_deployed_on_rex2_activation() {
    use alloy_evm::{block::BlockExecutor, Evm, EvmEnv, EvmFactory};
    use alloy_hardforks::ForkCondition;
    use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
    use alloy_primitives::B256;
    use mega_evm::{
        BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaHardfork,
        MegaHardforkConfig, TestExternalEnvs, KEYLESS_DEPLOY_CODE_HASH,
    };
    use revm::{context::BlockEnv, database::State};

    // Create a fresh in-memory database
    let mut db = MemoryDatabase::default();
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory and environment
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment with Rex2 spec
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::REX2;
    let block_env = BlockEnv {
        number: revm::primitives::U256::from(1000),
        timestamp: revm::primitives::U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create the EVM instance
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block execution context
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Default::default(),
        BlockLimits::no_limits(),
    );

    // Configure hardforks with Rex2 activated at timestamp 0
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::Rex2, ForkCondition::Timestamp(0));

    // Create receipt builder
    let receipt_builder = OpAlloyReceiptBuilder::default();

    // Create block executor
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec.clone(), receipt_builder);

    // Call apply_pre_execution_changes which triggers keyless deploy contract deployment
    executor.apply_pre_execution_changes().expect("Pre-execution changes should succeed");

    // Verify the keyless deploy contract is deployed in the database
    let db_ref = executor.evm_mut().db_mut();

    // Load the keyless deploy contract account from the cache
    let cache_acc =
        db_ref.load_cache_account(KEYLESS_DEPLOY_ADDRESS).expect("Should be able to load account");
    let acc_info = cache_acc.account_info().expect("Keyless deploy contract account should exist");

    // Verify code hash matches
    assert_eq!(
        acc_info.code_hash, KEYLESS_DEPLOY_CODE_HASH,
        "Keyless deploy contract code hash should match"
    );

    // Verify code is set and matches
    assert!(acc_info.code.is_some(), "Code should be set on the account");
    let deployed_code = acc_info.code.as_ref().unwrap();
    assert_eq!(
        deployed_code.original_bytes(),
        KEYLESS_DEPLOY_CODE,
        "Deployed code should match original code"
    );

    // Verify that calling transact_deploy_keyless_deploy_contract again returns state
    // with account marked as read (proving the contract is already deployed)
    use mega_evm::transact_deploy_keyless_deploy_contract;
    let result = transact_deploy_keyless_deploy_contract(&chain_spec, 0, db_ref)
        .expect("Should not error")
        .unwrap();
    assert_eq!(
        result.len(),
        1,
        "Keyless deploy should already be deployed, so function should return state with account marked as read"
    );
}

// =============================================================================
// Corner Case Tests
// =============================================================================

#[test]
fn test_keyless_deploy_address_with_existing_balance() {
    // Test: Deploy address has ETH balance but no code - should succeed
    let mut db = MemoryDatabase::default();

    // Calculate expected deploy address for CREATE2_FACTORY_TX
    let deploy_address = calculate_deploy_address_for_tx(CREATE2_FACTORY_TX);
    assert_eq!(deploy_address, CREATE2_FACTORY_CONTRACT);

    // Pre-fund the deploy address with 1 ETH (no code)
    let pre_existing_balance = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
    db.set_account_balance(deploy_address, pre_existing_balance);

    // Fund deployer with enough ETH for gas
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Decode return value
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();
    assert_eq!(ret.deployedAddress, CREATE2_FACTORY_CONTRACT);

    // Verify contract code exists and balance is preserved
    let contract = state.get(&CREATE2_FACTORY_CONTRACT).expect("contract should exist in state");
    assert!(
        contract.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "contract should have non-empty code"
    );
    assert_eq!(
        contract.info.code_hash, CREATE2_FACTORY_CODE_HASH,
        "contract code hash should match"
    );
    assert_eq!(contract.info.balance, pre_existing_balance, "balance should be preserved");
}

#[test]
fn test_keyless_deploy_init_code_selfdestructs() {
    // Test: Init code calls SELFDESTRUCT
    //
    // EIP-6780 (Cancun): SELFDESTRUCT destroys the account only if it was created in
    // the same transaction. Since we're self-destructing during CREATE (same tx),
    // the account will be destroyed at the end of the transaction.
    //
    // Key behavior:
    // - CREATE still returns the allocated address (not zero)
    // - The contract's ETH is sent to the beneficiary
    // - The contract code is deleted (account destroyed)
    let mut db = MemoryDatabase::default();

    let beneficiary = address!("0000000000000000000000000000000000beeef1");
    let contract_value = U256::from(1_000_000_000_000_000_000u128); // 1 ETH

    // Init code that:
    // 1. SELFDESTRUCTs to beneficiary (sends contract's ETH)
    // 2. Execution continues (post-Cancun SELFDESTRUCT doesn't halt)
    // 3. Returns minimal runtime code
    //
    // Note: Even though init code returns runtime code, SELFDESTRUCT in same-tx
    // causes the account to be destroyed, so the code won't persist.
    let init_code = BytecodeBuilder::default()
        .push_address(beneficiary)
        .append(SELFDESTRUCT)
        // Runtime code comes after SELFDESTRUCT + return sequence = 22 bytes (0x16)
        .push_number(1_u8) // size = 1
        .push_number(0x1a_u8) // offset of runtime code
        .push_number(0_u8) // dest offset
        .append(CODECOPY)
        .push_number(1_u8) // return size
        .push_number(0_u8) // return offset
        .append(RETURN)
        .append(STOP) // runtime code: STOP
        .build();

    // Create tx with value so the contract has ETH to send via SELFDESTRUCT
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx_with_value(init_code, contract_value);

    // Fund signer with enough for gas + value
    db.set_account_balance(signer, U256::from(1_001_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success (keylessDeploy succeeds even if contract self-destructs)
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Get deployed address from return value
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();

    // CREATE returns the allocated address even when SELFDESTRUCT is called
    assert_ne!(
        ret.deployedAddress,
        Address::ZERO,
        "CREATE should return allocated address even with SELFDESTRUCT"
    );

    // EIP-6780: Beneficiary receives the contract's ETH via SELFDESTRUCT
    let beneficiary_acc = state.get(&beneficiary).expect("beneficiary should be in state");
    assert_eq!(
        beneficiary_acc.info.balance, contract_value,
        "beneficiary should receive the contract's ETH via SELFDESTRUCT"
    );

    // EIP-6780: Contract is destroyed because SELFDESTRUCT was called in same tx as creation
    // The account may exist in state but should have no code
    let has_code = state
        .get(&ret.deployedAddress)
        .and_then(|acc| acc.info.code.as_ref())
        .map(|c| !c.is_empty())
        .unwrap_or(false);
    assert!(!has_code, "contract should have no code after SELFDESTRUCT in same-tx (EIP-6780)");
}

#[test]
fn test_keyless_deploy_modifies_other_contract_state() {
    // Test: Init code writes to another contract's storage via CALL
    let mut db = MemoryDatabase::default();

    // Deploy a "storage" contract that stores calldata[0:32] to slot 0
    // Bytecode:
    // PUSH1 0x20 PUSH1 0x00 PUSH1 0x00 CALLDATACOPY  ; copy 32 bytes from calldata to memory
    // PUSH1 0x00 MLOAD                               ; load value from memory
    // PUSH1 0x00 SSTORE                              ; store to slot 0
    // STOP
    let storage_contract_code = BytecodeBuilder::default()
        .push_number(0x20_u8) // size
        .push_number(0_u8) // srcOffset
        .push_number(0_u8) // destOffset
        .append(CALLDATACOPY)
        .push_number(0_u8) // offset
        .append(MLOAD)
        .push_number(0_u8) // slot
        .append(SSTORE)
        .stop()
        .build();

    let storage_contract = address!("0000000000000000000000000000000000500000");
    db.set_account_code(storage_contract, storage_contract_code);

    // Value to store
    let test_value = U256::from(0x1234_5678_9abc_def0u64);

    // Init code that:
    // 1. Stores test_value to memory at offset 0
    // 2. CALLs storage_contract with the value as calldata
    // 3. Returns minimal runtime code
    let init_code = BytecodeBuilder::default()
        .mstore(0, test_value.to_be_bytes_vec())
        .push_number(0_u8) // retSize
        .push_number(0_u8) // retOffset
        .push_number(0x20_u8) // argsSize = 32
        .push_number(0_u8) // argsOffset
        .push_number(0_u8) // value
        .push_address(storage_contract)
        .append(GAS)
        .append(CALL)
        .append(POP)
        // Return minimal runtime code: just return a constant 0x42
        .push_number(1_u8) // size = 1
        .push_number(0x49_u8) // offset of runtime code (0x49 = 73)
        .push_number(0_u8) // dest offset
        .append(CODECOPY)
        .push_number(1_u8) // return size
        .push_number(0_u8) // return offset
        .append(RETURN)
        .append(STOP) // runtime code: STOP
        .build();

    let (tx_bytes, signer) = create_pre_eip155_deploy_tx_with_value(init_code, U256::ZERO);

    // Fund signer
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Decode return value
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();

    // Verify deployed contract code and code hash
    let contract = state.get(&ret.deployedAddress).expect("contract should exist in state");
    let code = contract.info.code.as_ref().expect("contract should have code");
    assert!(!code.is_empty(), "contract should have non-empty code");

    // Verify storage contract's slot 0 contains the test value
    let storage_acc = state.get(&storage_contract).expect("storage contract should be in state");
    let stored_value = storage_acc.storage.get(&U256::ZERO).expect("slot 0 should exist");
    assert_eq!(stored_value.present_value, test_value, "storage should contain test value");
}

#[test]
fn test_keyless_deploy_creates_child_contract() {
    // Test: Init code deploys a child contract via CREATE
    let mut db = MemoryDatabase::default();

    // Child contract runtime code: just STOP
    let child_runtime_code: &[u8] = &[0x00]; // STOP

    // Child init code that returns the runtime code
    let child_init_code = BytecodeBuilder::default()
        .push_number(1_u8) // size = 1 (runtime code size)
        .push_number(0x0c_u8) // offset of runtime code
        .push_number(0_u8) // dest = 0
        .append(CODECOPY)
        .push_number(1_u8) // return size
        .push_number(0_u8) // return offset
        .append(RETURN)
        .append_many(child_runtime_code.iter().copied())
        .build();

    let child_init_len = child_init_code.len();

    // Parent init code:
    // 1. Store child init code to memory
    // 2. CREATE child contract with value=0
    // 3. Store child address to storage slot 0 (so we can verify it)
    // 4. Return parent runtime code
    let mut parent_init = BytecodeBuilder::default();

    // Store child init code to memory at offset 0 using mstore utility
    parent_init = parent_init.mstore(0, &child_init_code);

    // CREATE: value=0, offset=0, size=child_init_len
    parent_init = parent_init.push_number(child_init_len as u8); // size
    parent_init = parent_init.push_number(0_u8); // offset
    parent_init = parent_init.push_number(0_u8); // value
    parent_init = parent_init.append(CREATE);

    // Stack now has child address, store to slot 0
    parent_init = parent_init.push_number(0_u8); // slot
    parent_init = parent_init.append(SSTORE);

    // Now calculate offset for runtime code
    let current_len = parent_init.len();
    // We'll add: PUSH1 0x01, PUSH1 <offset>, PUSH1 0x00, CODECOPY, PUSH1 0x01, PUSH1 0x00, RETURN
    // That's 2 + 2 + 2 + 1 + 2 + 2 + 1 = 12 bytes, then 1 byte for runtime code
    let runtime_offset = current_len + 12;

    parent_init = parent_init
        .push_number(1_u8) // size = 1
        .push_number(runtime_offset as u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .append(STOP); // runtime code: STOP

    let init_code = parent_init.build();
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx_with_value(init_code, U256::ZERO);

    // Fund signer
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Get parent address
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();
    let parent_address = ret.deployedAddress;

    // Both parent and child have runtime code 0x00 (STOP)
    let runtime_code: &[u8] = &[0x00];
    let expected_code_hash = keccak256(runtime_code);

    // Verify parent contract code and code hash
    let parent = state.get(&parent_address).expect("parent contract should exist");
    assert!(
        parent.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "parent should have code"
    );
    assert_eq!(parent.info.code_hash, expected_code_hash, "parent code hash should match");

    // Get child address from parent's storage slot 0
    let child_address_slot = parent.storage.get(&U256::ZERO).expect("slot 0 should exist");
    let child_address = Address::from_word(child_address_slot.present_value.into());

    // Verify child contract code and code hash
    let child = state.get(&child_address).expect("child contract should exist in state");
    assert!(
        child.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "child should have code"
    );
    assert_eq!(child.info.code_hash, expected_code_hash, "child code hash should match");
}

#[test]
fn test_keyless_deploy_with_value_transfer() {
    // Test: Keyless tx with non-zero value transfers to contract
    let mut db = MemoryDatabase::default();

    // Simple init code that just returns minimal runtime code
    let init_code = BytecodeBuilder::default()
        .push_number(1_u8) // size = 1
        .push_number(0x0c_u8) // offset of runtime code
        .push_number(0_u8) // dest = 0
        .append(CODECOPY)
        .push_number(1_u8) // return size
        .push_number(0_u8) // return offset
        .append(RETURN)
        .append(STOP) // runtime code: STOP
        .build();

    // Create tx with 0.5 ETH value
    let transfer_value = U256::from(500_000_000_000_000_000u128); // 0.5 ETH
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx_with_value(init_code, transfer_value);

    // Fund signer with enough for gas + value
    // gas_price * gas_limit_override + value = 100 gwei * 10B + 0.5 ETH = 1000 ETH + 0.5 ETH
    let signer_balance = U256::from(1_000_500_000_000_000_000_000u128);
    db.set_account_balance(signer, signer_balance);

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Get deployed address
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();

    // Verify deployed contract has code, code hash, and balance
    let runtime_code: &[u8] = &[0x00]; // STOP
    let expected_code_hash = keccak256(runtime_code);
    let contract = state.get(&ret.deployedAddress).expect("contract should exist");
    assert!(
        contract.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "contract should have non-empty code"
    );
    assert_eq!(contract.info.code_hash, expected_code_hash, "contract code hash should match");
    assert_eq!(contract.info.balance, transfer_value, "contract should have transferred value");
}

#[test]
fn test_keyless_deploy_emits_logs() {
    // Test that logs emitted during sandbox execution are propagated to the parent context
    let mut db = MemoryDatabase::default();

    // Init code that:
    // 1. Stores some data to memory
    // 2. Emits LOG0 with that data
    // 3. Returns minimal runtime code
    let log_data = 0xdeadbeef_u32;

    let init_code = BytecodeBuilder::default()
        // Store log data to memory (left-padded in 32 bytes via push_bytes + MSTORE)
        .push_bytes(log_data.to_be_bytes())
        .push_number(0_u8)
        .append(MSTORE)
        // LOG0(offset=28, size=4) - read the last 4 bytes of the 32-byte word
        .push_number(4_u8) // size = 4
        .push_number(0x1c_u8) // offset = 28 (32 - 4)
        .append(LOG0)
        // Return minimal runtime code
        .push_number(1_u8) // size = 1
        .push_number(0x14_u8) // offset of runtime code
        .push_number(0_u8) // dest = 0
        .append(CODECOPY)
        .push_number(1_u8) // return size
        .push_number(0_u8) // return offset
        .append(RETURN)
        .append(STOP) // runtime code: STOP
        .build();

    let (tx_bytes, signer) = create_pre_eip155_deploy_tx_with_value(init_code, U256::ZERO);

    // Fund signer
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify deployment succeeds
    let ResultAndState { result, state: _ } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Verify logs are propagated from sandbox
    let logs = result.logs();
    assert_eq!(logs.len(), 1, "Expected exactly one log from sandbox");

    // Verify log data matches what init code emitted
    let log = &logs[0];
    assert_eq!(log.data.data.as_ref(), &log_data.to_be_bytes(), "Log data should match");
    assert!(log.data.topics().is_empty(), "LOG0 should have no topics");
}

#[test]
fn test_keyless_deploy_reads_existing_contract() {
    // Test: Init code reads from a pre-deployed contract via STATICCALL
    let mut db = MemoryDatabase::default();

    // Deploy a "getter" contract that returns a constant value when called
    // The value is left-padded to 32 bytes (standard EVM word format)
    let return_value = 0x12345678_u32;
    let getter_code = BytecodeBuilder::default()
        .push_bytes(return_value.to_be_bytes())
        .push_number(0_u8)
        .append(MSTORE)
        .push_number(0x20_u8) // return 32 bytes
        .push_number(0_u8)
        .append(RETURN)
        .build();

    let getter_contract = address!("0000000000000000000000000000000000600000");
    db.set_account_code(getter_contract, getter_code);

    // Init code that:
    // 1. STATICCALLs getter contract
    // 2. Stores the return value to slot 0
    // 3. Returns minimal runtime code
    let init_code = BytecodeBuilder::default()
        // STATICCALL: gas, addr, argsOffset, argsSize, retOffset, retSize
        .push_number(0x20_u8) // retSize = 32
        .push_number(0_u8) // retOffset = 0
        .push_number(0_u8) // argsSize = 0
        .push_number(0_u8) // argsOffset = 0
        .push_address(getter_contract)
        .append(GAS)
        .append(STATICCALL)
        .append(POP) // POP success flag
        // Load return value from memory and store to slot 0
        .push_number(0_u8)
        .append(MLOAD)
        .push_number(0_u8) // slot
        .append(SSTORE)
        // Return minimal runtime code
        .push_number(1_u8) // size = 1
        .push_number(0x2a_u8) // offset of runtime code
        .push_number(0_u8) // dest = 0
        .append(CODECOPY)
        .push_number(1_u8) // return size
        .push_number(0_u8) // return offset
        .append(RETURN)
        .append(STOP) // runtime code: STOP
        .build();

    let (tx_bytes, signer) = create_pre_eip155_deploy_tx_with_value(init_code, U256::ZERO);

    // Fund signer
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000_000u128));

    let result = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    // Verify success
    let ResultAndState { result, state } = result;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "Expected success, got: {:?}",
        result
    );

    // Get deployed address
    let output = result.output().unwrap();
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output).unwrap();

    // Verify deployed contract's code and code hash
    let contract = state.get(&ret.deployedAddress).expect("contract should exist");
    let code = contract.info.code.as_ref().expect("contract should have code");
    assert!(!code.is_empty(), "contract should have non-empty code");

    // Verify deployed contract's storage slot 0 has the value from getter
    let stored = contract.storage.get(&U256::ZERO).expect("slot 0 should exist");
    // The getter returns a 32-byte value with our u32 right-justified (standard EVM behavior)
    let expected = U256::from(return_value);
    assert_eq!(stored.present_value, expected, "storage should have value from getter");
}

#[test]
fn test_keyless_deploy_twice_fails_second_time() {
    // Test: Deploy same tx twice - second fails with ContractAlreadyExists
    let mut db = MemoryDatabase::default();

    // Fund deployer with enough ETH for multiple deployments
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(2_000_000_000_000_000_000_000u128));

    // First deployment should succeed
    let result1 = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    let ResultAndState { result, state } = result1;
    assert!(
        matches!(result, ExecutionResult::Success { .. }),
        "First deployment should succeed, got: {:?}",
        result
    );

    // Verify the first deployment produced correct code and code hash
    let contract = state.get(&CREATE2_FACTORY_CONTRACT).expect("contract should exist");
    let code = contract.info.code.as_ref().expect("contract should have code");
    assert!(!code.is_empty(), "contract should have non-empty code");
    assert_eq!(
        contract.info.code_hash, CREATE2_FACTORY_CODE_HASH,
        "contract code hash should match"
    );

    // Apply only the contract code from first deployment to the database.
    // We specifically apply the code at CREATE2_FACTORY_CONTRACT to simulate
    // the contract existing, without modifying nonce which would cause NonceTooLow.
    db.set_account_code(CREATE2_FACTORY_CONTRACT, code.original_bytes());

    // Second deployment should fail with ContractAlreadyExists
    let result2 = call_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
        U256::ZERO,
    );

    assert_revert_with_error(&result2, KeylessDeployError::ContractAlreadyExists);
}
