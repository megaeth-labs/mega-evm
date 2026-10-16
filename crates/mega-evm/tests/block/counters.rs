//! What a block counts of the transactions it packed.

use alloy_consensus::transaction::Recovered;
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{test_utils::BytecodeBuilder, BlockGasCounters, BlockLimits, MegaTxEnvelope};
use revm::{bytecode::opcode::LOG0, database::State};

use crate::common::{self, executor, user_tx, CONTRACT};

/// An account with no code, to transfer to.
const RECIPIENT: Address = address!("0x3000000000000000000000000000000000000003");

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

/// The state and history ledgers are counted, and nothing refuses a transaction on them: their
/// block limits belong to the mechanisms that bring those ledgers.
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
    assert_eq!(counters.history, 0, "history gas arrives with the mechanism that meters it");
}
