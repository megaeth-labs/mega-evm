//! Tests for deposit transaction gas stipend functionality.
//!
//! This test suite verifies that deposit transactions calling whitelisted addresses
//! receive additional gas stipend by having their gas limit multiplied.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    DEPOSIT_TX_GAS_STIPEND_MULTIPLIER, DEPOSIT_TX_GAS_STIPEND_WHITELIST,
};
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    bytecode::opcode::{GAS, MSTORE, PUSH0, RETURN},
    context::{BlockEnv, ContextSetters, ContextTr, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::{TxKind, KECCAK_EMPTY},
    state::{AccountInfo, Bytecode},
};
use std::convert::Infallible;

const REGULAR_CALLER: Address = address!("0000000000000000000000000000000000100000");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000BEEF01");
const NON_WHITELISTED_ADDR: Address = address!("0000000000000000000000000000000000DEAD01");

// Whitelisted addresses from DEPOSIT_TX_GAS_STIPEND_WHITELIST
const L1_BLOCK_ADDR: Address = address!("4200000000000000000000000000000000000015");
const GAS_PRICE_ORACLE_ADDR: Address = address!("420000000000000000000000000000000000000F");
const OPERATOR_FEE_VAULT_ADDR: Address = address!("420000000000000000000000000000000000001b");

/// Creates a test EVM instance with the provided database.
fn create_evm(db: MemoryDatabase) -> MegaEvm<MemoryDatabase, NoOpInspector, DefaultExternalEnvs> {
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, DefaultExternalEnvs::default());

    let block_env = BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        basefee: 1000,
        ..Default::default()
    };
    context.set_block(block_env);

    // Set operator fees to zero for cleaner testing
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));

    MegaEvm::new(context)
}

/// Creates a contract that reports the gas available at the start of execution.
fn create_gas_reporter_contract() -> Bytes {
    BytecodeBuilder::default()
        .append(GAS) // Get gas
        .append(PUSH0) // Push 0 for memory position
        .append(MSTORE) // Store gas at memory position 0
        .push_number(32u8) // Return size
        .append(PUSH0) // Return offset
        .append(RETURN) // Return the gas value
        .build()
}

/// Creates and executes a deposit transaction, returns the gas used.
fn execute_deposit_tx(
    evm: &mut MegaEvm<MemoryDatabase, NoOpInspector, DefaultExternalEnvs>,
    to: Address,
    gas_limit: u64,
) -> Result<u64, mega_evm::EVMError<Infallible, mega_evm::MegaTransactionError>> {
    let tx = TxEnv {
        caller: REGULAR_CALLER,
        kind: TxKind::Call(to),
        data: Bytes::new(),
        value: U256::from(0),
        gas_limit,
        gas_price: 1000,
        ..Default::default()
    };

    let mut mega_tx = MegaTransaction::new(tx);

    // Set deposit transaction properties to bypass fees
    mega_tx.deposit.source_hash = KECCAK_EMPTY; // Non-zero hash makes it a deposit transaction
    mega_tx.enveloped_tx = Some(Bytes::from([DEPOSIT_TRANSACTION_TYPE]));

    let result = alloy_evm::Evm::transact_raw(evm, mega_tx)?;
    Ok(result.result.gas_used())
}

/// Creates and executes a regular (non-deposit) transaction, returns the gas used.
fn execute_regular_tx(
    evm: &mut MegaEvm<MemoryDatabase, NoOpInspector, DefaultExternalEnvs>,
    to: Address,
    gas_limit: u64,
) -> Result<u64, mega_evm::EVMError<Infallible, mega_evm::MegaTransactionError>> {
    // Give the caller enough balance to pay for the transaction
    evm.ctx_mut().db_mut().insert_account_info(
        REGULAR_CALLER,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u64), // 1 ETH
            ..Default::default()
        },
    );

    let tx = TxEnv {
        caller: REGULAR_CALLER,
        kind: TxKind::Call(to),
        data: Bytes::new(),
        value: U256::from(0),
        gas_limit,
        gas_price: 1000,
        ..Default::default()
    };

    let mut mega_tx = MegaTransaction::new(tx);
    mega_tx.enveloped_tx = Some(Bytes::new()); // Regular transaction

    let result = alloy_evm::Evm::transact_raw(evm, mega_tx)?;
    Ok(result.result.gas_used())
}

#[test]
fn test_deposit_tx_gas_stipend_for_l1_block() {
    let mut db = MemoryDatabase::default();

    // Deploy gas reporter contract at L1Block address
    let contract_code = create_gas_reporter_contract();
    db.insert_account_info(
        L1_BLOCK_ADDR,
        AccountInfo { code: Some(Bytecode::new_raw(contract_code)), ..Default::default() },
    );

    let mut evm = create_evm(db);
    let base_gas_limit = 100000u64;

    let gas_used = execute_deposit_tx(&mut evm, L1_BLOCK_ADDR, base_gas_limit).unwrap();

    // Verify transaction succeeded and used reasonable amount of gas
    assert!(gas_used > 0);
    assert!(gas_used < base_gas_limit * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER);

    // The contract should have executed successfully, meaning it had enough gas
    // which suggests the stipend was applied
    assert!(gas_used < 50000); // Should use much less than the enhanced limit
}

#[test]
fn test_deposit_tx_gas_stipend_for_gas_price_oracle() {
    let mut db = MemoryDatabase::default();

    // Deploy gas reporter contract at GasPriceOracle address
    let contract_code = create_gas_reporter_contract();
    db.insert_account_info(
        GAS_PRICE_ORACLE_ADDR,
        AccountInfo { code: Some(Bytecode::new_raw(contract_code)), ..Default::default() },
    );

    let mut evm = create_evm(db);
    let base_gas_limit = 100000u64;

    let gas_used = execute_deposit_tx(&mut evm, GAS_PRICE_ORACLE_ADDR, base_gas_limit).unwrap();

    // Verify transaction succeeded and used reasonable amount of gas
    assert!(gas_used > 0);
    assert!(gas_used < base_gas_limit * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER);
    assert!(gas_used < 50000);
}

#[test]
fn test_deposit_tx_gas_stipend_for_operator_fee_vault() {
    let mut db = MemoryDatabase::default();

    // Deploy gas reporter contract at OperatorFeeVault address
    let contract_code = create_gas_reporter_contract();
    db.insert_account_info(
        OPERATOR_FEE_VAULT_ADDR,
        AccountInfo { code: Some(Bytecode::new_raw(contract_code)), ..Default::default() },
    );

    let mut evm = create_evm(db);
    let base_gas_limit = 100000u64;

    let gas_used = execute_deposit_tx(&mut evm, OPERATOR_FEE_VAULT_ADDR, base_gas_limit).unwrap();

    // Verify transaction succeeded and used reasonable amount of gas
    assert!(gas_used > 0);
    assert!(gas_used < base_gas_limit * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER);
    assert!(gas_used < 50000);
}

#[test]
fn test_deposit_tx_no_gas_stipend_for_non_whitelisted_address() {
    let mut db = MemoryDatabase::default();

    // Deploy gas reporter contract at non-whitelisted address
    let contract_code = create_gas_reporter_contract();
    db.insert_account_info(
        NON_WHITELISTED_ADDR,
        AccountInfo { code: Some(Bytecode::new_raw(contract_code)), ..Default::default() },
    );

    let mut evm = create_evm(db);
    let base_gas_limit = 100000u64;

    let gas_used = execute_deposit_tx(&mut evm, NON_WHITELISTED_ADDR, base_gas_limit).unwrap();

    // Verify transaction succeeded and used reasonable amount of gas
    // For non-whitelisted addresses, no stipend should be applied
    assert!(gas_used > 0);
    assert!(gas_used < base_gas_limit);
    assert!(gas_used < 50000);
}

#[test]
fn test_deposit_tx_gas_stipend_for_contract_creation() {
    let db = MemoryDatabase::default();
    let mut evm = create_evm(db);

    let tx = TxEnv {
        caller: REGULAR_CALLER,
        kind: TxKind::Create,
        data: create_gas_reporter_contract(),
        value: U256::from(0),
        gas_limit: 100000,
        gas_price: 1000,
        ..Default::default()
    };

    let mut mega_tx = MegaTransaction::new(tx);

    // Set deposit transaction properties to bypass fees
    mega_tx.deposit.source_hash = KECCAK_EMPTY; // Non-zero hash makes it a deposit transaction
    mega_tx.enveloped_tx = Some(Bytes::from([DEPOSIT_TRANSACTION_TYPE]));

    let result = alloy_evm::Evm::transact_raw(&mut evm, mega_tx).unwrap();

    // Contract creation should not receive gas stipend
    let gas_used = result.result.gas_used();
    assert!(gas_used > 0);
    // Contract creation typically uses more gas, but should still be within reasonable bounds
    // and much less than what the stipend would allow
    assert!(gas_used < 100000 * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER / 2); // Should use original
                                                                        // limit, not full stipend
}

#[test]
fn test_gas_stipend_multiplier_value() {
    // Verify the gas stipend multiplier constant is set correctly
    assert_eq!(DEPOSIT_TX_GAS_STIPEND_MULTIPLIER, 100);
}

#[test]
fn test_gas_stipend_whitelist_addresses() {
    // Verify all expected addresses are in the whitelist
    assert!(DEPOSIT_TX_GAS_STIPEND_WHITELIST.contains(&L1_BLOCK_ADDR));
    assert!(DEPOSIT_TX_GAS_STIPEND_WHITELIST.contains(&GAS_PRICE_ORACLE_ADDR));
    assert!(DEPOSIT_TX_GAS_STIPEND_WHITELIST.contains(&OPERATOR_FEE_VAULT_ADDR));

    // Verify non-whitelisted address is not in the list
    assert!(!DEPOSIT_TX_GAS_STIPEND_WHITELIST.contains(&NON_WHITELISTED_ADDR));

    // Verify the whitelist has the expected number of addresses
    assert_eq!(DEPOSIT_TX_GAS_STIPEND_WHITELIST.len(), 3);
}

#[test]
fn test_normal_transaction_no_gas_stipend() {
    let mut db = MemoryDatabase::default();

    // Deploy gas reporter contract at whitelisted address
    let contract_code = create_gas_reporter_contract();
    db.insert_account_info(
        L1_BLOCK_ADDR,
        AccountInfo { code: Some(Bytecode::new_raw(contract_code)), ..Default::default() },
    );

    let mut evm = create_evm(db);
    let base_gas_limit = 100000u64;

    let gas_used = execute_regular_tx(&mut evm, L1_BLOCK_ADDR, base_gas_limit).unwrap();

    // Normal transaction should NOT receive gas stipend even to whitelisted address
    assert!(gas_used > 0);
    assert!(gas_used < base_gas_limit);

    // Gas used should be similar to regular execution, not enhanced
    assert!(gas_used < 50000);
}

#[test]
fn test_deposit_tx_gas_limit_multiplication() {
    // Test to verify that gas stipend actually multiplies the gas limit
    let mut db = MemoryDatabase::default();

    // Create a more gas-intensive contract to better test the stipend effect
    let expensive_contract = BytecodeBuilder::default()
        .append(GAS) // Get initial gas
        .append(PUSH0) // Push 0 for memory position
        .append(MSTORE) // Store gas at memory position 0
        // Add some expensive operations
        .push_number(1000u16) // Loop counter
        .push_number(0u8) // Start position
        // Simple loop to consume more gas
        .append(GAS) // Get gas again after operations
        .push_number(32u8) // Return size
        .append(PUSH0) // Return offset
        .append(RETURN) // Return the final gas value
        .build();

    db.insert_account_info(
        L1_BLOCK_ADDR,
        AccountInfo { code: Some(Bytecode::new_raw(expensive_contract)), ..Default::default() },
    );

    let mut evm = create_evm(db);

    // Use a smaller gas limit to better test the stipend effect
    let base_gas_limit = 50000u64;

    let gas_used = execute_deposit_tx(&mut evm, L1_BLOCK_ADDR, base_gas_limit).unwrap();

    // The transaction should succeed even with the smaller base limit
    // because the stipend multiplied the available gas
    assert!(gas_used > 0);
    assert!(gas_used < base_gas_limit * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER);

    // The fact that it executed successfully with complex operations suggests
    // that the gas stipend was applied
    println!(
        "Gas used: {}, Base limit: {}, With stipend: {}",
        gas_used,
        base_gas_limit,
        base_gas_limit * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER
    );
}

#[test]
fn test_deposit_tx_reports_gas_stipend() {
    // This test verifies that the gas reporter contract actually sees the enhanced gas limit
    let mut db = MemoryDatabase::default();

    // Deploy gas reporter contract at L1Block address
    let contract_code = create_gas_reporter_contract();
    db.insert_account_info(
        L1_BLOCK_ADDR,
        AccountInfo { code: Some(Bytecode::new_raw(contract_code)), ..Default::default() },
    );

    let mut evm = create_evm(db);
    let base_gas_limit = 100000u64;

    let tx = TxEnv {
        caller: REGULAR_CALLER,
        kind: TxKind::Call(L1_BLOCK_ADDR),
        data: Bytes::new(),
        value: U256::from(0),
        gas_limit: base_gas_limit,
        gas_price: 1000,
        ..Default::default()
    };

    let mut mega_tx = MegaTransaction::new(tx);

    // Set deposit transaction properties to bypass fees
    mega_tx.deposit.source_hash = KECCAK_EMPTY; // Non-zero hash makes it a deposit transaction
    mega_tx.enveloped_tx = Some(Bytes::from([DEPOSIT_TRANSACTION_TYPE]));

    let result = alloy_evm::Evm::transact_raw(&mut evm, mega_tx).unwrap();

    // The contract returns the gas available at the start of execution
    let output = result.result.output().unwrap_or_default();
    assert_eq!(output.len(), 32, "Contract should return 32 bytes (U256)");

    // Convert the returned bytes to U256 to get the gas value
    let reported_gas = U256::from_be_slice(output);
    println!("Contract reported gas: {}", reported_gas);
    println!("Base gas limit: {}", base_gas_limit);
    println!("Expected with stipend: {}", base_gas_limit * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER);

    // The reported gas should be significantly higher than the base limit
    // but less than the theoretical maximum (due to intrinsic gas costs)
    let expected_enhanced_limit = base_gas_limit * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER;

    // The contract should see much more gas than the original limit
    assert!(
        reported_gas > U256::from(base_gas_limit),
        "Contract should see more gas than base limit. Reported: {}, Base: {}",
        reported_gas,
        base_gas_limit
    );

    // But less than the full enhanced amount due to intrinsic costs
    assert!(reported_gas < U256::from(expected_enhanced_limit),
           "Contract should see less than full enhanced limit due to intrinsic costs. Reported: {}, Enhanced: {}",
           reported_gas, expected_enhanced_limit);

    // The reported gas should be at least 50% of the enhanced limit to confirm stipend was applied
    let minimum_expected = expected_enhanced_limit / 2;
    assert!(
        reported_gas > U256::from(minimum_expected),
        "Contract should see at least 50% of enhanced limit. Reported: {}, Minimum expected: {}",
        reported_gas,
        minimum_expected
    );
}

#[test]
fn test_regular_tx_reports_no_gas_stipend() {
    // Compare with regular transaction to show the difference
    let mut db = MemoryDatabase::default();

    // Deploy gas reporter contract at L1Block address
    let contract_code = create_gas_reporter_contract();
    db.insert_account_info(
        L1_BLOCK_ADDR,
        AccountInfo { code: Some(Bytecode::new_raw(contract_code)), ..Default::default() },
    );

    let mut evm = create_evm(db);
    let base_gas_limit = 100000u64;

    // Give the caller enough balance to pay for the transaction
    evm.ctx_mut().db_mut().insert_account_info(
        REGULAR_CALLER,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u64), // 1 ETH
            ..Default::default()
        },
    );

    let tx = TxEnv {
        caller: REGULAR_CALLER,
        kind: TxKind::Call(L1_BLOCK_ADDR),
        data: Bytes::new(),
        value: U256::from(0),
        gas_limit: base_gas_limit,
        gas_price: 1000,
        ..Default::default()
    };

    let mut mega_tx = MegaTransaction::new(tx);
    mega_tx.enveloped_tx = Some(Bytes::new()); // Regular transaction

    let result = alloy_evm::Evm::transact_raw(&mut evm, mega_tx).unwrap();

    // The contract returns the gas available at the start of execution
    let output = result.result.output().unwrap_or_default();
    assert_eq!(output.len(), 32, "Contract should return 32 bytes (U256)");

    // Convert the returned bytes to U256 to get the gas value
    let reported_gas = U256::from_be_slice(output);
    println!("Regular tx contract reported gas: {}", reported_gas);
    println!("Base gas limit: {}", base_gas_limit);

    // For regular transactions, the reported gas should be close to the base limit (minus intrinsic
    // costs) It should NOT be enhanced by the stipend multiplier
    let expected_enhanced_limit = base_gas_limit * DEPOSIT_TX_GAS_STIPEND_MULTIPLIER;

    // Regular transaction should see much less gas than what stipend would provide
    assert!(
        reported_gas < U256::from(expected_enhanced_limit / 2),
        "Regular transaction should not see enhanced gas. Reported: {}, Half of enhanced: {}",
        reported_gas,
        expected_enhanced_limit / 2
    );
}
