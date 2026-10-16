//! What a block counts of the transactions it packed.

use alloy_consensus::transaction::Recovered;
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{transfer_log, BytecodeBuilder},
    BlockGasCounters, BlockLimits, MegaTxEnvelope,
};
use revm::{
    bytecode::opcode::{CALL, LOG0, LOG3, POP, PUSH0},
    database::State,
};

use crate::common::{self, executor, user_tx, CONTRACT};

/// An account with no code, to transfer to.
const RECIPIENT: Address = address!("0x3000000000000000000000000000000000000003");

/// An account whose code emits an event when it is paid.
const RECEIVER: Address = address!("0x3000000000000000000000000000000000000004");

/// Code that writes a slot and emits a log, so a transaction touching it spends on more than
/// plain execution once the mechanisms that price those land.
fn writing_contract() -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(7))
        .push_number(64_u32)
        .push_number(0_u8)
        .append(LOG0)
        .stop()
        .build()
}

fn state_with_writer() -> State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(CONTRACT, writing_contract());
    State::builder().with_database(db).build()
}

/// A transfer to an account with no code.
fn transfer(nonce: u64) -> Recovered<MegaTxEnvelope> {
    common::recovered(common::tx(nonce, RECIPIENT, Bytes::new(), 100_000))
}

/// The block's three ledgers are the sum of its transactions', each on its own ledger, and the
/// block's gas used stays the sum of its receipts.
#[test]
fn test_a_block_sums_each_ledger_of_its_transactions() {
    let mut state = state_with_writer();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let mut expected = BlockGasCounters::default();
    let mut expected_gas_used = 0;
    for tx in [transfer(0), user_tx(1, 1_000_000), transfer(2)] {
        let outcome = executor.run_transaction(&tx).expect("the transaction executes");
        expected.record(&outcome.gas);
        expected_gas_used += outcome.gas.gas_used;
        executor.commit_transaction_outcome(outcome).expect("the block has room");
    }
    // A deposit counts on the same ledgers as any other transaction.
    let deposit = common::deposit_tx(Bytes::new(), 100_000);
    let outcome = executor.run_transaction(&deposit).expect("the deposit executes");
    expected.record(&outcome.gas);
    expected_gas_used += outcome.gas.gas_used;
    executor.commit_transaction_outcome(outcome).expect("the block has room");

    assert_eq!(*executor.gas(), expected);
    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.gas, expected);
    assert_eq!(result.gas_used, expected_gas_used, "the header's gas used is its receipts' sum");
    assert!(result.gas.execution > 0, "execution gas is what these transactions spend");
}

/// The counters and the footprint ride on the block's result, where the node reads them.
#[test]
fn test_the_result_carries_the_counters_and_the_footprint() {
    let mut state = state_with_writer();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&user_tx(0, 1_000_000)).expect("the transaction executes");

    let usage = executor.limiter().usage;
    let gas = *executor.gas();
    let footprint = executor.limiter().block_da_footprint_used;
    let (_, result) = executor.finish_with_counters().expect("the block finishes");

    assert_eq!(result.gas, gas);
    assert_eq!(result.usage, usage);
    assert_eq!(result.blob_gas_used, footprint);
    assert_eq!(result.receipts().len(), 1);
    assert!(result.usage.write_records > 0, "the transaction wrote to state");
}

/// The execution-gas dimension of the block: the transaction that crosses the limit is packed,
/// and the next one is refused before it runs.
#[test]
fn test_the_execution_limit_packs_the_crossing_transaction_and_skips_the_next() {
    let mut state = state_with_writer();

    // What one of these transactions spends on the execution ledger, so the block below has room
    // for exactly one of them.
    let spent = {
        let mut probe_state = state_with_writer();
        let mut probe = executor(&mut probe_state, common::unlimited_ctx());
        probe.apply_pre_execution_changes().expect("the block starts");
        probe.execute_transaction(&user_tx(0, 1_000_000)).expect("the probe executes");
        probe.gas().execution
    };
    assert!(spent > 0);

    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_execution_gas_limit(spent - 1)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    executor
        .execute_transaction(&user_tx(0, 1_000_000))
        .expect("the transaction that crosses the limit is still packed");
    assert!(executor.gas().execution > spent - 1, "the block has crossed its limit");

    let err = executor
        .execute_transaction(&user_tx(1, 1_000_000))
        .expect_err("the block has no execution gas left");
    assert!(format!("{err}").contains("Block execution gas limit reached"), "{err}");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
}

/// A deposit is not a transaction the builder chose: the block derived from L1 must include it,
/// so the execution-gas limit never refuses one, however far past it the block is. It still
/// counts, so an ordinary transaction after the deposits finds the room they used.
#[test]
fn test_a_deposit_is_never_refused_by_the_execution_limit() {
    let deposit = || common::deposit_tx_to(RECIPIENT, Bytes::new(), 1_000_000);

    // What one deposit spends on the execution ledger, so two of them cross the limit below.
    let spent = {
        let mut probe_state = state_with_writer();
        let mut probe = executor(&mut probe_state, common::unlimited_ctx());
        probe.apply_pre_execution_changes().expect("the block starts");
        probe.execute_transaction(&deposit()).expect("the probe executes");
        probe.gas().execution
    };
    assert!(spent > 0);

    let mut state = state_with_writer();
    let mut executor = executor(
        &mut state,
        common::block_ctx(
            BlockLimits::no_limits().with_block_execution_gas_limit(spent + spent / 2),
        ),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    executor.execute_transaction(&deposit()).expect("the block has room");
    executor.execute_transaction(&deposit()).expect("the deposit that crosses the limit");
    assert_eq!(executor.gas().execution, 2 * spent, "two deposits crossed the limit between them");

    executor
        .execute_transaction(&deposit())
        .expect("a deposit is included whatever the block's execution gas");
    assert_eq!(executor.gas().execution, 3 * spent, "and it counts");

    // Each deposit bumped the sender's nonce, so this one is valid and refused for room alone.
    let err = executor
        .execute_transaction(&user_tx(3, 1_000_000))
        .expect_err("an ordinary transaction finds no execution gas left");
    assert!(format!("{err}").contains("Block execution gas limit reached"), "{err}");
    assert!(format!("{err}").contains(&format!("block_used={}", 3 * spent)), "{err}");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 3);
    assert_eq!(result.gas.execution, 3 * spent);
}

/// The state and history ledgers are counted, and with no limit configured nothing refuses a
/// transaction on them: history has no block limit, and the state ledger's is unlimited unless a
/// node sets one.
///
/// The history a block counts is the sum of its transactions', which is what makes the block's
/// column readable: three transactions carrying the same body count three bodies.
#[test]
fn test_the_state_and_history_ledgers_refuse_nothing() {
    let mut state = state_with_writer();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    for nonce in 0..3 {
        executor.execute_transaction(&user_tx(nonce, 1_000_000)).expect("nothing refuses these");
    }

    let counters = *executor.gas();
    assert!(counters.state > 0, "the Satin gas table prices state gas, and these writes draw it");
    // Each transaction carries its body and emits a log over 64 bytes. Only the first writes the
    // slot: the two after it find it already holding the value, which is no write and no record.
    let bytes = 3 * mega_evm::TX_BODY_SIZE +
        mega_evm::WRITE_RECORD_SIZE +
        3 * (mega_evm::LOG_BASE_SIZE + 64);
    // Each is charged where it is made, so the gas is the sum of the three prices.
    let priced = |bytes| mega_evm::history_gas(bytes).expect("the bytes have a price");
    assert_eq!(
        counters.history,
        3 * priced(mega_evm::TX_BODY_SIZE) +
            priced(mega_evm::WRITE_RECORD_SIZE) +
            3 * priced(mega_evm::LOG_BASE_SIZE + 64),
        "three bodies, three logs and the one write record",
    );
    assert_eq!(counters.history_bytes, bytes, "and the block reports the bytes beside the gas");
}

/// A contract that sends one wei to [`RECEIVER`] with the gas a `transfer()` forwards, and a
/// receiver that emits the one three-topic event a history allowance pays for.
fn state_with_allowance() -> State<mega_evm::test_utils::MemoryDatabase> {
    let sender = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(1_u8)
        .push_address(RECEIVER)
        .push_number(2_300_u32)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let receiver = BytecodeBuilder::default()
        .push_number(1_u8)
        .push_number(2_u8)
        .push_number(3_u8)
        .push_number(32_u8)
        .push_number(0_u8)
        .append(LOG3)
        .stop()
        .build();
    let mut db = common::database();
    db.set_account_code(CONTRACT, sender);
    db.set_account_balance(CONTRACT, U256::from(1_000));
    db.set_account_code(RECEIVER, receiver);
    State::builder().with_database(db).build()
}

/// A block reports the history bytes its transactions appended beside the history gas they paid,
/// and the two columns part by what the history allowances paid: here one event per transaction,
/// which the receiver's allowance paid in full. The transfer log each transfer leaves is in the
/// receipt and in the block's data size, and in neither history column.
#[test]
fn test_a_block_reports_the_bytes_its_history_gas_does_not_cover() {
    if !mega_evm::active_satin_prices().is_constants() {
        return;
    }
    let mut state = state_with_allowance();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    for nonce in 0..2 {
        let outcome = executor.run_transaction(&user_tx(nonce, 1_000_000)).expect("it executes");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        let logs = outcome.result.logs();
        assert_eq!(logs.len(), 2, "the transfer's log and the receiver's event");
        assert_eq!(logs[0], transfer_log(CONTRACT, RECEIVER, U256::from(1)));
        executor.commit_transaction_outcome(outcome).expect("the block has room");
    }

    let counters = *executor.gas();
    // Each transaction: its body, the transfer's two records and the receiver's event.
    let per_tx = mega_evm::TX_BODY_SIZE +
        2 * mega_evm::WRITE_RECORD_SIZE +
        mega_evm::STORAGE_CALL_STIPEND_BYTES;
    assert_eq!(counters.history_bytes, 2 * per_tx);
    assert_eq!(
        counters.history_bytes * mega_evm::constants::COST_PER_HISTORY_BYTE - counters.history,
        2 * mega_evm::storage_call_stipend(),
        "the allowances paid for both events, and no gas ledger has them",
    );

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.gas, counters, "the result carries both columns");
    assert_eq!(
        result.usage.data_size,
        2 * (per_tx + mega_evm::TRANSFER_LOG_SIZE),
        "the data size counts the transfer logs the history columns leave out",
    );
}

/// A transaction bound by its floor adds its floor to the block's execution gas. Its history comes
/// out before the floor applies, so what the block counts is neither the fork's figure, which
/// carries history, nor that figure less history, which falls below the floor.
#[test]
fn test_a_floor_bound_transaction_adds_its_floor_to_the_block() {
    if !mega_evm::active_satin_prices().is_constants() {
        return;
    }
    let mut state = common::state();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let tx = common::user_tx_with_input(0, Bytes::from(vec![0; 1_000]), 1_000_000);
    let outcome = executor.run_transaction(&tx).expect("the transaction executes");
    let gas = outcome.gas;
    let fork_figure = outcome.result.gas().block_regular_gas_used();
    assert!(gas.floor > gas.regular, "{gas:?}: a kilobyte of calldata and nothing run");
    executor.commit_transaction_outcome(outcome).expect("the block has room");

    assert_eq!(executor.gas().execution, gas.floor);
    assert!(fork_figure > gas.floor, "the fork's figure carries the history");
    assert!(fork_figure - gas.history < gas.floor, "and less history it falls below the floor");
}
