use alloy_primitives::{Address, Bytes, TxKind, U256};
use core::fmt::Debug;
use revm::context::{
    result::{EVMError, ResultAndState},
    TxEnv,
};

use crate::{
    MegaContext, MegaDatabase, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};

/// Executes a transaction on the EVM.
pub fn transact<DB>(
    spec: MegaSpecId,
    db: DB,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<<DB as revm::Database>::Error, MegaTransactionError>>
where
    DB: MegaDatabase + Debug,
    <DB as revm::Database>::Error: Send + Sync + Debug + 'static,
{
    let mut context = MegaContext::new(db, spec);
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
        gas_limit: 1000000000000000000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}
