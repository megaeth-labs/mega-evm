//! Shared setup of the system contract tests.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    system::{
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE},
        ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE,
    },
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    bytecode::opcode::{ADD, MSTORE, RETURN, RETURNDATACOPY, RETURNDATASIZE},
    context::{result::ResultAndState, BlockEnv, TxEnv},
    Database,
};

/// The sender of every transaction the tests run.
pub(crate) const CALLER: Address = address!("0x0000000000000000000000000000000000300000");

/// The contract the tests deploy when a call has to come from code.
pub(crate) const CONTRACT: Address = address!("0x0000000000000000000000000000000000300001");

/// A gas limit below the execution cap, so a transaction has no state-gas reservoir.
pub(crate) const GAS_LIMIT: u64 = 100_000_000;

/// A database holding the code of every system contract, as the chain holds it once the fork
/// that deploys them has activated.
pub(crate) fn system_db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_u64))
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
        .account_code(HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE)
        .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE)
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
        .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE)
}

/// A database as [`system_db`], with `code` deployed at [`CONTRACT`] and a balance to send
/// from, so a test that makes a value-bearing call fails on the policy and not on the funds.
pub(crate) fn with_contract(code: Bytes) -> MemoryDatabase {
    system_db().account_code(CONTRACT, code).account_balance(CONTRACT, U256::from(1_000_000))
}

/// A block with room for any transaction the tests run.
pub(crate) fn block() -> BlockEnv {
    BlockEnv { number: U256::from(1), gas_limit: 10_000_000_000, ..Default::default() }
}

/// A Satin context over `db` with zero L1 fees.
pub(crate) fn context<DB: Database>(db: DB) -> MegaContext<DB> {
    MegaContext::new(db, MegaSpecId::SATIN).with_block(block()).with_chain(zero_fee_l1_block_info())
}

/// A transaction from [`CALLER`] calling `to` with `data` and `value`.
pub(crate) fn call_tx(to: Address, data: impl AsRef<[u8]>, value: U256) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data: Bytes::copy_from_slice(data.as_ref()),
        value,
        gas_limit: GAS_LIMIT,
        ..Default::default()
    }))
}

/// Runs `tx` on a fresh Satin EVM over `db`, without committing.
pub(crate) fn run<DB: alloy_evm::Database>(
    db: DB,
    tx: MegaTransaction,
) -> ResultAndState<MegaHaltReason> {
    MegaEvm::new(context(db)).transact_raw(tx).expect("the transaction is valid")
}

/// Code that calls `target` with `data` through the `scheme` opcode, forwarding all the gas it
/// has, and returns the call's status followed by the callee's return data.
///
/// `value` is passed by the two schemes that carry one (`CALL`, `CALLCODE`) and ignored by the
/// two that do not.
pub(crate) fn calls_with(scheme: u8, target: Address, data: &[u8], value: u64) -> Bytes {
    use revm::bytecode::opcode::{CALL, CALLCODE, GAS};

    let mut code = BytecodeBuilder::default().mstore(0x0, data);
    code = code
        .push_number(0_u64) // retSize: the return data is read with RETURNDATACOPY
        .push_number(0_u64) // retOffset
        .push_number(data.len() as u64) // argsSize
        .push_number(0_u64); // argsOffset
    if scheme == CALL || scheme == CALLCODE {
        code = code.push_number(value);
    }
    code.push_address(target)
        .append(GAS)
        .append(scheme)
        // memory[0..32] = the call's status
        .push_number(0_u64)
        .append(MSTORE)
        // memory[32..32 + returndatasize] = the callee's return data
        .append(RETURNDATASIZE)
        .push_number(0_u64)
        .push_number(32_u64)
        .append(RETURNDATACOPY)
        // return both
        .append(RETURNDATASIZE)
        .push_number(32_u64)
        .append(ADD)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// Splits what [`calls_with`] returns into the inner call's status and its return data.
pub(crate) fn split_outcome(output: &[u8]) -> (bool, &[u8]) {
    let (status, data) = output.split_at(32);
    (U256::from_be_slice(status) == U256::ONE, data)
}

/// The output of a successful transaction.
pub(crate) fn output(result: &ResultAndState<MegaHaltReason>) -> Bytes {
    assert!(result.result.is_success(), "the transaction failed: {:?}", result.result);
    result.result.output().cloned().unwrap_or_default()
}

/// The revert data of a transaction that reverted.
pub(crate) fn revert_data(result: &ResultAndState<MegaHaltReason>) -> Bytes {
    match &result.result {
        revm::context::result::ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("the transaction did not revert: {other:?}"),
    }
}
