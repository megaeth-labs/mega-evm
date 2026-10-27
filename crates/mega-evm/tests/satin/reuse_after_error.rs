//! An EVM reused after a call failed with an error runs its next call as a fresh EVM runs it.
//!
//! A call that fails with an error — a read the database could not serve — is no result a block
//! includes, and what it left in the EVM is the handler's fatal cleanup's to drop
//! (`MegaHandler::catch_error`): the error a Host load recorded and nothing took, the failed
//! call's journal transaction, op-revm's per-transaction caches and the frame stack. A `_one`
//! entry point leaves the journal to its caller, so the calls before the failed one stay in it and
//! none of the failed call's writes do; a failed deposit is a result, whose nonce bump and mint
//! stay. Nothing here needs an inspector.
//!
//! This file pins the state a failed call leaves behind; `db_error.rs` pins the error itself.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, TxKind, B256, U256};
use mega_evm::{
    op_revm::constants::{BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    test_utils::{op_transaction, BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    EvmTxRuntimeLimits, MegaEvm, MegaHaltReason, MegaTransaction, MegaTransactionOutcome,
    TX_BODY_SIZE,
};
use revm::{
    bytecode::opcode::{BALANCE, POP, SLOAD},
    context::{result::ExecutionResult, BlockEnv, ContextTr, TxEnv},
    context_interface::LocalContextTr,
    handler::EvmTr,
    ExecuteEvm,
};

use crate::common::{block, call, context};

const SENDER: Address = address!("0000000000000000000000000000000000e10000");
/// Another sender, of the transaction a `_one` journal holds before the failing one.
const EARLIER_SENDER: Address = address!("0000000000000000000000000000000000e10001");
/// `SSTORE(0, 7)`, then a call to `CHILD`.
const WRITER: Address = address!("0000000000000000000000000000000000e10002");
/// Reads slot 1, which the database fails.
const CHILD: Address = address!("0000000000000000000000000000000000e10003");
/// `SSTORE(0, 1)`: the call that runs before the failing one.
const KEPT: Address = address!("0000000000000000000000000000000000e10004");
/// Reads the balance of the account the test names.
const READS: Address = address!("0000000000000000000000000000000000e10005");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000e1bef0");
const GAS_LIMIT: u64 = 100_000_000;

/// A block whose beneficiary is [`BENEFICIARY`].
fn block_with_beneficiary() -> BlockEnv {
    BlockEnv { beneficiary: BENEFICIARY, ..block() }
}

/// The pre-state every test starts from: the senders funded, and the contracts.
fn database() -> MemoryDatabase {
    let writer = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(7))
        .call(CHILD, U256::ZERO)
        .append(POP)
        .stop()
        .build();
    let child = BytecodeBuilder::default().push_number(1_u64).append(SLOAD).append(POP).stop();
    MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10u64.pow(18)))
        .account_balance(EARLIER_SENDER, U256::from(10u64.pow(18)))
        .account_code(WRITER, writer)
        .account_code(CHILD, child.build())
        .account_code(KEPT, BytecodeBuilder::default().sstore(U256::ZERO, U256::ONE).stop().build())
}

/// A Satin EVM over `db`, in a block whose beneficiary is [`BENEFICIARY`].
fn evm(
    db: ErrorInjectingDatabase,
) -> MegaEvm<ErrorInjectingDatabase, revm::inspector::NoOpInspector> {
    MegaEvm::new(context(db).with_block(block_with_beneficiary()))
}

/// Asserts `reused` is the outcome `fresh` is, in every part: the result, the state, the gas by
/// ledger, the usage, the stop and the oracle reads.
fn assert_same_outcome(reused: &MegaTransactionOutcome, fresh: &MegaTransactionOutcome, row: &str) {
    assert_eq!(reused.result, fresh.result, "{row}: the result");
    assert_eq!(reused.state, fresh.state, "{row}: the state");
    assert_eq!(reused.gas, fresh.gas, "{row}: the gas by ledger");
    assert_eq!(reused.usage, fresh.usage, "{row}: the usage");
    assert_eq!(reused.limit_exceeded, fresh.limit_exceeded, "{row}: the stop");
    assert_eq!(reused.oracle_reads, fresh.oracle_reads, "{row}: the oracle reads");
}

/// The transaction's own frame reads an account and the database fails the read: the Host records
/// the error and the frame ends, and post-execution then reads the same account — the beneficiary
/// it rewards, or a fee recipient it credits — and fails first, so the handler never takes the
/// recorded error. The fatal cleanup drops it with the failed transaction: the EVM's error slot is
/// empty, and a different transaction run next on the same EVM, the database serving again, has
/// the outcome a fresh EVM gives it.
#[test]
fn test_an_error_post_execution_preempts_does_not_fail_the_next_transaction() {
    for account in [BENEFICIARY, L1_FEE_RECIPIENT, BASE_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT] {
        let row = format!("a read of {account}");
        let reads =
            BytecodeBuilder::default().push_address(account).append(BALANCE).append(POP).stop();
        let reads = reads.build();
        let db = || database().account_code(READS, reads.clone());
        let mut armed = ErrorInjectingDatabase::new(db());
        armed.fail_on_account = Some(account);
        let mut evm = evm(armed);

        let error = evm
            .execute_transaction(call(SENDER, READS, U256::ZERO, GAS_LIMIT))
            .expect_err("the read fails");
        assert!(format!("{error:?}").contains(&format!("{account}")), "{row}: {error:?}");
        assert!(evm.ctx_mut().error().is_ok(), "{row}: the recorded error is dropped");

        evm.ctx_mut().db_mut().fail_on_account = None;
        let next = call(SENDER, KEPT, U256::ZERO, GAS_LIMIT);
        let reused = evm.execute_transaction(next.clone()).expect("the next transaction is valid");
        let fresh = self::evm(ErrorInjectingDatabase::new(db()))
            .execute_transaction(next)
            .expect("the next transaction is valid");
        assert!(fresh.result.is_success(), "{row}: {:?}", fresh.result);
        assert_same_outcome(&reused, &fresh, &row);
    }
}

/// A transaction run through `transact_one` whose child's read fails after its own frame wrote a
/// slot leaves none of its writes in the journal — its nonce bump, its fee, the slot — and the
/// transaction the journal held before it stays: `transact_one` leaves the journal to its caller
/// on an error as on a success, and the failure takes back only its own transaction.
#[test]
fn test_a_failed_transact_one_keeps_the_transaction_before_it_and_none_of_its_own_writes() {
    let mut db = ErrorInjectingDatabase::new(database());
    db.fail_on_storage = Some((CHILD, U256::ONE));
    let mut evm = evm(db);
    let earlier = evm
        .transact_one(call(EARLIER_SENDER, KEPT, U256::ZERO, GAS_LIMIT))
        .expect("the earlier transaction is valid");
    assert!(earlier.is_success(), "{earlier:?}");
    let error =
        evm.transact_one(call(SENDER, WRITER, U256::ZERO, GAS_LIMIT)).expect_err("the read fails");
    assert!(format!("{error:?}").contains("injected storage()"), "{error:?}");

    let state = evm.finalize();
    assert_eq!(state[&EARLIER_SENDER].info.nonce, 1, "the earlier transaction's nonce bump");
    assert_eq!(state[&KEPT].storage[&U256::ZERO].present_value, U256::ONE, "its slot");
    let sender = &state[&SENDER];
    assert_eq!(
        (sender.info.nonce, sender.info.balance),
        (0, U256::from(10u64.pow(18))),
        "the failed transaction's nonce bump and fee"
    );
    assert!(!sender.is_touched(), "the failed transaction's sender is touched by nothing kept");
    let written = state.get(&WRITER).and_then(|writer| writer.storage.get(&U256::ZERO));
    assert!(
        written.is_none_or(|slot| slot.present_value == slot.original_value),
        "the failed transaction's slot: {written:?}"
    );
}

/// A deposit run through `transact_one` whose validation refuses it is a failed deposit, a result:
/// its nonce bump and mint stay in the journal, as op-revm commits them, and the journal hands them
/// out when the caller finalizes it.
#[test]
fn test_a_failed_deposit_through_transact_one_keeps_its_nonce_bump_and_mint() {
    const MINT: u128 = 7;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE - 1);
    let mut evm = evm(ErrorInjectingDatabase::new(database())).with_tx_runtime_limits(limits);
    let result =
        evm.transact_one(over_the_limit_deposit(SENDER, MINT)).expect("the deposit is included");
    assert!(
        matches!(result, ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }),
        "{result:?}"
    );
    let state = evm.finalize();
    let sender = &state[&SENDER].info;
    assert_eq!(sender.nonce, 1, "the failed deposit's nonce bump");
    assert_eq!(sender.balance, U256::from(10u64.pow(18)) + U256::from(MINT), "its mint");
}

/// A deposit from `sender` minting `mint`, whose body is over a data-size limit of one byte below
/// [`TX_BODY_SIZE`]: validation refuses it before it reads its sender's account.
fn over_the_limit_deposit(sender: Address, mint: u128) -> MegaTransaction {
    let mut tx = op_transaction(TxEnv {
        caller: sender,
        kind: TxKind::Call(KEPT),
        gas_limit: 100_000,
        ..Default::default()
    });
    tx.deposit.source_hash = B256::repeat_byte(0x22);
    tx.deposit.mint = Some(mint);
    OpTx(tx)
}

/// op-revm turns a deposit's refusal into a failed deposit by reading its sender's account, and a
/// read the database fails there is an error out of that recovery, which skips op-revm's own
/// cleanup of the per-transaction caches. The engine's cleanup runs on every path out of the
/// handler's error handling, so the error leaves the caches empty, the frame stack cleared, the
/// error slot empty and the journal with no transaction open, and the EVM runs the next transaction
/// as a fresh one does.
///
/// No path fills the caches and then fails that read — a deposit computes no L1 cost and runs no
/// frame before its sender is read — so the test fills them itself: the cleanup makes no
/// assumption about what the failed call left.
#[test]
fn test_an_error_in_the_failed_deposit_recovery_leaves_the_evm_clean() {
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE - 1);
    let mut db = ErrorInjectingDatabase::new(database());
    db.fail_on_account = Some(SENDER);
    let mut evm = evm(db).with_tx_runtime_limits(limits);
    evm.ctx_mut().chain_mut().tx_l1_cost = Some(U256::from(1_000));
    evm.ctx_mut().local_mut().shared_memory_buffer().borrow_mut().resize(64, 0xff);
    evm.ctx_mut().local_mut().set_precompile_error_context("left behind".into());

    let error = evm
        .transact_one(over_the_limit_deposit(SENDER, 7))
        .expect_err("the recovery's read of the sender fails");
    assert!(format!("{error:?}").contains("injected basic()"), "{error:?}");
    assert_eq!(evm.ctx_mut().chain_mut().tx_l1_cost, None, "the cached L1 cost");
    assert!(
        evm.ctx_mut().local_mut().shared_memory_buffer().borrow().is_empty(),
        "the shared memory"
    );
    assert_eq!(evm.ctx_mut().local_mut().take_precompile_error_context(), None);
    assert_eq!(EvmTr::frame_stack(&mut evm).index(), None, "the frame stack");
    assert!(evm.ctx_mut().error().is_ok(), "the error slot");
    let journal = evm.ctx_mut().journal_mut();
    assert_eq!((journal.depth, journal.journal.len()), (0, 0), "no transaction is open");

    // What the refused deposit loaded is the caller's to drop, as after any `transact_one`; the
    // next transaction runs under the spec's limits.
    evm.ctx_mut().db_mut().fail_on_account = None;
    let _ = evm.finalize();
    evm.set_tx_runtime_limits(EvmTxRuntimeLimits::default());
    let next = call(SENDER, KEPT, U256::ZERO, GAS_LIMIT);
    let reused = evm.execute_transaction(next.clone()).expect("the next transaction is valid");
    let fresh = self::evm(ErrorInjectingDatabase::new(database()))
        .execute_transaction(next)
        .expect("the next transaction is valid");
    assert_same_outcome(&reused, &fresh, "after the recovery's error");
}
