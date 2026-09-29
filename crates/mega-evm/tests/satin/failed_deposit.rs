//! A deposit op-revm refuses reports the refusal and nothing else.
//!
//! op-revm answers a deposit's transaction error with a `FailedDeposit` halt that bills the whole
//! gas limit and discards everything the deposit did. The common execution layer settles as it
//! does for an out-of-gas before the first frame: the halt is what the outcome reports, not a stop
//! the body may have latched before op-revm validated the deposit, and the body is what the deposit
//! kept.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    test_utils::{op_transaction, MemoryDatabase},
    EvmTxRuntimeLimits, LimitUsage, MegaEvm, MegaHaltReason, MegaTransaction, TX_BODY_SIZE,
};
use revm::context::{result::ExecutionResult, TxEnv};

use crate::common::context;

const CALLER: Address = address!("0000000000000000000000000000000000f00000");
const CALLEE: Address = address!("0000000000000000000000000000000000f00001");

/// A deposit of `data` to the callee, as a system transaction when `system`, which Regolith
/// refuses.
fn deposit(data: Bytes, system: bool) -> MegaTransaction {
    let mut tx = op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        data,
        gas_limit: 100_000,
        ..Default::default()
    });
    tx.deposit.source_hash = B256::repeat_byte(0x22);
    tx.deposit.is_system_transaction = system;
    OpTx(tx)
}

/// A system deposit whose body crosses the data-size limit: the body latches the transaction
/// before op-revm validates it, op-revm refuses the deposit, and the outcome reports the failed
/// deposit, no stop, and the body as what was kept.
#[test]
fn test_a_failed_deposit_reports_the_halt_and_not_the_stop_its_body_latched() {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1));
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE);
    let mut evm = MegaEvm::new(context(db)).with_tx_runtime_limits(limits);
    let tx = deposit(Bytes::from_static(&[0xab]), true);
    let body = mega_evm::transaction_body_bytes(&tx);
    assert!(body > TX_BODY_SIZE, "the body crosses the limit");

    let outcome = evm.execute_transaction(tx).expect("the deposit is included");
    assert!(
        matches!(
            outcome.result,
            ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
        ),
        "{:?}",
        outcome.result
    );
    assert_eq!(outcome.gas.gas_used, 100_000, "a failed deposit bills its gas limit");
    assert_eq!(outcome.limit_exceeded, None, "the halt is what the deposit reports");
    assert_eq!(outcome.usage, LimitUsage { data_size: body, write_records: 0 });
    assert_eq!(outcome.state[&CALLER].info.nonce, 1, "a failed deposit bumps its sender's nonce");

    // The same body from a user deposit, which op-revm admits, is the stop.
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1));
    let mut evm = MegaEvm::new(context(db)).with_tx_runtime_limits(limits);
    let outcome = evm.execute_transaction(deposit(Bytes::from_static(&[0xab]), false)).unwrap();
    assert!(matches!(outcome.result, ExecutionResult::Revert { .. }), "{:?}", outcome.result);
    assert!(outcome.limit_exceeded.is_some(), "the body's stop is reported");
}
