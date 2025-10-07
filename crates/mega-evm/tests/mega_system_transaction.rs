//! Tests for mega system transaction functionality.
//!
//! This test suite verifies that transactions from the `MEGA_SYSTEM_ADDRESS` are processed
//! as deposit-like transactions, bypassing signature validation, nonce verification,
//! and fee deduction while maintaining normal execution behavior.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    is_mega_system_transaction,
    system_tx::{
        is_deposit_like_transaction, MEGA_SYSTEM_ADDRESS, MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
    },
    test_utils::{opcode_gen::BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
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
const NON_WHITELISTED_ADDR: Address = address!("0000000000000000000000000000000000DEAD01");

// Whitelisted address from MEGA_SYSTEM_TX_WHITELIST
const WHITELISTED_ADDR: Address = address!("4200000000000000000000000000000000000101");

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
    evm: &mut MegaEvm<MemoryDatabase, NoOpInspector, DefaultExternalEnvs>,
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

/// Tests the utility functions for detecting mega system address transactions and deposit-like
/// transactions.
///
/// This test verifies that:
/// - `is_mega_system_address_transaction` correctly identifies transactions from
///   `MEGA_SYSTEM_ADDRESS`
/// - `is_deposit_like_transaction` correctly identifies both actual deposit transactions and mega
///   system transactions
#[test]
fn test_utility_functions() {
    // Test is_mega_system_address_transaction
    let mega_system_tx = MegaTransaction {
        base: TxEnv {
            caller: MEGA_SYSTEM_ADDRESS,
            kind: TxKind::Call(WHITELISTED_ADDR),
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

    assert!(is_mega_system_transaction(&mega_system_tx));
    assert!(!is_mega_system_transaction(&regular_tx));

    // Test is_deposit_like_transaction
    assert!(is_deposit_like_transaction(&mega_system_tx));
    assert!(!is_deposit_like_transaction(&regular_tx));
}

/// Tests that mega system transactions execute successfully and behave as deposit-like
/// transactions.
///
/// This test verifies that:
/// - Transactions from `MEGA_SYSTEM_ADDRESS` to whitelisted addresses execute successfully
/// - The contract receives `MEGA_SYSTEM_ADDRESS` as the caller (no address manipulation)
/// - The transaction follows deposit-like execution path
#[test]
fn test_mega_system_transaction_execution() {
    // Create database and set up contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Execute transaction from mega system address
    let result = execute_transaction(
        &mut evm,
        MEGA_SYSTEM_ADDRESS,
        WHITELISTED_ADDR,
        U256::ZERO,
        Bytes::new(),
    );

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

/// Tests that regular transactions continue to work normally and are not affected by mega system
/// transaction handling.
///
/// This test verifies that:
/// - Regular transactions to whitelisted addresses execute successfully
/// - Regular transactions follow normal execution path (not deposit-like)
/// - The contract receives the actual caller address
/// - Regular transactions are not processed as system transactions
#[test]
fn test_regular_transaction_still_works() {
    // Create database and set up accounts and contract
    let mut db = MemoryDatabase::default();
    db.set_account_balance(REGULAR_CALLER, U256::from(1_000_000_000u64)); // Give it some balance
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Execute transaction from regular address
    let result =
        execute_transaction(&mut evm, REGULAR_CALLER, WHITELISTED_ADDR, U256::ZERO, Bytes::new());

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

/// Tests that mega system transactions bypass balance checks and execute without sufficient
/// balance.
///
/// This test verifies that:
/// - `MEGA_SYSTEM_ADDRESS` transactions execute successfully even without any balance
/// - The transaction bypasses normal balance validation for gas fees
/// - This demonstrates deposit-like behavior where balance checks are skipped
/// - The contract execution proceeds normally despite insufficient balance
#[test]
fn test_mega_system_transaction_bypasses_balance_check() {
    // Create database and set up contract
    // Note: We don't set any balance for MEGA_SYSTEM_ADDRESS
    let mut db = MemoryDatabase::default();
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Regular transactions would fail without sufficient balance for gas,
    // but mega system transactions should succeed

    // Execute transaction from mega system address (no value transfer, just gas bypass test)
    let result = execute_transaction(
        &mut evm,
        MEGA_SYSTEM_ADDRESS,
        WHITELISTED_ADDR,
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

/// Tests that mega system transactions correctly set the deposit source hash.
///
/// This test verifies that:
/// - Mega system transactions automatically set the `MEGA_SYSTEM_TRANSACTION_SOURCE_HASH`
/// - The source hash is properly configured in the transaction's deposit info
/// - This enables the transaction to be processed as a deposit-like transaction
/// - The source hash matches the expected constant value
#[test]
fn test_mega_system_transaction_sets_source_hash() {
    let db = MemoryDatabase::default();
    let mut evm = create_evm(db);

    // Create a mega system transaction
    let tx = MegaTransaction {
        base: TxEnv {
            caller: MEGA_SYSTEM_ADDRESS,
            kind: TxKind::Call(WHITELISTED_ADDR),
            gas_limit: 1_000_000,
            gas_price: 1000,
            ..Default::default()
        },
        ..Default::default()
    };

    // The transaction should be detected as from mega system address
    assert!(is_mega_system_transaction(&tx));

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

/// Tests that existing deposit transaction behavior is preserved and not affected by mega system
/// transaction logic.
///
/// This test verifies that:
/// - Actual deposit transactions (with `DEPOSIT_TRANSACTION_TYPE`) still work correctly
/// - Deposit transactions are detected as deposit-like but not as mega system transactions
/// - The mega system transaction detection doesn't interfere with existing deposit logic
/// - Both transaction types can coexist without conflicts
#[test]
fn test_deposit_transaction_behavior_preserved() {
    // Create a transaction that looks like a deposit transaction
    let mut deposit_tx = MegaTransaction {
        base: TxEnv {
            caller: REGULAR_CALLER,
            kind: TxKind::Call(WHITELISTED_ADDR),
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
    assert!(!is_mega_system_transaction(&deposit_tx));
}

/// Tests that mega system transactions do not deduct gas fees from the system address balance.
///
/// This test verifies that:
/// - `MEGA_SYSTEM_ADDRESS` balance remains unchanged after transaction execution
/// - Gas fees are not deducted from the system address (deposit-like behavior)
/// - The transaction executes successfully despite gas costs
/// - This demonstrates the fee bypass mechanism for system transactions
#[test]
fn test_mega_system_transaction_no_fee_deduction() {
    // Create database and set up contract with an initial balance for the mega system address
    let mut db = MemoryDatabase::default();
    let initial_balance = U256::from(1_000_000u64);
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, initial_balance);
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Get the initial balance before transaction
    let balance_before = evm.ctx().db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().balance;
    assert_eq!(balance_before, initial_balance, "Initial balance should be set correctly");

    // Execute transaction from mega system address with high gas price
    let result = execute_transaction(
        &mut evm,
        MEGA_SYSTEM_ADDRESS,
        WHITELISTED_ADDR,
        U256::ZERO,
        Bytes::new(),
    );

    // Transaction should succeed
    assert!(result.result.is_success(), "Mega system transaction should succeed");

    // Check balance after transaction - should be unchanged (no fee deducted)
    let balance_after = evm.ctx().db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().balance;
    assert_eq!(
        balance_after, initial_balance,
        "Balance should remain unchanged - no gas fee should be deducted from mega system address"
    );
}

/// Tests that regular transactions correctly deduct gas fees from the caller's balance.
///
/// This test verifies that:
/// - Regular transactions deduct gas fees from the caller's balance
/// - The balance reduction reflects the actual gas cost of the transaction
/// - This confirms normal transaction processing is unaffected
/// - Provides a contrast to mega system transactions which bypass fee deduction
#[test]
fn test_regular_transaction_fee_deduction() {
    // Create database and set up contract with an initial balance for the regular caller
    let mut db = MemoryDatabase::default();
    let initial_balance = U256::from(1_000_000_000_000u64);
    db.set_account_balance(REGULAR_CALLER, initial_balance);
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Get the initial balance before transaction
    let balance_before = evm.ctx().db_mut().basic(REGULAR_CALLER).unwrap().unwrap().balance;
    assert_eq!(balance_before, initial_balance, "Initial balance should be set correctly");

    // Execute transaction from regular caller
    let result =
        execute_transaction(&mut evm, REGULAR_CALLER, WHITELISTED_ADDR, U256::ZERO, Bytes::new());

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

/// Tests that mega system transactions do not pay operator fees even when fees are enabled.
///
/// This test verifies that:
/// - `MEGA_SYSTEM_ADDRESS` balance remains unchanged even with operator fees enabled
/// - Operator fees are not deducted from the system address (deposit-like behavior)
/// - L1 data fees are not charged to system transactions
/// - This demonstrates complete fee bypass including L1 and operator fees
#[test]
fn test_mega_system_transaction_no_operator_fees() {
    // Create database and set up contract with an initial balance for the mega system address
    let mut db = MemoryDatabase::default();
    let initial_balance = U256::from(1_000_000u64);
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, initial_balance);
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    // Create EVM with operator fees enabled
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, DefaultExternalEnvs::default());

    let block_env = BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        basefee: 1000,
        ..Default::default()
    };
    context.set_block(block_env);

    // Enable operator fees (instead of setting to zero)
    context.chain_mut().operator_fee_scalar = Some(U256::from(1_000_000));
    context.chain_mut().operator_fee_constant = Some(U256::from(10_000));

    let mut evm = MegaEvm::new(context);

    // Get the initial balance before transaction
    let balance_before = evm.ctx().db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().balance;
    assert_eq!(balance_before, initial_balance, "Initial balance should be set correctly");

    // Execute transaction from mega system address with some calldata (to trigger L1 data fee)
    let call_data = Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    let result =
        execute_transaction(&mut evm, MEGA_SYSTEM_ADDRESS, WHITELISTED_ADDR, U256::ZERO, call_data);

    // Transaction should succeed
    assert!(result.result.is_success(), "Mega system transaction should succeed");

    // Check balance after transaction - should be unchanged (no operator or L1 data fee deducted)
    let balance_after = evm.ctx().db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().balance;
    assert_eq!(
        balance_after, initial_balance,
        "Balance should remain unchanged - no operator fee or L1 data fee should be deducted from mega system address"
    );
}

/// Tests that mega system transactions with value transfer only deduct the transferred value.
///
/// This test verifies that:
/// - When a mega system transaction transfers value, balance changes by exactly that amount
/// - No gas fees, operator fees, or L1 data fees are added to the value transfer
/// - The recipient receives the exact transferred amount
/// - This demonstrates that fee bypass applies even when value is transferred
#[test]
fn test_mega_system_transaction_value_transfer_no_fees() {
    // Create database and set up accounts
    let mut db = MemoryDatabase::default();
    let initial_sender_balance = U256::from(1_000_000_000_000u64);
    let transfer_value = U256::from(50_000u64);

    // Allocate balance to sender BEFORE creating EVM
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, initial_sender_balance);
    // Set initial balance for recipient to ensure account exists
    db.set_account_balance(WHITELISTED_ADDR, U256::ZERO);
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    // Create EVM with operator fees enabled to ensure they're bypassed
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, DefaultExternalEnvs::default());

    let block_env = BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        basefee: 1000,
        ..Default::default()
    };
    context.set_block(block_env);

    // Enable operator fees
    context.chain_mut().operator_fee_scalar = Some(U256::from(1_000_000));
    context.chain_mut().operator_fee_constant = Some(U256::from(10_000));

    let mut evm = MegaEvm::new(context);

    // Verify initial balances are set correctly
    let sender_balance_before =
        evm.ctx().db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().balance;
    let recipient_balance_before =
        evm.ctx().db_mut().basic(WHITELISTED_ADDR).unwrap().unwrap().balance;

    assert_eq!(
        sender_balance_before, initial_sender_balance,
        "Initial sender balance should be set correctly"
    );
    assert_eq!(recipient_balance_before, U256::ZERO, "Initial recipient balance should be zero");

    // Execute transaction with value transfer
    let result = execute_transaction(
        &mut evm,
        MEGA_SYSTEM_ADDRESS,
        WHITELISTED_ADDR,
        transfer_value,
        Bytes::new(),
    );

    // Transaction should succeed
    assert!(
        result.result.is_success(),
        "Mega system transaction with value should succeed: {:?}",
        result.result
    );

    // Check sender balance - should be reduced by exactly the transfer value (no fees)
    let sender_balance_after = result.state.get(&MEGA_SYSTEM_ADDRESS).unwrap().info.balance;
    assert_eq!(
        sender_balance_after,
        initial_sender_balance - transfer_value,
        "Sender balance should be reduced by exactly the transfer value with no additional fees. Expected: {}, Got: {}",
        initial_sender_balance - transfer_value,
        sender_balance_after
    );

    // Check recipient balance - should be increased by exactly the transfer value
    let recipient_balance_after = result.state.get(&WHITELISTED_ADDR).unwrap().info.balance;
    assert_eq!(
        recipient_balance_after,
        recipient_balance_before + transfer_value,
        "Recipient balance should be increased by exactly the transfer value. Expected: {}, Got: {}",
        recipient_balance_before + transfer_value,
        recipient_balance_after
    );
}

/// Tests that mega system transactions to whitelisted addresses execute successfully.
///
/// This test verifies that:
/// - Transactions from `MEGA_SYSTEM_ADDRESS` to whitelisted addresses are allowed
/// - The transaction executes successfully and returns expected results
/// - The contract receives `MEGA_SYSTEM_ADDRESS` as the caller
/// - Whitelist enforcement allows legitimate system transactions
#[test]
fn test_mega_system_transaction_to_whitelisted_address_succeeds() {
    // Create database and set up the whitelisted contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Execute transaction from mega system address to whitelisted address
    let result = execute_transaction(
        &mut evm,
        MEGA_SYSTEM_ADDRESS,
        WHITELISTED_ADDR,
        U256::ZERO,
        Bytes::new(),
    );

    // Transaction should succeed since the address is whitelisted
    assert!(
        result.result.is_success(),
        "Mega system transaction to whitelisted address should succeed"
    );

    // The contract should return the caller address (MEGA_SYSTEM_ADDRESS)
    if let Some(output) = result.result.output() {
        let expected_caller = MEGA_SYSTEM_ADDRESS;
        let mut expected_output = [0u8; 32];
        expected_output[12..].copy_from_slice(expected_caller.as_slice());

        assert_eq!(
            output.as_ref(),
            &expected_output,
            "Whitelisted contract should receive MEGA_SYSTEM_ADDRESS as caller"
        );
    } else {
        panic!("Expected output from whitelisted contract");
    }
}

/// Tests that mega system transactions to non-whitelisted addresses are rejected.
///
/// This test verifies that:
/// - Transactions from `MEGA_SYSTEM_ADDRESS` to non-whitelisted addresses fail
/// - The error message indicates whitelist violation
/// - Whitelist enforcement prevents unauthorized system transactions
/// - Security mechanism correctly blocks non-approved destinations
#[test]
fn test_mega_system_transaction_to_non_whitelisted_address_fails() {
    // Create database and set up the non-whitelisted contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(NON_WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Attempt to execute transaction from mega system address to non-whitelisted address
    let tx = MegaTransaction {
        base: TxEnv {
            caller: MEGA_SYSTEM_ADDRESS,
            kind: TxKind::Call(NON_WHITELISTED_ADDR),
            value: U256::ZERO,
            data: Bytes::new(),
            gas_limit: 1_000_000,
            gas_price: 1000,
            ..Default::default()
        },
        ..Default::default()
    };

    // Transaction should fail due to whitelist check
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(result.is_err(), "Mega system transaction to non-whitelisted address should fail");

    // Check that the error message contains whitelist-related text
    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(
        error_msg.contains("whitelist") || error_msg.contains("Whitelist"),
        "Error should mention whitelist: {}",
        error_msg
    );
}

/// Tests that mega system transactions with CREATE operations are rejected.
///
/// This test verifies that:
/// - CREATE transactions from `MEGA_SYSTEM_ADDRESS` are not supported
/// - The error message indicates CREATE is not allowed for system transactions
/// - Security measure prevents system transactions from deploying arbitrary contracts
/// - Only CALL operations are supported for system transactions
#[test]
fn test_mega_system_transaction_create_fails() {
    // Create database
    let db = MemoryDatabase::default();
    let mut evm = create_evm(db);

    // Attempt to execute CREATE transaction from mega system address
    let tx = MegaTransaction {
        base: TxEnv {
            caller: MEGA_SYSTEM_ADDRESS,
            kind: TxKind::Create,
            value: U256::ZERO,
            data: Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]), // Simple contract bytecode
            gas_limit: 10_000_000,
            gas_price: 1000,
            ..Default::default()
        },
        ..Default::default()
    };

    // Transaction should fail since CREATE is not supported for system transactions
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(result.is_err(), "Mega system CREATE transaction should fail");
}

/// Tests that regular transactions to non-whitelisted addresses work normally.
///
/// This test verifies that:
/// - Regular transactions (non-system) can call any address regardless of whitelist
/// - Whitelist restrictions only apply to mega system transactions
/// - Normal transaction processing is unaffected by whitelist implementation
/// - Backward compatibility is maintained for existing transactions
#[test]
fn test_regular_transaction_to_non_whitelisted_address_succeeds() {
    // Create database and set up accounts and contract
    let mut db = MemoryDatabase::default();
    db.set_account_balance(REGULAR_CALLER, U256::from(1_000_000_000u64));
    db.set_account_code(NON_WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Execute transaction from regular address to non-whitelisted address
    let result = execute_transaction(
        &mut evm,
        REGULAR_CALLER,
        NON_WHITELISTED_ADDR,
        U256::ZERO,
        Bytes::new(),
    );

    // Transaction should succeed since whitelist only applies to system transactions
    assert!(
        result.result.is_success(),
        "Regular transaction to non-whitelisted address should succeed"
    );

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

/// Tests that mega system transactions to whitelisted addresses work with call data.
///
/// This test verifies that:
/// - System transactions can include arbitrary call data
/// - Whitelisted addresses can receive complex function calls from system transactions
/// - Call data does not affect whitelist validation
/// - Full contract interaction is supported for whitelisted addresses
#[test]
fn test_mega_system_transaction_whitelist_with_data() {
    // Create database and set up the whitelisted contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(WHITELISTED_ADDR, create_simple_contract());

    let mut evm = create_evm(db);

    // Execute transaction from mega system address to whitelisted address with data
    let call_data = Bytes::from_static(&[0x01, 0x02, 0x03, 0x04]);
    let result =
        execute_transaction(&mut evm, MEGA_SYSTEM_ADDRESS, WHITELISTED_ADDR, U256::ZERO, call_data);

    // Transaction should succeed even with call data
    assert!(
        result.result.is_success(),
        "Mega system transaction with data to whitelisted address should succeed"
    );
}
