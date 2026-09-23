//! The block's state-gas limit: the transaction that reaches it is packed, every later one that
//! adds state gas is skipped, and one that adds none still fits.

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, Evm as _};
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP, test_utils::BytecodeBuilder, BlockLimits, MegaTxEnvelope,
};
use revm::{
    bytecode::opcode::{CALLDATALOAD, PUSH0, SSTORE},
    context::BlockEnv,
    database::State,
    Database,
};

use crate::common::{self, executor, CALLER, CHAIN_ID, CONTRACT};

/// Two more senders, so candidates of one block can be executed before any of them commits.
const CALLER2: Address = address!("0x2000000000000000000000000000000000000022");
const CALLER3: Address = address!("0x2000000000000000000000000000000000000033");

/// An account with no code: a call to it adds no state gas.
const EMPTY: Address = address!("0x3000000000000000000000000000000000000003");

/// Below the execution cap, where the reservoir is empty and state gas spills onto regular gas.
const GAS_LIMIT: u64 = 1_000_000;

/// Above the execution cap, where the reservoir pays state gas first.
const ABOVE_CAP: u64 = TX_GAS_LIMIT_CAP + 100_000_000;

/// Code that sets the slot the first calldata word names to one.
fn slot_writer() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1_u8)
        .append(PUSH0)
        .append(CALLDATALOAD)
        .append(SSTORE)
        .stop()
        .build()
}

fn state() -> State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    for caller in [CALLER2, CALLER3] {
        db.set_account_balance(caller, U256::from(1_000_000_000_000_000_u64));
    }
    State::builder().with_database(db).build()
}

/// A transaction from `caller` to `to`, carrying `input`.
fn tx_from(
    caller: Address,
    nonce: u64,
    to: Address,
    input: Bytes,
    gas_limit: u64,
) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input,
    };
    let hash = B256::from(U256::from(nonce) + (U256::from_be_slice(caller.as_slice()) << 64));
    Recovered::new_unchecked(
        MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, Signature::test_signature(), hash)),
        caller,
    )
}

/// A transaction from [`CALLER`] that fills `slot`, or leaves it as it is when it is already set.
fn writes(nonce: u64, slot: u64, gas_limit: u64) -> Recovered<MegaTxEnvelope> {
    let input = Bytes::from(U256::from(slot).to_be_bytes::<32>());
    tx_from(CALLER, nonce, CONTRACT, input, gas_limit)
}

/// The state gas a transaction that fills one fresh slot spends, read off a probe block.
fn one_slot() -> u64 {
    let mut state = state();
    let mut probe = executor(&mut state, common::unlimited_ctx());
    probe.apply_pre_execution_changes().expect("the block starts");
    probe.execute_transaction(&writes(0, 1, GAS_LIMIT)).expect("the probe executes");
    let slot = probe.gas().state;
    assert!(slot > 0, "a fresh slot draws state gas");
    slot
}

/// A block with room for one slot and a half: the second slot crosses the limit.
fn limits(slot: u64) -> BlockLimits {
    BlockLimits::no_limits().with_block_state_gas_limit(slot + slot / 2)
}

/// The n-th transaction finds the block below its limit and crosses it: it is packed. The
/// (n+1)-th adds state gas and is skipped with the state dimension's own error, leaving nothing
/// behind. A transaction after it that adds none still fits.
#[test]
fn test_the_crossing_transaction_is_packed_and_the_next_that_adds_state_is_skipped() {
    let slot = one_slot();
    let mut state = state();
    let mut executor = executor(&mut state, common::block_ctx(limits(slot)));
    executor.apply_pre_execution_changes().expect("the block starts");

    executor.execute_transaction(&writes(0, 1, GAS_LIMIT)).expect("the block has room");
    executor
        .execute_transaction(&writes(1, 2, GAS_LIMIT))
        .expect("the transaction that crosses the limit is still packed");
    assert_eq!(executor.gas().state, 2 * slot, "the block overshoots by that one transaction");

    let err = executor
        .execute_transaction(&writes(2, 3, GAS_LIMIT))
        .expect_err("the next transaction adds state gas");
    assert!(format!("{err}").contains("Block state gas limit reached"), "{err}");
    assert_eq!(
        executor.evm_mut().db_mut().storage(CONTRACT, U256::from(3)).unwrap(),
        U256::ZERO,
        "the skipped transaction wrote nothing",
    );

    // The same nonce again, now writing a slot that already holds its value: no state gas.
    executor
        .execute_transaction(&writes(2, 1, GAS_LIMIT))
        .expect("a transaction that adds no state gas still fits");
    executor
        .execute_transaction(&tx_from(CALLER, 3, EMPTY, Bytes::new(), GAS_LIMIT))
        .expect("and so does one that writes nothing at all");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 4);
    assert_eq!(result.gas.state, 2 * slot, "the skipped transaction counted nothing");
}

/// A deposit is not a transaction the builder chose: the block derived from L1 must include it,
/// so the state-gas limit never refuses one, however far past it the block is. It still counts,
/// so an ordinary transaction after the deposits finds the room they used.
#[test]
fn test_a_deposit_is_never_refused_by_the_state_gas_limit() {
    let slot = one_slot();
    let mut state = state();
    let mut executor = executor(&mut state, common::block_ctx(limits(slot)));
    executor.apply_pre_execution_changes().expect("the block starts");

    let slot_word = |slot: u64| Bytes::from(U256::from(slot).to_be_bytes::<32>());
    let deposit = |slot: u64| common::deposit_tx_to(CONTRACT, slot_word(slot), GAS_LIMIT);

    executor.execute_transaction(&deposit(1)).expect("the block has room");
    executor.execute_transaction(&deposit(2)).expect("the deposit that crosses the limit");
    assert_eq!(executor.gas().state, 2 * slot, "two deposits crossed the limit between them");

    executor
        .execute_transaction(&deposit(3))
        .expect("a deposit is included whatever the block's state gas");
    assert_eq!(executor.gas().state, 3 * slot, "and it counts");

    let err = executor
        .execute_transaction(&tx_from(CALLER2, 0, CONTRACT, slot_word(4), GAS_LIMIT))
        .expect_err("an ordinary transaction that adds state gas finds no room left");
    assert!(format!("{err}").contains("Block state gas limit reached"), "{err}");
    assert!(format!("{err}").contains(&format!("block_used={}", 3 * slot)), "{err}");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 3);
    assert_eq!(result.gas.state, 3 * slot);
}

/// A builder that executes candidates against the same pre-state and then picks among them gets
/// the same rule at commit: the block's state gas may have been reached while a candidate waited.
#[test]
fn test_the_state_gas_limit_is_checked_again_at_commit() {
    let slot = one_slot();
    let mut state = state();
    let limits = BlockLimits::no_limits().with_block_state_gas_limit(slot);
    let mut executor = executor(&mut state, common::block_ctx(limits));
    executor.apply_pre_execution_changes().expect("the block starts");

    let slot_word = |slot: u64| Bytes::from(U256::from(slot).to_be_bytes::<32>());
    let first = executor.run_transaction(&writes(0, 1, GAS_LIMIT)).expect("the block has room");
    let second = executor
        .run_transaction(&tx_from(CALLER2, 0, CONTRACT, slot_word(2), GAS_LIMIT))
        .expect("nothing has committed yet");
    let third = executor
        .run_transaction(&tx_from(CALLER3, 0, EMPTY, Bytes::new(), GAS_LIMIT))
        .expect("nothing has committed yet");
    assert!(second.gas.state > 0 && third.gas.state == 0);

    executor.commit_transaction_outcome(first).expect("the block has room");
    assert_eq!(executor.gas().state, slot, "the block has spent exactly its limit");

    let err = executor
        .commit_transaction_outcome(second)
        .expect_err("the block reached its state gas while this transaction waited");
    assert!(format!("{err}").contains("Block state gas limit reached"), "{err}");
    executor.commit_transaction_outcome(third).expect("a transaction that adds none still fits");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 2);
    assert_eq!(result.gas.state, slot);
}

/// Above the execution cap the reservoir pays the state gas, and the block counts and caps it the
/// same way: the ledger is the state gas spent, whichever pool paid it.
#[test]
fn test_the_state_gas_limit_counts_what_the_reservoir_paid() {
    let slot = one_slot();
    let mut state = state();
    let mut env = common::evm_env();
    env.block_env = BlockEnv { gas_limit: 10_000_000_000, ..env.block_env };
    let mut executor = common::executor_with_env(&mut state, common::block_ctx(limits(slot)), env);
    executor.apply_pre_execution_changes().expect("the block starts");

    for (nonce, slot_index) in [(0, 1), (1, 2)] {
        let outcome =
            executor.run_transaction(&writes(nonce, slot_index, ABOVE_CAP)).expect("it executes");
        assert_eq!(outcome.gas.state, slot, "the same state gas as below the cap");
        assert!(outcome.gas.reservoir_remaining > 0, "the reservoir paid it");
        assert_eq!(
            outcome.gas.reservoir_remaining,
            ABOVE_CAP - TX_GAS_LIMIT_CAP - outcome.gas.state - outcome.gas.history,
        );
        executor.commit_transaction_outcome(outcome).expect("the crossing transaction is packed");
    }

    let err = executor
        .execute_transaction(&writes(2, 3, ABOVE_CAP))
        .expect_err("the next transaction adds state gas");
    assert!(format!("{err}").contains("Block state gas limit reached"), "{err}");
    executor
        .execute_transaction(&writes(2, 1, ABOVE_CAP))
        .expect("a transaction that adds no state gas still fits");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 3);
    assert_eq!(result.gas.state, 2 * slot);
}
