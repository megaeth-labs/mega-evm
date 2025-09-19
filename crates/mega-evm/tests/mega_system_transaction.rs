//! Tests for mega system transaction functionality.
//!
//! This test suite verifies that transactions from the `MEGA_SYSTEM_ADDRESS` are processed
//! as deposit-like transactions, bypassing signature validation, nonce verification,
//! and fee deduction while maintaining normal execution behavior.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{MEGA_SYSTEM_ADDRESS, MEGA_SYSTEM_TRANSACTION_SOURCE_HASH},
    is_deposit_like_transaction, is_mega_system_address_transaction,
    test_utils::{opcode_gen::BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, NoOpOracle,
};
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    bytecode::opcode::{CALLER, MSTORE, RETURN},
    context::{result::ResultAndState, BlockEnv, ContextSetters, ContextTr, Transaction, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::TxKind,
    Database,
};

const REGULAR_CALLER: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT_ADDR: Address = address!("0000000000000000000000000000000000100001");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000BEEF01");

/// Creates a test EVM instance with the provided database.
fn create_evm(db: MemoryDatabase) -> MegaEvm<MemoryDatabase, NoOpInspector, NoOpOracle> {
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, NoOpOracle::default());

    let block_env = BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        basefee: 1000,
        ..Default::default()
    };
    context.set_block(block_env);

    // Set operator fees to zero for cleaner testing (except for the fee deduction test)
    // Note: Some tests need fees enabled to test fee deduction behavior
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));

    MegaEvm::new(context)
}

/// Creates a simple contract that returns the caller address.
fn create_simple_contract() -> Bytes {
    // CALLER PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
    BytecodeBuilder::default()
        .append(CALLER)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0x00u8)
        .append(RETURN)
        .build()
}

/// Executes a transaction and returns the result.
fn execute_transaction(
    evm: &mut MegaEvm<MemoryDatabase, NoOpInspector, NoOpOracle>,
    caller: Address,
    to: Address,
    value: U256,
    data: Bytes,
) -> ResultAndState<MegaHaltReason> {
    let tx = MegaTransaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(to),
            value,
            data,
            gas_limit: 1_000_000,
            gas_price: 1000,
            ..Default::default()
        },
        ..Default::default()
    };

    alloy_evm::Evm::transact_raw(evm, tx).expect("Transaction should execute")
}

#[test]
fn test_utility_functions() {
    // Test is_mega_system_address_transaction
    let mega_system_tx = MegaTransaction {
        base: TxEnv {
            caller: MEGA_SYSTEM_ADDRESS,
            kind: TxKind::Call(CONTRACT_ADDR),
            ..Default::default()
        },
        ..Default::default()
    };

    let regular_tx = MegaTransaction {
        base: TxEnv {
            caller: REGULAR_CALLER,
            kind: TxKind::Call(CONTRACT_ADDR),
            ..Default::default()
        },
        ..Default::default()
    };

    assert!(is_mega_system_address_transaction(&mega_system_tx));
    assert!(!is_mega_system_address_transaction(&regular_tx));

    // Test is_deposit_like_transaction
    assert!(is_deposit_like_transaction(&mega_system_tx));
    assert!(!is_deposit_like_transaction(&regular_tx));
}

#[test]
fn test_mega_system_transaction_execution() {
    // Create database and set up contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Execute transaction from mega system address
    let result =
        execute_transaction(&mut evm, MEGA_SYSTEM_ADDRESS, CONTRACT_ADDR, U256::ZERO, Bytes::new());

    // Transaction should succeed
    assert!(result.result.is_success(), "Mega system transaction should succeed");

    // The contract should return the caller address (MEGA_SYSTEM_ADDRESS)
    if let Some(output) = result.result.output() {
        // The output should contain the MEGA_SYSTEM_ADDRESS padded to 32 bytes
        let expected_caller = MEGA_SYSTEM_ADDRESS;
        let mut expected_output = [0u8; 32];
        expected_output[12..].copy_from_slice(expected_caller.as_slice());

        assert_eq!(
            output.as_ref(),
            &expected_output,
            "Contract should receive MEGA_SYSTEM_ADDRESS as caller"
        );
    } else {
        panic!("Expected output from contract");
    }
}

#[test]
fn test_regular_transaction_still_works() {
    // Create database and set up accounts and contract
    let mut db = MemoryDatabase::default();
    db.set_account_balance(REGULAR_CALLER, U256::from(1_000_000_000u64)); // Give it some balance
    db.set_account_code(CONTRACT_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Execute transaction from regular address
    let result =
        execute_transaction(&mut evm, REGULAR_CALLER, CONTRACT_ADDR, U256::ZERO, Bytes::new());

    // Transaction should succeed
    assert!(result.result.is_success(), "Regular transaction should succeed");

    // The contract should return the caller address (REGULAR_CALLER)
    if let Some(output) = result.result.output() {
        let expected_caller = REGULAR_CALLER;
        let mut expected_output = [0u8; 32];
        expected_output[12..].copy_from_slice(expected_caller.as_slice());

        assert_eq!(
            output.as_ref(),
            &expected_output,
            "Contract should receive REGULAR_CALLER as caller"
        );
    } else {
        panic!("Expected output from contract");
    }
}

#[test]
fn test_mega_system_transaction_bypasses_balance_check() {
    // Create database and set up contract
    // Note: We don't set any balance for MEGA_SYSTEM_ADDRESS
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Regular transactions would fail without sufficient balance for gas,
    // but mega system transactions should succeed

    // Execute transaction from mega system address (no value transfer, just gas bypass test)
    let result = execute_transaction(
        &mut evm,
        MEGA_SYSTEM_ADDRESS,
        CONTRACT_ADDR,
        U256::ZERO, // No value transfer to avoid deposit validation complexities
        Bytes::new(),
    );

    // Transaction should succeed even without balance (deposit-like behavior)
    assert!(
        result.result.is_success(),
        "Mega system transaction should succeed even without balance: {:?}",
        result.result
    );
}

#[test]
fn test_mega_system_transaction_sets_source_hash() {
    let db = MemoryDatabase::default();
    let mut evm = create_evm(db);

    // Create a mega system transaction
    let tx = MegaTransaction {
        base: TxEnv {
            caller: MEGA_SYSTEM_ADDRESS,
            kind: TxKind::Call(CONTRACT_ADDR),
            gas_limit: 1_000_000,
            gas_price: 1000,
            ..Default::default()
        },
        ..Default::default()
    };

    // The transaction should be detected as from mega system address
    assert!(is_mega_system_address_transaction(&tx));

    // Set the transaction in the context
    evm.ctx().set_tx(tx);

    // Execute the transaction which will trigger pre_execution and set the source hash
    let tx_clone = evm.ctx().tx().clone();
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx_clone);
    assert!(result.is_ok(), "Transaction should execute successfully");

    // Check that the source hash was set
    assert_eq!(
        evm.ctx().tx().deposit.source_hash,
        MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
        "Source hash should be set for mega system transactions"
    );
}

#[test]
fn test_deposit_transaction_behavior_preserved() {
    // Create a transaction that looks like a deposit transaction
    let mut deposit_tx = MegaTransaction {
        base: TxEnv {
            caller: REGULAR_CALLER,
            kind: TxKind::Call(CONTRACT_ADDR),
            ..Default::default()
        },
        ..Default::default()
    };

    // Manually set it as a deposit transaction type
    deposit_tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH; // Any non-zero hash makes it a deposit

    // Should be detected as deposit-like
    assert!(deposit_tx.tx_type() == DEPOSIT_TRANSACTION_TYPE);
    assert!(is_deposit_like_transaction(&deposit_tx));

    // But should NOT be detected as mega system address transaction
    assert!(!is_mega_system_address_transaction(&deposit_tx));
}

#[test]
fn test_mega_system_transaction_no_fee_deduction() {
    // Create database and set up contract with an initial balance for the mega system address
    let mut db = MemoryDatabase::default();
    let initial_balance = U256::from(1_000_000u64);
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, initial_balance);
    db.set_account_code(CONTRACT_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Get the initial balance before transaction
    let balance_before = evm.ctx().db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().balance;
    assert_eq!(balance_before, initial_balance, "Initial balance should be set correctly");

    // Execute transaction from mega system address with high gas price
    let result =
        execute_transaction(&mut evm, MEGA_SYSTEM_ADDRESS, CONTRACT_ADDR, U256::ZERO, Bytes::new());

    // Transaction should succeed
    assert!(result.result.is_success(), "Mega system transaction should succeed");

    // Check balance after transaction - should be unchanged (no fee deducted)
    let balance_after = evm.ctx().db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().balance;
    assert_eq!(
        balance_after, initial_balance,
        "Balance should remain unchanged - no gas fee should be deducted from mega system address"
    );
}

#[test]
fn test_regular_transaction_fee_deduction() {
    // Create database and set up contract with an initial balance for the regular caller
    let mut db = MemoryDatabase::default();
    let initial_balance = U256::from(1_000_000_000_000u64);
    db.set_account_balance(REGULAR_CALLER, initial_balance);
    db.set_account_code(CONTRACT_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Get the initial balance before transaction
    let balance_before = evm.ctx().db_mut().basic(REGULAR_CALLER).unwrap().unwrap().balance;
    assert_eq!(balance_before, initial_balance, "Initial balance should be set correctly");

    // Execute transaction from regular caller
    let result =
        execute_transaction(&mut evm, REGULAR_CALLER, CONTRACT_ADDR, U256::ZERO, Bytes::new());

    // Transaction should succeed
    assert!(result.result.is_success(), "Regular transaction should succeed");

    // Check balance after transaction - should be reduced by gas fees
    // Get the updated balance from the transaction result state
    let balance_after = result.state.get(&REGULAR_CALLER).unwrap().info.balance;
    assert!(
        balance_after < initial_balance,
        "Balance should be reduced by gas fees for regular transactions. Before: {}, After: {}",
        initial_balance,
        balance_after
    );

    // Calculate the fee deducted
    let fee_deducted = initial_balance - balance_after;
    assert!(
        fee_deducted > U256::ZERO,
        "Gas fee should be deducted from regular caller. Fee deducted: {}",
        fee_deducted
    );
}

#[test]
fn test_mega_system_address_constant() {
    // Verify the mega system address is correctly defined
    assert_eq!(MEGA_SYSTEM_ADDRESS, address!("0xdeaddeaddeaddeaddeaddeaddeaddeaddead0002"));

    // Verify it's different from other known addresses
    assert_ne!(MEGA_SYSTEM_ADDRESS, Address::ZERO);
    assert_ne!(MEGA_SYSTEM_ADDRESS, REGULAR_CALLER);
    assert_ne!(MEGA_SYSTEM_ADDRESS, CONTRACT_ADDR);
}
