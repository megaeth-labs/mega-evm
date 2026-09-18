use alloy_evm::{Database, Evm};
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use op_revm::{L1BlockInfo, OpTransaction};
use revm::context::{
    result::{EVMError, ResultAndState},
    TxEnv,
};

use crate::{MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransactionError};

/// Executes one transaction on a fresh [`MegaEvm`] over `db` and returns its result and state
/// without committing.
///
/// The L1 fees are zero (operator fee zero, empty enveloped transaction) and so is the gas
/// price, so the caller needs no balance beyond `value`. A `gas_limit` above the execution cap
/// puts the excess into the state-gas reservoir.
pub fn transact<DB: Database>(
    spec: MegaSpecId,
    db: DB,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
    gas_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<DB::Error, MegaTransactionError>> {
    let context = MegaContext::new(db, spec).with_chain(L1BlockInfo {
        operator_fee_scalar: Some(U256::ZERO),
        operator_fee_constant: Some(U256::ZERO),
        ..Default::default()
    });
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit,
        ..Default::default()
    };
    let tx =
        OpTx(OpTransaction { base: tx, enveloped_tx: Some(Bytes::new()), ..Default::default() });
    MegaEvm::new(context).transact_raw(tx)
}
