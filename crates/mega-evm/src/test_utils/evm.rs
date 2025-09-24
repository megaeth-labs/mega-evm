use alloy_primitives::{Address, Bytes, TxKind, U256};
use core::fmt::Debug;
use revm::{
    context::{
        result::{EVMError, ResultAndState},
        TxEnv,
    },
    Database,
};

use crate::{
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError,
    NoOpOracle,
};

/// Executes a transaction on the EVM.
pub fn transact<DB>(
    spec: MegaSpecId,
    db: DB,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<DB::Error, MegaTransactionError>>
where
    DB: Database + Debug,
    DB::Error: Send + Sync + Debug + 'static,
{
    let mut context = MegaContext::new(db, spec, NoOpOracle::default());
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
