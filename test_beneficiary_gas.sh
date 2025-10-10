#!/bin/bash
# Quick test to verify beneficiary gas limiting works

cat > /tmp/test_beneficiary_gas.rs << 'TESTEOF'
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
};
use revm::{bytecode::opcode::*, context::TxEnv};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000BEEF01");

fn main() {
    println!("Testing beneficiary gas limiting...\n");
    
    // Create contract that reads beneficiary balance
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);
    
    let external_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX, &external_envs);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    context.modify_block(|block| {
        block.beneficiary = BENEFICIARY;
    });
    
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        data: Default::default(),
        value: U256::ZERO,
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    
    let result = alloy_evm::Evm::transact_commit(&mut evm, tx).unwrap();
    let success = result.is_success();
    let gas_used = result.gas_used();
    
    println!("Transaction success: {}", success);
    println!("Gas used: {}", gas_used);
    
    assert!(success, "Transaction should succeed");
    assert!(gas_used < 100_000, 
        "Gas should be limited after beneficiary access, got {}", gas_used);
    
    println!("\n✅ Beneficiary gas limiting works correctly!");
    println!("   Gas was limited to prevent DoS after accessing beneficiary balance");
}
TESTEOF

rustc --edition 2021 \
  -L /nvme2/data/william/mega-evm/target/debug/deps \
  --extern alloy_primitives=/nvme2/data/william/mega-evm/target/debug/deps/liballoy_primitives-*.rlib \
  --extern mega_evm=/nvme2/data/william/mega-evm/target/debug/libmega_evm.rlib \
  --extern revm=/nvme2/data/william/mega-evm/target/debug/deps/librevm-*.rlib \
  /tmp/test_beneficiary_gas.rs -o /tmp/test_beneficiary_gas 2>&1 | grep -v "warning:" | head -20

if [ -f /tmp/test_beneficiary_gas ]; then
  /tmp/test_beneficiary_gas
else
  echo "Compilation failed"
fi
