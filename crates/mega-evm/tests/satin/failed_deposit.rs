//! A deposit op-revm fails reports the failure, and keeps what it counted.
//!
//! op-revm answers a deposit's transaction error with a `FailedDeposit` halt that bills the whole
//! gas limit and discards everything the deposit did. The halt is what the outcome reports, not a
//! stop the body may have latched before op-revm validated the deposit. What the deposit counted
//! stands: its body, and the Oracle hints it forwarded before it halted, which have left the
//! machine as they have for any other transaction that halts.

use core::convert::Infallible;

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE},
    test_utils::{
        op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase, OutcomeView,
    },
    EvmTxRuntimeLimits, ExternalEnvs, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionOutcome, TestExternalEnvs,
    TX_BODY_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, INVALID, POP},
    context::{result::ExecutionResult, TxEnv},
};

use crate::{
    cases::by_case,
    common::{block, call, context},
};

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
    let failed = OutcomeView::new(&outcome);

    // The same body from a user deposit, which op-revm admits, is the stop.
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1));
    let mut evm = MegaEvm::new(context(db)).with_tx_runtime_limits(limits);
    let outcome = evm.execute_transaction(deposit(Bytes::from_static(&[0xab]), false)).unwrap();
    assert!(matches!(outcome.result, ExecutionResult::Revert { .. }), "{:?}", outcome.result);
    assert!(outcome.limit_exceeded.is_some(), "the body's stop is reported");
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: TX_BODY_SIZE,
            used: body,
            frame_local: false,
        }),
        "the body crossed the data-size limit",
    );
    crate::assert_sorted_json_snapshot!(&by_case([
        ("a system deposit", failed),
        ("a user deposit", OutcomeView::new(&outcome)),
    ]));
}

/// Runs `tx` against a callee that sends the Oracle a hint of `payload` and then halts on
/// `INVALID`, and returns the outcome with the number of hints the oracle service received.
fn run_hint_then_halt(tx: MegaTransaction, payload: &[u8]) -> (MegaTransactionOutcome, usize) {
    let code = BytecodeBuilder::default()
        .mstore(0, payload)
        .push_number(0_u8) // retSize
        .push_number(0_u8) // retOffset
        .push_number(payload.len() as u64) // argsSize
        .push_number(0_u8) // argsOffset
        .push_number(0_u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(100_000_u32)
        .append_many([CALL, POP, INVALID])
        .build();
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1))
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
        .account_code(CALLEE, code);
    let envs = TestExternalEnvs::<Infallible>::new();
    let ctx = MegaContext::new_with_external_envs(
        db,
        MegaSpecId::SATIN,
        ExternalEnvs::from(envs.clone()),
    )
    .with_block(block())
    .with_chain(zero_fee_l1_block_info())
    .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    let outcome = MegaEvm::new(ctx).execute_transaction(tx).expect("the transaction is included");
    (outcome, envs.recorded_hints().len())
}

/// A deposit that forwards an Oracle hint and then halts is a failed deposit that keeps its body
/// and the hint's payload as data size: the hint reached the oracle service before the halt, and
/// nothing takes it back. The same program reached by an ordinary transaction halts and keeps the
/// same bytes.
#[test]
fn test_a_failed_deposit_keeps_the_hints_it_forwarded() {
    let payload = IOracle::sendHintCall {
        topic: B256::repeat_byte(0x7a),
        data: Bytes::from_static(b"a hint sent before the halt"),
    }
    .abi_encode();
    let mut deposit = op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 1_000_000,
        ..Default::default()
    });
    deposit.deposit.source_hash = B256::repeat_byte(0x22);
    let deposit = OpTx(deposit);
    let kept = LimitUsage {
        data_size: mega_evm::transaction_body_bytes(&deposit) + payload.len() as u64,
        write_records: 0,
    };

    let (outcome, hints) = run_hint_then_halt(deposit, &payload);
    assert!(
        matches!(
            outcome.result,
            ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
        ),
        "{:?}",
        outcome.result
    );
    assert_eq!(hints, 1, "the hint reached the oracle service before the halt");
    assert_eq!(outcome.limit_exceeded, None);
    assert_eq!(outcome.usage, kept, "the body and the forwarded hint stay counted");
    let failed = OutcomeView::new(&outcome);

    let ordinary = call(CALLER, CALLEE, U256::ZERO, 1_000_000);
    assert_eq!(
        mega_evm::transaction_body_bytes(&ordinary),
        kept.data_size - payload.len() as u64,
        "the two transactions have the same body"
    );
    let (outcome, hints) = run_hint_then_halt(ordinary, &payload);
    assert!(
        matches!(&outcome.result, ExecutionResult::Halt { reason, .. }
            if *reason != MegaHaltReason::FailedDeposit),
        "{:?}",
        outcome.result
    );
    assert_eq!(hints, 1);
    assert_eq!(outcome.usage, kept, "an ordinary transaction that halts keeps the same bytes");
    crate::assert_sorted_json_snapshot!(&by_case([
        ("a deposit", failed),
        ("an ordinary transaction", OutcomeView::new(&outcome)),
    ]));
}
