use alloy_evm::{Database, Evm};
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use op_revm::{L1BlockInfo, OpTransaction};
use revm::context::{
    result::{EVMError, ResultAndState},
    TxEnv,
};

use crate::{MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransactionError};

/// L1 block info with zero operator fee, so a transaction pays no L1-side fee.
pub fn zero_fee_l1_block_info() -> L1BlockInfo {
    L1BlockInfo {
        operator_fee_scalar: Some(U256::ZERO),
        operator_fee_constant: Some(U256::ZERO),
        ..Default::default()
    }
}

/// Wraps `tx` as a non-deposit OP transaction with an empty enveloped encoding, so its L1 data
/// fee is zero.
pub fn op_transaction(tx: TxEnv) -> OpTransaction<TxEnv> {
    OpTransaction { base: tx, enveloped_tx: Some(Bytes::new()), ..Default::default() }
}

/// Executes one transaction on a fresh [`MegaEvm`] over `db` and returns its result and state
/// without committing.
///
/// The L1 fees are zero (see [`zero_fee_l1_block_info`] and [`op_transaction`]) and so is the
/// gas price, so the caller needs no balance beyond `value`. A `gas_limit` above the execution
/// cap puts the excess into the state-gas reservoir.
pub fn transact<DB: Database>(
    spec: MegaSpecId,
    db: DB,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
    gas_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<DB::Error, MegaTransactionError>> {
    let context = MegaContext::new(db, spec).with_chain(zero_fee_l1_block_info());
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit,
        ..Default::default()
    };
    MegaEvm::new(context).transact_raw(OpTx(op_transaction(tx)))
}
