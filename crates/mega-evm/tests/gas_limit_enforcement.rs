//! This test suite verifies the gas limit enforcement functionality in the `MegaETH` EVM.
//! It tests three scenarios:
//! 1. Basic gas limit enforcement with beneficiary access
//! 2. No enforcement occurs without beneficiary access
//! 3. Enforce gas limit with high gas consumption

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::mini_rex::BENEFICIARY_GAS_LIMIT, Context, Evm, GasLimitEnforcementInspector,
    HaltReason, SpecId, Transaction,
};
use revm::{
    bytecode::opcode::{ADD, BALANCE, CALLER, POP, PUSH1, PUSH20, STOP},
    context::{
        result::{ExecutionResult, HaltReason as BaseHaltReason, OutOfGasError},
        BlockEnv, ContextSetters, ContextTr, TxEnv,
    },
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
};

// Helper function to set account code
fn set_account_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = AccountInfo { code: Some(bytecode), code_hash, ..Default::default() };
    db.insert_account_info(address, account_info);
}

fn setup_test_env() -> (CacheDB<EmptyDB>, Address, Address, Address) {
    let db = CacheDB::<EmptyDB>::default();
    let beneficiary = address!("0000000000000000000000000000000000000001");
    let contract_address = address!("0000000000000000000000000000000000100001");
    let caller = address!("0000000000000000000000000000000000100000");
    (db, beneficiary, contract_address, caller)
}

fn create_context(db: CacheDB<EmptyDB>, beneficiary: Address) -> Context<CacheDB<EmptyDB>> {
    let mut context = Context::new(db, SpecId::MINI_REX);
    context.set_block(BlockEnv { beneficiary, ..Default::default() });
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    context
}

fn create_beneficiary_access_contract(
    db: &mut CacheDB<EmptyDB>,
    contract_address: Address,
    beneficiary: Address,
) {
    let mut contract_code = vec![PUSH20];
    contract_code.extend(beneficiary.as_slice());
    contract_code.extend([BALANCE, POP, STOP]);
    set_account_code(db, contract_address, contract_code.into());
}

fn create_non_beneficiary_contract(db: &mut CacheDB<EmptyDB>, contract_address: Address) {
    let contract_code = vec![CALLER, POP, STOP];
    set_account_code(db, contract_address, contract_code.into());
}

fn create_gas_heavy_contract(
    db: &mut CacheDB<EmptyDB>,
    contract_address: Address,
    beneficiary: Address,
) {
    let mut contract_code = vec![PUSH20];
    contract_code.extend(beneficiary.as_slice());
    contract_code.extend([BALANCE, POP]);

    // Add expensive computation loop to exceed BENEFICIARY_GAS_LIMIT
    for _ in 0..5000 {
        // Increase loop size to ensure we exceed 50k gas
        contract_code.extend([PUSH1, 1, PUSH1, 1, ADD, POP]);
    }
    contract_code.push(STOP);
    set_account_code(db, contract_address, contract_code.into());
}

/// Test basic gas limit enforcement functionality
#[test]
fn test_gas_limit_enforcement_basic() {
    let (mut db, beneficiary, contract_address, caller) = setup_test_env();
    create_beneficiary_access_contract(&mut db, contract_address, beneficiary);

    // Test with enforcement inspector
    let context = create_context(db, beneficiary);
    let inspector = GasLimitEnforcementInspector(NoOpInspector);
    let mut evm = Evm::new(context, inspector);
    evm.enable_inspect();

    let tx = Transaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract_address),
            gas_limit: 1000000,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    assert!(result.result.is_success());
    assert!(evm.ctx_ref().has_accessed_beneficiary_balance());
}

/// Test that no enforcement occurs without beneficiary access
#[test]
fn test_no_enforcement_without_beneficiary_access() {
    let (mut db, beneficiary, contract_address, caller) = setup_test_env();
    create_non_beneficiary_contract(&mut db, contract_address);

    let context = create_context(db, beneficiary);
    let inspector = GasLimitEnforcementInspector(NoOpInspector);
    let mut evm = Evm::new(context, inspector);
    evm.enable_inspect();

    let tx = Transaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract_address),
            gas_limit: 1000000,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    assert!(result.result.is_success());
    assert!(!evm.ctx_ref().has_accessed_beneficiary_balance());
}

/// Test enforcement with high gas consumption
#[test]
fn test_gas_limit_enforcement_with_high_consumption() {
    let (mut db, beneficiary, contract_address, caller) = setup_test_env();
    create_gas_heavy_contract(&mut db, contract_address, beneficiary);

    let context = create_context(db, beneficiary);
    let inspector = GasLimitEnforcementInspector(NoOpInspector);
    let mut evm = Evm::new(context, inspector);
    evm.enable_inspect();

    let tx = Transaction {
        base: TxEnv {
            caller,
            kind: TxKind::Call(contract_address),
            gas_limit: 1000000, // High gas limit
            ..Default::default()
        },
        ..Default::default()
    };

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();

    // Should detect beneficiary access and enforce limit
    assert!(evm.ctx_ref().has_accessed_beneficiary_balance());

    // Gas usage should be limited to BENEFICIARY_GAS_LIMIT
    let gas_used = result.result.gas_used();
    assert!(
        gas_used <= BENEFICIARY_GAS_LIMIT,
        "Gas should be limited to {}, got {}",
        BENEFICIARY_GAS_LIMIT,
        gas_used
    );

    // Check the result type - if gas was limited due to enforcement, it should be InvalidOperand
    match &result.result {
        ExecutionResult::Halt {
            reason: HaltReason::Base(BaseHaltReason::OutOfGas(OutOfGasError::InvalidOperand)),
            ..
        } => {
            // This is the expected enforcement result when gas limit is exceeded
            println!("Correctly halted with OutOfGas(InvalidOperand) due to enforcement");
        }
        ExecutionResult::Success { .. } => {
            // If transaction succeeded, it means the contract completed within the enforcement
            // limit
            println!("Transaction completed within enforcement limit");
            assert!(
                gas_used <= BENEFICIARY_GAS_LIMIT,
                "If transaction succeeds, gas should be within limit. Used: {}, Limit: {}",
                gas_used,
                BENEFICIARY_GAS_LIMIT
            );
        }
        other => {
            panic!("Unexpected result for gas limit enforcement: {:?}", other);
        }
    }
}
