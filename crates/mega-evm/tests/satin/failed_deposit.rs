//! A deposit op-revm fails reports the failure, and keeps what it counted.
//!
//! op-revm answers a deposit's transaction error with a `FailedDeposit` halt that bills the whole
//! gas limit and discards everything the deposit did, and validation's refusal of a body over the
//! data-size limit is one such error. The halt is what the outcome reports. What the deposit
//! counted stands: its body, and the Oracle hints it forwarded before it halted, which have left
//! the machine as they have for any other transaction that halts.

use core::convert::Infallible;

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE},
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, ExternalEnvs, LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionOutcome, TestExternalEnvs, TX_BODY_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, INVALID, POP},
    context::{result::ExecutionResult, TxEnv},
};

use crate::common::{account_state_gas, block, call, context, grown};

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

/// A system deposit whose body crosses the data-size limit is refused by the inherited rules,
/// which refuse every system deposit under Regolith, and op-revm includes it as a failed deposit:
/// the outcome reports the failure, no stop, and the body as what was kept.
#[test]
fn test_a_system_deposit_over_the_limit_reports_the_failed_deposit() {
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
    assert_eq!(outcome.usage, LimitUsage { data_size: body, write_records: 0, state_growth: 0 });
    assert_eq!(outcome.state[&CALLER].info.nonce, 1, "a failed deposit bumps its sender's nonce");
}

/// A deposit whose body exceeds the data-size limit fails validation, as any transaction with that
/// body does, and op-revm includes it as a failed deposit: it bills its gas limit, bumps its
/// sender's nonce, runs nothing, reports no stop, and keeps its body as its data size.
///
/// At the spec's limit no deposit carries such a body and passes the gas checks, the floor alone
/// being past the execution cap; under a limit one byte below its body, a user deposit whose gas
/// covers it is refused for the body itself.
#[test]
fn test_a_deposit_whose_body_is_over_the_limit_is_a_failed_deposit() {
    let spec_limit =
        mega_evm::ProtocolLimits::for_spec(MegaSpecId::SATIN).tx_runtime_limits.tx_data_size_limit;
    let spec_body = vec![0xab; (spec_limit - TX_BODY_SIZE + 1) as usize];
    let lowered = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE);
    let cases = [
        ("the spec's limit", Bytes::from(spec_body), None),
        ("a lowered limit", Bytes::from_static(&[0xab]), Some(lowered)),
    ];
    for (case, data, limits) in cases {
        let tx = deposit(data, false);
        let body = mega_evm::transaction_body_bytes(&tx);
        let limit = limits.map_or(spec_limit, |limits| limits.tx_data_size_limit);
        assert_eq!(body, limit + 1, "{case}");

        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1));
        let mut ctx = context(db);
        if let Some(limits) = limits {
            ctx = ctx.with_tx_runtime_limits(limits);
        }
        let outcome = MegaEvm::new(ctx).execute_transaction(tx).expect("the deposit is included");
        assert!(
            matches!(
                outcome.result,
                ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
            ),
            "{case}: {:?}",
            outcome.result
        );
        assert_eq!(outcome.gas.gas_used, 100_000, "{case}: a failed deposit bills its gas limit");
        assert_eq!(outcome.limit_exceeded, None, "{case}: the failure is what it reports");
        assert_eq!(
            outcome.usage,
            LimitUsage { data_size: body, write_records: 0, state_growth: 0 },
            "{case}"
        );
        assert_eq!(outcome.state[&CALLER].info.nonce, 1, "{case}: the sender's nonce is bumped");
    }
}

/// A sender that does not exist before its deposit, which the deposit creates.
const ABSENT: Address = address!("0000000000000000000000000000000000f00002");

/// A deposit whose body crosses the data-size limit is a failed deposit at every gas limit,
/// whether or not it would also fail another check, and whether or not its gas could pay for the
/// account it creates for its caller: validation refuses it before anything runs or is charged.
///
/// The failed deposit bills its whole gas limit as regular gas and nothing on the state or history
/// ledgers — the caller account a failed deposit creates carries no state gas — bumps its
/// sender's nonce, credits its mint, reports no stop, and keeps its body as its data size. Every
/// deposit here calls an account without code, with no value and no calldata, so its intrinsic gas
/// is 15,000: EIP-2780's base of 12,000 and 3,000 for the recipient; the data-size limit is one
/// byte below the 310-byte body.
///
/// The rows:
///
/// - a user deposit whose gas limit is one below its intrinsic gas, and one whose gas limit covers
///   it;
/// - a system deposit, which the inherited rules refuse under Regolith as well, with a gas limit
///   below its intrinsic gas and from an absent sender with 10,000,000;
/// - a user deposit from an absent sender whose gas limit covers its intrinsic gas and all but one
///   gas of its caller's account, and one whose gas limit covers that account too.
///
/// Rules: [S10.15], [S11], [S5], [S4], [S21.5], [S21.7], [S5.12]. Independence: constants — the
/// intrinsic gas and the body are hand arithmetic from EIP-2780 and `TX_BODY_SIZE`; the caller
/// account's state gas is the schedule's entry at the price in effect.
#[test]
fn test_a_deposit_over_the_limit_fails_at_every_gas_limit() {
    const INTRINSIC: u64 = 12_000 + 3_000;
    const MINT: u64 = 7;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE - 1);
    let body = LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 };
    let run = |sender: Address, gas_limit: u64, system: bool| {
        let mut tx = op_transaction(TxEnv {
            caller: sender,
            kind: TxKind::Call(CALLEE),
            gas_limit,
            ..Default::default()
        });
        tx.deposit.source_hash = B256::repeat_byte(0x22);
        tx.deposit.mint = Some(u128::from(MINT));
        tx.deposit.is_system_transaction = system;
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1));
        MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .execute_transaction(OpTx(tx))
            .expect("the deposit is included")
    };

    // (row, a system deposit, the sender, its balance before, the gas limit)
    let rows = [
        ("a user deposit short of its intrinsic gas", false, CALLER, 1, INTRINSIC - 1),
        ("a user deposit that covers it", false, CALLER, 1, INTRINSIC),
        ("a system deposit", true, CALLER, 1, INTRINSIC - 1),
        ("a system deposit from an absent sender", true, ABSENT, 0, 10_000_000),
        (
            "a user deposit short of its caller's account",
            false,
            ABSENT,
            0,
            INTRINSIC + account_state_gas() - 1,
        ),
        ("a user deposit that covers it", false, ABSENT, 0, INTRINSIC + account_state_gas()),
    ];
    for (row, system, sender, balance_before, gas_limit) in rows {
        let outcome = run(sender, gas_limit, system);
        assert!(
            matches!(
                outcome.result,
                ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
            ),
            "{row}: {:?}",
            outcome.result
        );
        assert_eq!(outcome.limit_exceeded, None, "{row}: no stop is reported");
        assert_eq!(outcome.gas.gas_used, gas_limit, "{row}: the whole gas limit");
        assert_eq!(
            (outcome.gas.regular, outcome.gas.state, outcome.gas.history),
            (gas_limit, 0, 0),
            "{row}: all of it regular gas"
        );
        let created = u64::from(sender == ABSENT);
        assert_eq!(
            outcome.usage,
            grown(body, created),
            "{row}: the body stays counted, and an absent sender's new account is state growth"
        );
        let sender = &outcome.state[&sender].info;
        assert_eq!(sender.nonce, 1, "{row}: the sender's nonce is bumped");
        assert_eq!(sender.balance, U256::from(balance_before + MINT), "{row}: the mint");
    }
}

/// A failed deposit whose sender had no account creates it, as its nonce bump and mint stand: one
/// account of state growth, which the transaction reports and its block counts, with no state gas
/// for it on any ledger. The deposit has already failed, so no limit stops it, a state-growth limit
/// of nothing included. A sender that had an account adds nothing.
#[test]
fn test_a_failed_deposit_counts_the_caller_it_creates() {
    const INTRINSIC: u64 = 12_000 + 3_000;
    let run = |sender: Address, limits: EvmTxRuntimeLimits| {
        let mut tx = op_transaction(TxEnv {
            caller: sender,
            kind: TxKind::Call(CALLEE),
            gas_limit: INTRINSIC - 1,
            ..Default::default()
        });
        tx.deposit.source_hash = B256::repeat_byte(0x22);
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1));
        MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .execute_transaction(OpTx(tx))
            .expect("the deposit is included")
    };
    let unlimited = EvmTxRuntimeLimits::no_limits();
    let none_left = unlimited.with_tx_state_growth_limit(0);
    for (case, sender, limits, created) in [
        ("an absent sender", ABSENT, unlimited, 1),
        ("an absent sender under a limit of nothing", ABSENT, none_left, 1),
        ("a sender with an account", CALLER, unlimited, 0),
    ] {
        let outcome = run(sender, limits);
        assert!(
            matches!(
                outcome.result,
                ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
            ),
            "{case}: {:?}",
            outcome.result
        );
        assert_eq!(outcome.limit_exceeded, None, "{case}: no stop");
        assert_eq!(outcome.usage.state_growth, created, "{case}");
        assert_eq!(outcome.gas.state, 0, "{case}: no state gas for the account");
        assert_eq!(outcome.state[&sender].info.nonce, 1, "{case}: the account exists");
    }
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
        state_growth: 0,
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
}
