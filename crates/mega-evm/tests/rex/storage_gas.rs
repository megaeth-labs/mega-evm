//! Tests for REX hardfork storage gas costs.

use std::convert::Infallible;

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    address_to_bucket_id,
    constants::{self, rex::*},
    slot_to_bucket_id,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, EVMError, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionError,
};
use revm::{
    context::{result::ResultAndState, TxEnv},
    primitives::Address,
};
use salt::constant::MIN_BUCKET_SIZE;

pub const CALLER: Address = address!("2000000000000000000000000000000000000002");
pub const CALLEE: Address = address!("1000000000000000000000000000000000000001");
pub const NEW_ACCOUNT: Address = address!("3000000000000000000000000000000000000003");

/// Base intrinsic gas cost for all transactions
const BASE_INTRINSIC_GAS: u64 = 21_000;
const BASE_CREATE_GAS: u64 = 32_000;

/// Executes a transaction on the EVM.
#[allow(clippy::too_many_arguments)]
pub fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: &DefaultExternalEnvs,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
    gas_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec, external_envs);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

#[test]
fn test_sstore_minimum_bucket_zero_gas() {
    // With multiplier = 1 (minimum bucket), REX should charge 0 gas for SSTORE
    // Formula: 20,000 × (1 - 1) = 0
    let mut db = MemoryDatabase::default();

    let storage_key = U256::ZERO;
    let storage_value = U256::from(0x42);
    let bucket_id = slot_to_bucket_id(CALLEE, storage_key);

    // Deploy contract with SSTORE operation
    let bytecode = BytecodeBuilder::default().sstore(storage_key, storage_value).stop().build();

    db.set_account_balance(CALLER, U256::from(100_000_000_000u64));
    db.set_account_code(CALLEE, bytecode);

    let external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, MIN_BUCKET_SIZE as u64); // multiplier = 1

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO,
        100_000,
    )
    .expect("Transaction should succeed");

    // Gas breakdown:
    // - 21,000 (base intrinsic)
    // - 39,000 (TX_INTRINSIC_STORAGE_GAS)
    // - 22,100 (SSTORE cold access + first-time set in EVM)
    // - 0 (REX storage gas with multiplier = 1)
    // Total: ~82,200

    let gas_used = result.result.gas_used();
    assert_eq!(
        gas_used,
        BASE_INTRINSIC_GAS +
            constants::rex::TX_INTRINSIC_STORAGE_GAS +
            constants::equivalence::SSTORE_SET +
            constants::equivalence::COLD_SLOAD_COST +
            6, // bytecode overhead
    );
}

#[test]
fn test_sstore_with_multiplier_charges_storage_gas() {
    // With multiplier > 1, REX should charge storage gas
    // Formula: 20,000 × (multiplier - 1)
    let mut db = MemoryDatabase::default();

    let storage_key = U256::ZERO;
    let storage_value = U256::from(0x42);
    let bucket_id = slot_to_bucket_id(CALLEE, storage_key);

    let bytecode = BytecodeBuilder::default().sstore(storage_key, storage_value).stop().build();

    db.set_account_balance(CALLER, U256::from(100_000_000_000u64));
    db.set_account_code(CALLEE, bytecode);

    // Test with multiplier = 2 (bucket_capacity = 2 * MIN_BUCKET_SIZE)
    let bucket_capacity = MIN_BUCKET_SIZE as u64 * 2;
    let external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, bucket_capacity);

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO,
        200_000,
    )
    .expect("Transaction should succeed");

    // Expected storage gas: 20,000 × (2 - 1) = 20,000
    let gas_used = result.result.gas_used();
    let expected_storage_gas = SSTORE_SET_STORAGE_GAS_BASE * (2 - 1);

    assert_eq!(
        gas_used,
        BASE_INTRINSIC_GAS
            + constants::rex::TX_INTRINSIC_STORAGE_GAS
            + constants::equivalence::SSTORE_SET
            + constants::equivalence::COLD_SLOAD_COST
            + 6 // bytecode overhead
            + expected_storage_gas,
    );
}

#[test]
fn test_sstore_multiplier_scaling() {
    // Test that storage gas scales linearly with multiplier
    let multipliers = vec![1u64, 2, 5, 10];
    let mut previous_gas = 0u64;

    for multiplier in multipliers {
        let mut db = MemoryDatabase::default();

        let storage_key = U256::ZERO;
        let storage_value = U256::from(0x42);
        let bucket_id = slot_to_bucket_id(CALLEE, storage_key);

        let bytecode = BytecodeBuilder::default().sstore(storage_key, storage_value).stop().build();

        db.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));
        db.set_account_code(CALLEE, bytecode);

        let bucket_capacity = MIN_BUCKET_SIZE as u64 * multiplier;
        let external_envs =
            DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, bucket_capacity);

        let result = transact(
            MegaSpecId::REX,
            &mut db,
            &external_envs,
            CALLER,
            Some(CALLEE),
            Bytes::new(),
            U256::ZERO,
            10_000_000,
        )
        .expect("Transaction should succeed");

        let gas_used = result.result.gas_used();
        let expected_storage_gas = SSTORE_SET_STORAGE_GAS_BASE * (multiplier - 1);
        let expected_total = BASE_INTRINSIC_GAS
            + constants::rex::TX_INTRINSIC_STORAGE_GAS
            + constants::equivalence::SSTORE_SET
            + constants::equivalence::COLD_SLOAD_COST
            + 6 // bytecode overhead
            + expected_storage_gas;

        assert_eq!(gas_used, expected_total);

        if previous_gas > 0 {
            // Verify gas increased from previous
            assert!(gas_used > previous_gas, "Gas should increase with higher multiplier");
        }

        previous_gas = gas_used;
    }
}

#[test]
fn test_sstore_reset_no_storage_gas() {
    // SSTORE reset (non-zero → non-zero) should not charge REX storage gas
    let mut db = MemoryDatabase::default();

    let storage_key = U256::ZERO;
    let bucket_id = slot_to_bucket_id(CALLEE, storage_key);

    // Bytecode that sets a value then resets it to different non-zero value
    let bytecode = BytecodeBuilder::default()
        // First SSTORE: 0 → 0x42
        .sstore(storage_key, U256::from(0x42))
        // Second SSTORE: 0x42 → 0x99 (reset, not initial set)
        .sstore(storage_key, U256::from(0x99))
        .stop()
        .build();

    db.set_account_balance(CALLER, U256::from(100_000_000_000u64));
    db.set_account_code(CALLEE, bytecode);

    let bucket_capacity = MIN_BUCKET_SIZE as u64 * 10; // High multiplier
    let external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, bucket_capacity);

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO,
        10_000_000,
    )
    .expect("Transaction should succeed");

    // Should only charge storage gas for the first SSTORE (0 → non-zero)
    // Expected: 20,000 × (10 - 1) = 180,000 for first SSTORE only
    let expected_single_sstore_gas = SSTORE_SET_STORAGE_GAS_BASE * (10 - 1);

    // Gas breakdown:
    // - Base intrinsic + TX_INTRINSIC_STORAGE_GAS
    // - First SSTORE: EVM gas + storage gas
    // - Second SSTORE: warm storage read (no storage gas, no reset cost)
    let gas_used = result.result.gas_used();
    let expected_total = BASE_INTRINSIC_GAS
        + constants::rex::TX_INTRINSIC_STORAGE_GAS
        + constants::equivalence::SSTORE_SET
        + constants::equivalence::COLD_SLOAD_COST
        + 12 // bytecode overhead (two SSTOREs)
        + expected_single_sstore_gas
        + constants::equivalence::WARM_STORAGE_READ_COST;

    assert_eq!(gas_used, expected_total, "Should only charge storage gas once for first SSTORE");
}

#[test]
fn test_new_account_minimum_bucket_zero_gas() {
    // With multiplier = 1, account creation should charge 0 storage gas
    // Formula: 25,000 × (1 - 1) = 0
    let mut db = MemoryDatabase::default();

    db.set_account_balance(CALLER, U256::from(100_000_000_000u64));

    let bucket_id = address_to_bucket_id(NEW_ACCOUNT);
    let external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, MIN_BUCKET_SIZE as u64); // multiplier = 1

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(NEW_ACCOUNT),
        Bytes::new(),
        U256::from(1_000), // Send value to create new account
        100_000,
    )
    .expect("Transaction should succeed");

    // Gas breakdown:
    // - 21,000 (base intrinsic)
    // - 39,000 (TX_INTRINSIC_STORAGE_GAS)
    // - 0 (REX account creation with multiplier = 1)
    // Total: 60,000

    assert_eq!(result.result.gas_used(), 60_000);
}

#[test]
fn test_new_account_with_multiplier() {
    // Test account creation with various multipliers
    // Formula: 25,000 × (multiplier - 1)
    let multipliers = vec![1u64, 2, 5, 10];

    for multiplier in multipliers {
        let mut db = MemoryDatabase::default();

        db.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));

        let bucket_id = address_to_bucket_id(NEW_ACCOUNT);
        let bucket_capacity = MIN_BUCKET_SIZE as u64 * multiplier;
        let external_envs =
            DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, bucket_capacity);

        let result = transact(
            MegaSpecId::REX,
            &mut db,
            &external_envs,
            CALLER,
            Some(NEW_ACCOUNT),
            Bytes::new(),
            U256::from(1_000),
            10_000_000,
        )
        .expect("Transaction should succeed");

        let expected_storage_gas = NEW_ACCOUNT_STORAGE_GAS_BASE * (multiplier - 1);
        let expected_total = 21_000 + TX_INTRINSIC_STORAGE_GAS + expected_storage_gas;

        assert_eq!(result.result.gas_used(), expected_total);
    }
}

#[test]
fn test_existing_account_no_storage_gas() {
    // Transfer to existing account should not charge storage gas
    let mut db = MemoryDatabase::default();

    db.set_account_balance(CALLER, U256::from(100_000_000_000u64));
    // Pre-create the account
    db.set_account_balance(NEW_ACCOUNT, U256::from(1_000));

    let bucket_id = address_to_bucket_id(NEW_ACCOUNT);
    let bucket_capacity = MIN_BUCKET_SIZE as u64 * 10; // High multiplier
    let external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, bucket_capacity);

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(NEW_ACCOUNT),
        Bytes::new(),
        U256::from(1_000),
        100_000,
    )
    .expect("Transaction should succeed");

    // Should only charge intrinsic gas, no storage gas for existing account
    let expected_total = 21_000 + TX_INTRINSIC_STORAGE_GAS;

    assert_eq!(result.result.gas_used(), expected_total);
}

#[test]
fn test_contract_creation_minimum_bucket() {
    // Contract creation with multiplier = 1 should charge 0 storage gas
    // Formula: 32,000 × (1 - 1) = 0
    let mut db = MemoryDatabase::default();

    db.set_account_balance(CALLER, U256::from(100_000_000_000u64));

    let deployed_bytecode = BytecodeBuilder::default().stop().build();

    // For CREATE transactions, we need to know the address that will be created
    // The created address is deterministic based on caller + nonce
    // For this test, we'll use a simplified approach and set a wide bucket capacity
    let external_envs = DefaultExternalEnvs::default();

    // CREATE transaction
    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        None, // None = CREATE
        deployed_bytecode,
        U256::ZERO,
        200_000,
    )
    .expect("Transaction should succeed");

    // Should not charge REX contract creation storage gas with minimum bucket (default env has
    // MIN_BUCKET_SIZE)
    let expected_gas = BASE_INTRINSIC_GAS + BASE_CREATE_GAS + constants::rex::TX_INTRINSIC_STORAGE_GAS + 46; // bytecode overhead
    assert_eq!(result.result.gas_used(), expected_gas);
}

#[test]
fn test_contract_creation_with_multiplier() {
    // Test contract creation with various multipliers
    // Formula: 32,000 × (multiplier - 1)
    let multipliers = vec![2u64, 5, 10];

    for multiplier in multipliers {
        let mut db = MemoryDatabase::default();

        db.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));

        let deployed_bytecode = BytecodeBuilder::default().stop().build();

        // Calculate the deterministic contract address (RLP(sender, nonce))
        // For nonce=0: keccak256(rlp([sender, 0]))[12:]
        let created_address = CALLER.create(0);
        let bucket_id = address_to_bucket_id(created_address);
        let bucket_capacity = MIN_BUCKET_SIZE as u64 * multiplier;

        let external_envs =
            DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, bucket_capacity);

        let result = transact(
            MegaSpecId::REX,
            &mut db,
            &external_envs,
            CALLER,
            None,
            deployed_bytecode,
            U256::ZERO,
            10_000_000,
        )
        .expect("Transaction should succeed");

        let expected_storage_gas = CONTRACT_CREATION_STORAGE_GAS_BASE * (multiplier - 1);
        let gas_used = result.result.gas_used();

        // Gas breakdown for CREATE:
        // Base overhead (including intrinsic, CREATE opcode, code deposit, etc.) = 92,046
        // REX contract creation storage gas = 32,000 × (multiplier - 1)
        let base_create_gas = 92_046;
        let expected_total = base_create_gas + expected_storage_gas;

        assert_eq!(gas_used, expected_total);
    }
}

#[test]
fn test_contract_creation_costs_more_than_account() {
    // REX distinguishes contract creation (32k base) from account creation (25k base)
    // Mini-Rex used same 2M base for both
    let mut db_contract = MemoryDatabase::default();
    let mut db_account = MemoryDatabase::default();

    db_contract.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));
    db_account.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));

    let multiplier = 10u64;
    let bucket_capacity = MIN_BUCKET_SIZE as u64 * multiplier;

    // Contract creation
    let created_address = CALLER.create(0);
    let contract_bucket_id = address_to_bucket_id(created_address);
    let contract_external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(contract_bucket_id, 0, bucket_capacity);

    let deployed_bytecode = BytecodeBuilder::default().stop().build();
    let contract_result = transact(
        MegaSpecId::REX,
        &mut db_contract,
        &contract_external_envs,
        CALLER,
        None,
        deployed_bytecode,
        U256::ZERO,
        10_000_000,
    )
    .expect("Contract creation should succeed");

    // Account creation
    let account_bucket_id = address_to_bucket_id(NEW_ACCOUNT);
    let account_external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(account_bucket_id, 0, bucket_capacity);

    let account_result = transact(
        MegaSpecId::REX,
        &mut db_account,
        &account_external_envs,
        CALLER,
        Some(NEW_ACCOUNT),
        Bytes::new(),
        U256::from(1_000),
        10_000_000,
    )
    .expect("Account creation should succeed");

    let contract_gas = contract_result.result.gas_used();
    let account_gas = account_result.result.gas_used();

    // Expected difference in storage gas: (32,000 - 25,000) × (10 - 1) = 63,000
    let _expected_difference =
        (CONTRACT_CREATION_STORAGE_GAS_BASE - NEW_ACCOUNT_STORAGE_GAS_BASE) * (multiplier - 1);

    assert!(
        contract_gas > account_gas,
        "Contract creation should cost more than account creation in REX"
    );
}

#[test]
fn test_combined_contract_creation_and_sstore() {
    // Test that contract creation + SSTORE both charge storage gas
    let mut db = MemoryDatabase::default();

    db.set_account_balance(CALLER, U256::from(1_000_000_000_000u64));

    // Constructor that does SSTORE
    let storage_key = U256::ZERO;
    let storage_value = U256::from(0x42);
    let deployed_bytecode =
        BytecodeBuilder::default().sstore(storage_key, storage_value).stop().build();

    let multiplier = 5u64;
    let bucket_capacity = MIN_BUCKET_SIZE as u64 * multiplier;

    // Get both bucket IDs
    let created_address = CALLER.create(0);
    let contract_bucket_id = address_to_bucket_id(created_address);
    let storage_bucket_id = slot_to_bucket_id(created_address, storage_key);

    let external_envs = DefaultExternalEnvs::new()
        .with_bucket_capacity(contract_bucket_id, 0, bucket_capacity)
        .with_bucket_capacity(storage_bucket_id, 0, bucket_capacity);

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        None,
        deployed_bytecode,
        U256::ZERO,
        10_000_000,
    )
    .expect("Transaction should succeed");

    // Expected storage gas:
    // - Contract creation: 32,000 × (5 - 1) = 128,000
    // - SSTORE: 20,000 × (5 - 1) = 80,000
    // Total storage gas: 208,000
    let expected_storage_gas = CONTRACT_CREATION_STORAGE_GAS_BASE * (multiplier - 1) +
        SSTORE_SET_STORAGE_GAS_BASE * (multiplier - 1);

    let gas_used = result.result.gas_used();

    // Gas breakdown for CREATE with SSTORE:
    // Base overhead (including intrinsic, CREATE, SSTORE, code deposit, etc.) = 117,632
    // REX storage gas = 32,000 × (mult - 1) + 20,000 × (mult - 1) = 208,000
    let base_create_sstore_gas = 117_632;
    let expected_total = base_create_sstore_gas + expected_storage_gas;

    assert_eq!(gas_used, expected_total);
}

#[test]
fn test_rex_vs_minirex_comparison() {
    // Compare REX vs Mini-Rex storage gas costs
    // Mini-Rex: 2,000,000 × multiplier
    // REX: base × (multiplier - 1) where base is 20k/25k/32k

    let mut db_rex = MemoryDatabase::default();
    let mut db_minirex = MemoryDatabase::default();

    db_rex.set_account_balance(CALLER, U256::from(10_000_000_000_000u64));
    db_minirex.set_account_balance(CALLER, U256::from(10_000_000_000_000u64));

    let multiplier = 10u64;
    let bucket_capacity = MIN_BUCKET_SIZE as u64 * multiplier;

    let bucket_id = address_to_bucket_id(NEW_ACCOUNT);
    let external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, bucket_capacity);

    // Account creation comparison
    let rex_result = transact(
        MegaSpecId::REX,
        &mut db_rex,
        &external_envs,
        CALLER,
        Some(NEW_ACCOUNT),
        Bytes::new(),
        U256::from(1_000),
        100_000_000,
    )
    .expect("REX transaction should succeed");

    let minirex_result = transact(
        MegaSpecId::MINI_REX,
        &mut db_minirex,
        &external_envs,
        CALLER,
        Some(NEW_ACCOUNT),
        Bytes::new(),
        U256::from(1_000),
        100_000_000,
    )
    .expect("Mini-Rex transaction should succeed");

    // REX should charge less for dynamic operations but has higher intrinsic
    // REX: 21k + 39k + 25k×(10-1) = 285k
    // Mini-Rex: 21k + 0 + 2M×10 = 20,021k
    assert!(
        rex_result.result.gas_used() < minirex_result.result.gas_used(),
        "REX should charge less total gas than Mini-Rex for account creation"
    );
}

#[test]
fn test_large_multiplier_linear_scaling() {
    // Verify storage gas scales linearly even with large multipliers
    let mut db = MemoryDatabase::default();

    db.set_account_balance(CALLER, U256::from(100_000_000_000_000u64));

    let multiplier = 100u64;
    let bucket_capacity = MIN_BUCKET_SIZE as u64 * multiplier;

    let bucket_id = address_to_bucket_id(NEW_ACCOUNT);
    let external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, bucket_capacity);

    let result = transact(
        MegaSpecId::REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(NEW_ACCOUNT),
        Bytes::new(),
        U256::from(1_000),
        100_000_000,
    )
    .expect("Transaction should succeed");

    // Expected: 21k + 39k + 25k×(100-1) = 21k + 39k + 2,475k = 2,535k
    let expected_storage_gas = NEW_ACCOUNT_STORAGE_GAS_BASE * (multiplier - 1);
    let expected_total = 21_000 + TX_INTRINSIC_STORAGE_GAS + expected_storage_gas;

    assert_eq!(result.result.gas_used(), expected_total);
}
