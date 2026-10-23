//! What a block counts of the transactions it packed.

use alloy_consensus::transaction::Recovered;
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{transfer_log, BytecodeBuilder},
    BlockGasCounters, MegaTxEnvelope,
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

/// A gas limit for a call to the writing contract: 1,000,000 of regular gas on top of what its
/// slot, its write record, its log and its body cost at the byte prices in effect.
fn writer_gas() -> u64 {
    1_000_000 +
        common::slot_state_gas() +
        mega_evm::history_gas(
            mega_evm::TX_BODY_SIZE + mega_evm::WRITE_RECORD_SIZE + mega_evm::LOG_BASE_SIZE + 64,
        )
        .expect("the bytes have a price")
}

fn state_with_writer() -> State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(CONTRACT, writing_contract());
    State::builder().with_database(db).build()
}

/// A transfer to an account with no code.
fn transfer(nonce: u64) -> Recovered<MegaTxEnvelope> {
    common::recovered(common::tx(nonce, RECIPIENT, Bytes::new(), common::empty_call_gas()))
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
    for tx in [transfer(0), user_tx(1, writer_gas()), transfer(2)] {
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
    executor.execute_transaction(&user_tx(0, writer_gas())).expect("the transaction executes");

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
        executor.execute_transaction(&user_tx(nonce, writer_gas())).expect("nothing refuses these");
    }

    let counters = *executor.gas();
    assert_eq!(counters.state, common::slot_state_gas(), "the one fresh slot's state gas");
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
        mega_evm::HISTORY_ALLOWANCE_BYTES;
    assert_eq!(counters.history_bytes, 2 * per_tx);
    assert_eq!(
        counters.history_bytes * mega_evm::constants::COST_PER_HISTORY_BYTE - counters.history,
        2 * mega_evm::history_allowance_gas(),
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

/// A transaction that runs out of gas before its first frame, here on EIP-2780's charge for the
/// account its value would create, is billed its whole gas limit everywhere the limit is counted:
/// the sender pays it at the gas price, the fee recipients are credited the same amount, the
/// receipt's cumulative gas and the block's gas used are the gas limit, and the block's history
/// ledger takes the body's history and nothing for the record the frame never made.
#[test]
fn test_an_out_of_gas_before_the_first_frame_bills_the_whole_gas_limit_to_the_block() {
    use alloy_primitives::TxKind;
    use op_revm::constants::{BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT};
    use revm::Database as _;

    // With a state byte at nothing the phase has no charge of its own to fall short of.
    if common::state_is_free() {
        return;
    }
    const FRESH: Address = address!("0x3000000000000000000000000000000000000005");
    const GAS_PRICE: u128 = 1_000_000;
    let transfer = |gas_limit: u64| {
        common::legacy_from(
            common::CALLER,
            0,
            TxKind::Call(FRESH),
            U256::from(1),
            Bytes::new(),
            gas_limit,
            GAS_PRICE,
        )
    };

    // What the transfer spends when it has room, on a block of its own: one gas less falls short
    // on the last charge of the runtime gas phase.
    let mut state = common::state();
    let mut measuring = executor(&mut state, common::unlimited_ctx());
    measuring.apply_pre_execution_changes().expect("the block starts");
    let room = 1_000_000 + common::new_account_state_gas() + common::body_history(0);
    let outcome = measuring.run_transaction(&transfer(room)).expect("the transfer runs");
    assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);
    let gas_limit = outcome.inner.gas.gas_used - 1;
    drop(measuring);

    let mut state = common::state();
    let fee_recipients =
        [Address::ZERO, BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT];
    let balance = |state: &mut State<mega_evm::test_utils::MemoryDatabase>, address| {
        state.basic(address).expect("the account is read").map_or(U256::ZERO, |info| info.balance)
    };
    let sender_before = balance(&mut state, common::CALLER);
    let recipients_before: U256 = fee_recipients.iter().map(|a| balance(&mut state, *a)).sum();

    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");
    let outcome = executor.run_transaction(&transfer(gas_limit)).expect("it is included");
    assert!(outcome.inner.result.is_halt(), "{:?}", outcome.inner.result);
    let gas = outcome.inner.gas;
    assert_eq!(gas.gas_used, gas_limit, "the whole gas limit burns");
    assert_eq!(gas.reservoir_remaining, 0, "nothing comes back from a reservoir");
    assert_eq!(gas.history, common::body_history(0), "the history ledger reads the body");
    assert_eq!(gas.history_bytes, mega_evm::TX_BODY_SIZE);
    assert_eq!(gas.state, 0, "the account was not created");
    executor.commit_transaction_outcome(outcome).expect("the halt is included");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.gas_used, gas_limit, "the block's gas used");
    assert_eq!(result.receipts()[0].cumulative_gas_used(), gas_limit, "the receipt");
    let mut counters = BlockGasCounters::default();
    counters.record(&gas);
    assert_eq!(result.gas, counters);
    assert_eq!(result.gas.history, common::body_history(0), "the block's history ledger");

    let paid = U256::from(gas_limit) * U256::from(GAS_PRICE);
    assert_eq!(
        sender_before - balance(&mut state, common::CALLER),
        paid,
        "the sender pays its whole gas limit, and its value does not move"
    );
    let recipients_after: U256 = fee_recipients.iter().map(|a| balance(&mut state, *a)).sum();
    assert_eq!(recipients_after - recipients_before, paid, "the fee recipients are credited it");
    assert_eq!(balance(&mut state, FRESH), U256::ZERO, "the recipient was not created");
}

/// The pre-block calls are the protocol's own work, and no transaction of the block: what they
/// write is counted towards none of the block's budgets. The EIP-2935 call stores the parent's
/// hash and the EIP-4788 call the block's timestamp and its beacon root, each in a fresh slot, a
/// write record and a unit of state growth were a transaction to make it, and the block's usage
/// stays zero.
#[test]
fn test_the_pre_block_calls_count_towards_no_block_budget() {
    use alloy_eips::{
        eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
        eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    };
    use alloy_primitives::B256;
    use revm::Database as _;

    // The ring buffers both contracts store in.
    const BUFFER: u64 = 8_191;
    let (parent, root) = (B256::repeat_byte(0x11), B256::repeat_byte(0x22));
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    db.set_account_code(BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone());
    let mut state = State::builder().with_database(db).build();
    let slots = [
        (HISTORY_STORAGE_ADDRESS, U256::from((common::BLOCK_NUMBER - 1) % BUFFER), parent.into()),
        (
            BEACON_ROOTS_ADDRESS,
            U256::from(common::BLOCK_TIMESTAMP % BUFFER),
            U256::from(common::BLOCK_TIMESTAMP),
        ),
        (BEACON_ROOTS_ADDRESS, U256::from(common::BLOCK_TIMESTAMP % BUFFER + BUFFER), root.into()),
    ];
    for (address, slot, _) in slots {
        assert_eq!(state.storage(address, slot).unwrap(), U256::ZERO, "a fresh slot");
    }
    let ctx = mega_evm::MegaBlockExecutionCtx::new(
        parent,
        Some(root),
        Bytes::new(),
        mega_evm::BlockLimits::no_limits(),
    );
    let mut executor = executor(&mut state, ctx);
    executor.apply_pre_execution_changes().expect("the block starts");
    assert_eq!(executor.limiter().usage, mega_evm::LimitUsage::default());
    assert_eq!(*executor.gas(), BlockGasCounters::default());
    drop(executor);
    for (address, slot, value) in slots {
        assert_eq!(state.storage(address, slot).unwrap(), value, "the pre-block call stored it");
    }
}
