//! Beneficiary Gas Enforcement Demo
//!
//! This example demonstrates the gas limit enforcement mechanism for transactions
//! that access the beneficiary address. When a transaction accesses block.coinbase,
//! it is automatically subject to a reduced gas limit (50,000 gas) to prevent abuse.
//!
//! The demo shows three scenarios:
//! 1. Low gas transaction with beneficiary access - succeeds with normal gas usage
//! 2. Transaction without beneficiary access - no enforcement applied
//! 3. High gas transaction with beneficiary access - gas consumption limited to 50,000

use alloy_evm::Evm as EvmTrait;
use alloy_primitives::{address, Address, U256};
use mega_evm::{
    constants, Context, Evm, GasLimitEnforcementInspector, HaltReason, SpecId, Transaction,
};
use revm::{
    bytecode::opcode::{BALANCE, CALLER, POP, PUSH20, STOP},
    context::{
        result::{ExecutionResult, HaltReason as BaseHaltReason, OutOfGasError},
        BlockEnv, ContextSetters, ContextTr, TxEnv,
    },
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::TxKind,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(" **Beneficiary Gas Enforcement Demo**\n");

    let beneficiary = address!("0000000000000000000000000000000000000001");
    let caller = address!("0000000000000000000000000000000000100000");

    // Scenario 1: Low gas transaction with beneficiary access
    println!(" **Scenario 1: Low gas transaction with beneficiary access**");
    {
        let mut db = revm::database::CacheDB::<revm::database::EmptyDB>::default();
        let contract_address = address!("0000000000000000000000000000000000100001");

        // Create simple contract that accesses beneficiary and stops
        let mut contract_code = vec![PUSH20];
        contract_code.extend(beneficiary.as_slice());
        contract_code.extend([BALANCE, POP, STOP]);
        setup_contract(&mut db, contract_address, contract_code);

        let context = create_context(db, SpecId::MINI_REX, beneficiary);
        let inspector = GasLimitEnforcementInspector(NoOpInspector);
        let mut evm = Evm::new(context, inspector);
        evm.enable_inspect();

        let tx = Transaction {
            base: TxEnv {
                caller,
                kind: TxKind::Call(contract_address),
                gas_limit: 1_000_000,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = EvmTrait::transact_raw(&mut evm, tx)?;

        assert!(result.result.is_success());
        assert!(evm.ctx_ref().has_accessed_beneficiary_balance());

        println!("    Transaction succeeded with beneficiary access detected");
        println!("    Result: {:?}\n", result.result);
    }

    // Scenario 2: Transaction without beneficiary access
    println!(" **Scenario 2: Transaction without beneficiary access**");
    {
        let mut db = revm::database::CacheDB::<revm::database::EmptyDB>::default();
        let contract_address = address!("0000000000000000000000000000000000100002");

        // Create contract that doesn't access beneficiary
        let contract_code = vec![CALLER, POP, STOP];
        setup_contract(&mut db, contract_address, contract_code);

        let context = create_context(db, SpecId::MINI_REX, beneficiary);
        let inspector = GasLimitEnforcementInspector(NoOpInspector);
        let mut evm = Evm::new(context, inspector);
        evm.enable_inspect();

        let tx = Transaction {
            base: TxEnv {
                caller,
                kind: TxKind::Call(contract_address),
                gas_limit: 1_000_000,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = EvmTrait::transact_raw(&mut evm, tx)?;

        assert!(result.result.is_success());
        assert!(!evm.ctx_ref().has_accessed_beneficiary_balance());

        println!("    Transaction succeeded without beneficiary access");
        println!("    Result: {:?}\n", result.result);
    }

    // Scenario 3: High gas transaction with beneficiary access
    println!(" **Scenario 3: High gas transaction with beneficiary access**");
    {
        let mut db = revm::database::CacheDB::<revm::database::EmptyDB>::default();
        let contract_address = address!("0000000000000000000000000000000000100003");

        // Create gas-intensive contract that accesses beneficiary
        let mut contract_code = vec![PUSH20];
        contract_code.extend(beneficiary.as_slice());
        contract_code.extend([BALANCE, POP]);

        // Add expensive computation loop
        for _ in 0..500 {
            contract_code.push(PUSH20);
            contract_code.extend(beneficiary.as_slice());
            contract_code.extend([BALANCE, POP]);
        }
        contract_code.push(STOP);
        setup_contract(&mut db, contract_address, contract_code);

        let context = create_context(db, SpecId::MINI_REX, beneficiary);
        let inspector = GasLimitEnforcementInspector(NoOpInspector);
        let mut evm = Evm::new(context, inspector);
        evm.enable_inspect();

        let tx = Transaction {
            base: TxEnv {
                caller,
                kind: TxKind::Call(contract_address),
                gas_limit: 1_000_000, // High gas limit
                ..Default::default()
            },
            ..Default::default()
        };

        let result = EvmTrait::transact_raw(&mut evm, tx)?;

        // Assertions and output
        assert!(
            evm.ctx_ref().has_accessed_beneficiary_balance(),
            "Should detect beneficiary access"
        );

        let gas_used = result.result.gas_used();
        assert!(evm.ctx_ref().has_accessed_beneficiary_balance());
        assert!(gas_used <= constants::mini_rex::BENEFICIARY_GAS_LIMIT);
        assert!(matches!(
            result.result,
            ExecutionResult::Halt {
                reason: HaltReason::Base(BaseHaltReason::OutOfGas(OutOfGasError::InvalidOperand)),
                ..
            }
        ));

        println!(
            "    Gas enforcement working: {} gas (limit: {})",
            gas_used,
            constants::mini_rex::BENEFICIARY_GAS_LIMIT
        );
        println!("    Result: {:?}", result.result);
    }

    Ok(())
}

/// Set up contract code in database
fn setup_contract(
    db: &mut revm::database::CacheDB<revm::database::EmptyDB>,
    contract_address: Address,
    code: Vec<u8>,
) {
    let bytecode = revm::state::Bytecode::new_legacy(code.into());
    let code_hash = bytecode.hash_slow();
    let account_info =
        revm::state::AccountInfo { code: Some(bytecode), code_hash, ..Default::default() };
    db.insert_account_info(contract_address, account_info);
}

/// Create EVM context
fn create_context(
    db: revm::database::CacheDB<revm::database::EmptyDB>,
    spec: SpecId,
    beneficiary: Address,
) -> Context<revm::database::CacheDB<revm::database::EmptyDB>> {
    let mut context = Context::new(db, spec);
    context.set_block(BlockEnv { beneficiary, ..Default::default() });
    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));
    context
}
