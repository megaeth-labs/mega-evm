use alloy_primitives::{Address, Bytes, TxKind, U256};
use core::convert::Infallible;
use revm::{
    context::{
        result::{EVMError, ResultAndState},
        TxEnv,
    },
    database::{CacheDB, EmptyDB},
    inspector::NoOpInspector,
    state::{AccountInfo, Bytecode},
};

use crate::{Context, Evm, HaltReason, SpecId, Transaction, TransactionError};

/// Sets the code for an account in the database.
pub fn set_account_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = AccountInfo { code: Some(bytecode), code_hash, ..Default::default() };
    db.insert_account_info(address, account_info);
}

/// Executes a transaction on the EVM.
pub fn transact(
    spec: SpecId,
    db: &mut CacheDB<EmptyDB>,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
) -> Result<ResultAndState<HaltReason>, EVMError<Infallible, TransactionError>> {
    let mut context = Context::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = Evm::new(context, NoOpInspector);
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit: 1000000000000000000,
        ..Default::default()
    };
    let mut tx = Transaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}
