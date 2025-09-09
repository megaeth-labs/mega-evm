//! Tests for the data limit feature of the `MegaETH` EVM.

use std::convert::Infallible;

use alloy_primitives::{address, hex, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::set_account_code, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, NoOpOracle, TransactionError,
};
use revm::{
    context::{
        result::{EVMError, ResultAndState},
        TxEnv,
    },
    database::{CacheDB, EmptyDB},
    inspector::NoOpInspector,
};

/// Executes a transaction on the EVM.
fn transact(
    spec: MegaSpecId,
    db: &mut CacheDB<EmptyDB>,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
    data_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, TransactionError>> {
    let mut context = MegaContext::new(db, spec, NoOpOracle).with_data_limit(data_limit);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context, NoOpInspector);
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit: 1000000000000000000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

#[test]
fn test_data_limit() {
    let mut db = CacheDB::<EmptyDB>::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code: Bytes = hex!("620002005fa000").into();
    set_account_code(&mut db, contract_address, code);

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(MegaSpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO, 600)
            .unwrap();
    println!("result: {:?}", result);
    assert!(result.result.is_halt());
}
