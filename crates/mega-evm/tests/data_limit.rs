//! Tests for the data limit feature of the `MegaETH` EVM.

use std::convert::Infallible;

use alloy_primitives::{address, hex, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::set_account_code, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, NoOpOracle, TransactionError,
};
use revm::{
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        TxEnv,
    },
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    inspector::NoOpInspector,
    ExecuteEvm,
};

/// Executes a transaction on the EVM.
///
/// # Returns
///
/// Returns the execution result, the generated data size and the number of key-value updates.
fn transact(
    spec: MegaSpecId,
    db: &mut CacheDB<EmptyDB>,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
    data_limit: u64,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, TransactionError>> {
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
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx)?;

    let ctx = evm.ctx_ref();
    Ok((r, ctx.generated_data_size(), ctx.kv_update_count()))
}

/// Returns true if the result is a halt due to data limit exceeded.
fn is_data_limit_exceeded(result: ResultAndState<MegaHaltReason>) -> bool {
    match result.result {
        ExecutionResult::Halt { reason, .. } => reason == MegaHaltReason::DataLimitExceeded,
        _ => false,
    }
}

/// Returns true if the result is a halt due to KV update limit exceeded.
fn is_kv_update_limit_exceeded(result: ResultAndState<MegaHaltReason>) -> bool {
    match result.result {
        ExecutionResult::Halt { reason, .. } => reason == MegaHaltReason::KVUpdateLimitExceeded,
        _ => false,
    }
}

#[test]
fn test_data_limit() {
    let mut db = CacheDB::<EmptyDB>::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code: Bytes = hex!("620002005fa000").into();
    set_account_code(&mut db, contract_address, code);

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let (res, _, _) =
        transact(MegaSpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO, 600)
            .unwrap();
    assert!(res.result.is_halt());
}
