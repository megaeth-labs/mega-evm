//! A delayed commit holds an outcome to the state its sender had when it executed.
//!
//! `commit_transaction_outcome` commits an outcome a builder executed earlier, after other
//! commits may have moved the state it read. A commit writes the sender's account as the
//! transaction left it, so an outcome whose sender's nonce or balance moved since it executed would
//! write over what the other commit did: the commit refuses it. Any other read the outcome made is
//! the builder's to keep fresh, which the flow a sequencer builds with — an Oracle-reading
//! transaction executed, then the system-address transaction that writes the Oracle executed and
//! committed first — relies on.

use alloy_consensus::transaction::Recovered;
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    MegaTxEnvelope,
};
use revm::Database;

use crate::common::{self, body_history, new_account_state_gas, CALLER};

/// A second funded sender.
const SENDER: Address = address!("0x2000000000000000000000000000000000000005");

/// A recipient of value, absent from the pre-state.
const PAYEE_B: Address = address!("0x1000000000000000000000000000000000000006");

/// Another recipient of value, absent from the pre-state.
const PAYEE_C: Address = address!("0x1000000000000000000000000000000000000007");

/// What every test transaction is given: room for a new account and its records on top of the
/// body's history, at the byte prices in effect.
fn gas_limit(calldata: u64) -> u64 {
    1_000_000 +
        new_account_state_gas() +
        body_history(calldata) +
        mega_evm::write_record_history_gas(4).expect("the records have a price")
}

/// A legacy transaction from `sender` at `nonce` to `to`, carrying `value` and `input`.
fn legacy(
    sender: Address,
    nonce: u64,
    to: Address,
    value: u64,
    input: Bytes,
) -> Recovered<MegaTxEnvelope> {
    let limit = gas_limit(input.len() as u64);
    common::legacy_from(sender, nonce, TxKind::Call(to), U256::from(value), input, limit, 1_000_000)
}

/// A system-address transaction at `nonce` writing `value` into the Oracle's slot 0.
fn oracle_write(nonce: u64, value: u8) -> Recovered<MegaTxEnvelope> {
    let input = IOracle::setSlotsCall {
        slots: vec![U256::ZERO],
        values: vec![B256::with_last_byte(value)],
    }
    .abi_encode();
    legacy(MEGA_SYSTEM_ADDRESS, nonce, ORACLE_CONTRACT_ADDRESS, 0, input.into())
}

/// The block's state: the test callers funded.
fn state() -> revm::database::State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_balance(SENDER, U256::from(1_000_000_000_000_000_u64));
    revm::database::State::builder().with_database(db).build()
}

/// Asserts `err` is the refusal of an outcome its sender's state moved under.
fn assert_stale(err: &alloy_evm::block::BlockExecutionError, sender: Address) {
    let message = err.to_string();
    assert!(message.contains("stale"), "{message}");
    assert!(message.contains(&sender.to_string()), "{message}");
}

/// Two candidates from one sender at one nonce: the first commits, and the second, which executed
/// against the nonce and balance the first then moved, is refused. Committed, it would debit the
/// sender once and credit both recipients.
#[test]
fn test_a_second_outcome_at_the_same_nonce_is_refused() {
    let mut state = state();
    let mut executor = common::executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let to_b = executor.run_transaction(&legacy(CALLER, 0, PAYEE_B, 7, Bytes::new()));
    let to_c = executor.run_transaction(&legacy(CALLER, 0, PAYEE_C, 9, Bytes::new()));
    let (to_b, to_c) = (to_b.expect("executes"), to_c.expect("executes"));
    executor.commit_transaction_outcome(to_b).expect("fresh");
    let err = executor.commit_transaction_outcome(to_c).expect_err("stale");
    assert_stale(&err, CALLER);

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1, "the refused outcome is not in the block");
    drop(result);
    assert_eq!(state.basic(CALLER).unwrap().unwrap().nonce, 1);
    assert_eq!(state.basic(PAYEE_B).unwrap().unwrap().balance, U256::from(7));
    assert!(state.basic(PAYEE_C).unwrap().is_none_or(|payee| payee.balance.is_zero()));
}

/// A candidate whose sender received value from a transaction committed since it executed is
/// refused: committing it would write back the balance it read, and lose what the other commit
/// credited.
#[test]
fn test_an_outcome_whose_senders_balance_moved_is_refused() {
    let mut state = state();
    let mut executor = common::executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let waiting = executor.run_transaction(&legacy(SENDER, 0, PAYEE_B, 1, Bytes::new())).unwrap();
    let pays_the_sender =
        executor.run_transaction(&legacy(CALLER, 0, SENDER, 5, Bytes::new())).unwrap();
    executor.commit_transaction_outcome(pays_the_sender).expect("fresh");
    let err = executor.commit_transaction_outcome(waiting).expect_err("stale");
    assert_stale(&err, SENDER);
}

/// The system-address transaction is held to the system address's state: of two executed at the
/// same nonce, the one committed second is refused.
#[test]
fn test_a_second_system_transaction_at_the_same_nonce_is_refused() {
    let mut state = state();
    let mut executor = common::executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let first = executor.run_transaction(&oracle_write(0, 1)).unwrap();
    let second = executor.run_transaction(&oracle_write(0, 2)).unwrap();
    assert!(first.result.is_success() && second.result.is_success());
    executor.commit_transaction_outcome(first).expect("fresh");
    let err = executor.commit_transaction_outcome(second).expect_err("stale");
    assert_stale(&err, MEGA_SYSTEM_ADDRESS);
}

/// The flow a sequencer builds with: a user transaction that reads the Oracle executes first, the
/// system-address transaction that writes the Oracle executes after it and commits before it, and
/// the user transaction then commits. Neither commit moved the other's sender, so both are
/// included, in commit order; that the user transaction read the Oracle before the write is the
/// builder's to arrange.
#[test]
fn test_the_system_transaction_first_flow_commits() {
    let mut state = state();
    let mut executor = common::executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let read = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let reads = legacy(CALLER, 0, ORACLE_CONTRACT_ADDRESS, 0, read.into());
    let reads = executor.run_transaction(&reads).expect("executes");
    let writes = executor.run_transaction(&oracle_write(0, 3)).expect("executes");
    assert!(reads.result.is_success() && writes.result.is_success());
    executor.commit_transaction_outcome(writes).expect("the system transaction commits first");
    executor.commit_transaction_outcome(reads).expect("and the reading transaction after it");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 2);
    drop(result);
    assert_eq!(
        state.storage(ORACLE_CONTRACT_ADDRESS, U256::ZERO).unwrap(),
        U256::from(3),
        "the system transaction's write is in the block"
    );
}

/// The trait's commit cannot refuse, and its contract is that an outcome commits before the next
/// transaction executes: a debug build checks a stale outcome there too, and trips on it.
#[cfg(debug_assertions)]
#[test]
fn test_the_trait_commit_trips_on_a_stale_outcome() {
    let mut state = state();
    let mut executor = common::executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let to_b = executor.run_transaction(&legacy(CALLER, 0, PAYEE_B, 7, Bytes::new()));
    let to_c = executor.run_transaction(&legacy(CALLER, 0, PAYEE_C, 9, Bytes::new()));
    BlockExecutor::commit_transaction(&mut executor, to_b.unwrap());
    let to_c = to_c.unwrap();
    let tripped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BlockExecutor::commit_transaction(&mut executor, to_c);
    }))
    .expect_err("the commit trips");
    let message = tripped
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| tripped.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(message.contains("stale"), "{message}");
}
