//! The README's Quick Start: execute one transaction with `MegaEvm`.
//!
//! Kept as an example so the snippet the README shows is compiled with the crate.

use mega_evm::{
    alloy_evm,
    alloy_primitives::{address, Bytes, U256},
    op_revm::OpTransaction,
    revm::{
        context::{ContextTr, TxEnv},
        database::{CacheDB, EmptyDB},
        primitives::TxKind,
    },
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // An empty in-memory database and the latest frozen spec.
    let db = CacheDB::<EmptyDB>::default();
    let mut context = MegaContext::new(db, MegaSpecId::REX6);
    // Isthmus requires the L1 operator fee parameters to be set.
    context.chain_mut().operator_fee_scalar = Some(U256::ZERO);
    context.chain_mut().operator_fee_constant = Some(U256::ZERO);
    let mut evm = MegaEvm::new(context);

    let mut tx = MegaTransaction(OpTransaction::new(TxEnv {
        caller: address!("0x0000000000000000000000000000000000100000"),
        kind: TxKind::Call(address!("0x0000000000000000000000000000000000100001")),
        gas_limit: 1_000_000,
        ..Default::default()
    }));
    // The enveloped transaction feeds the L1 data fee; empty is enough here.
    tx.enveloped_tx = Some(Bytes::new());

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx)?;
    println!("success: {}, gas used: {}", result.result.is_success(), result.result.tx_gas_used());
    assert!(result.result.is_success());
    Ok(())
}
