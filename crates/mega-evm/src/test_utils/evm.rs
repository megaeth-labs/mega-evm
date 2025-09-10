use alloy_primitives::{Address, Bytes, TxKind, U256};
use core::convert::Infallible;
use revm::{
    context::{
        result::{EVMError, ResultAndState},
        TxEnv,
    },
    database::{AccountState, CacheDB, EmptyDB},
    inspector::NoOpInspector,
    state::{AccountInfo, Bytecode},
};

use crate::{
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError,
    NoOpOracle,
};

/// Sets the code for an account in the database. The account state is set to `None`, as if the
/// account originally had the code.
pub fn set_account_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = db.load_account(address).unwrap();
    account_info.info.code = Some(bytecode);
    account_info.info.code_hash = code_hash;
    account_info.account_state = AccountState::None;
}

/// Sets the balance for an account in the database. The account state is set to `None`, as if the
/// account originally had the balance.
pub fn set_account_balance(db: &mut CacheDB<EmptyDB>, address: Address, balance: U256) {
    let account_info = db.load_account(address).unwrap();
    account_info.info.balance = balance;
    account_info.account_state = AccountState::None;
}

/// Sets the nonce for an account in the database. The account state is set to `None`, as if the
/// account originally had the nonce.
pub fn set_account_nonce(db: &mut CacheDB<EmptyDB>, address: Address, nonce: u64) {
    let account_info = db.load_account(address).unwrap();
    account_info.info.nonce = nonce;
}

/// Executes a transaction on the EVM.
pub fn transact(
    spec: MegaSpecId,
    db: &mut CacheDB<EmptyDB>,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec, NoOpOracle);
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
