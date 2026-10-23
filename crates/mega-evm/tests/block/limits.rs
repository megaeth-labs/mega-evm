//! What a block admits, and what it does with a transaction it does not.

use alloy_consensus::{transaction::Recovered, Transaction};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::BytecodeBuilder, BlockLimits, EnrichedMegaTx, MegaTransactionExt, MegaTxEnvelope,
};
use op_revm::constants::{
    DA_FOOTPRINT_GAS_SCALAR_OFFSET, DA_FOOTPRINT_GAS_SCALAR_SLOT, L1_BLOCK_CONTRACT,
};
use revm::{
    bytecode::opcode::{ADD, DUP1, LOG0, SLOAD, SSTORE},
    context::{result::ExecutionResult, BlockEnv, ContextTr},
    Database as _,
};

use crate::common::{self, executor, incompressible, user_tx, CALLER, CONTRACT};

const CALLER2: Address = address!("0x2000000000000000000000000000000000000003");
const CALLER3: Address = address!("0x2000000000000000000000000000000000000004");

/// Code that emits one log of `data_size` zero bytes, so every call to it keeps data-size bytes.
fn log_generating_contract(data_size: usize) -> Bytes {
    BytecodeBuilder::default()
        .push_number(data_size as u32)
        .push_number(0_u8)
        .append(LOG0)
        .stop()
        .build()
}

/// A gas limit for a call to the contract that logs `data_size` bytes: 1,000,000 of regular gas on
/// top of what the log and the body cost at the byte prices in effect.
fn log_call_gas(data_size: usize) -> u64 {
    1_000_000 +
        common::body_history(0) +
        mega_evm::history_gas(mega_evm::log_history_bytes(0, data_size as u64))
            .expect("the log has a price")
}

/// A state whose callee emits a log of `data_size` bytes on every call.
fn state_with_log_contract(
    data_size: usize,
) -> revm::database::State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(CONTRACT, log_generating_contract(data_size));
    revm::database::State::builder().with_database(db).build()
}

/// A transaction from `caller`, so transactions of one block can come from different senders.
fn tx_from(caller: Address, nonce: u64, gas_limit: u64) -> Recovered<MegaTxEnvelope> {
    Recovered::new_unchecked(common::tx(nonce, CONTRACT, Bytes::new(), gas_limit), caller)
}

/// The first transaction of `caller`, carrying `input`.
fn tx_with_input(caller: Address, input: Bytes, gas_limit: u64) -> Recovered<MegaTxEnvelope> {
    Recovered::new_unchecked(common::tx(0, CONTRACT, input, gas_limit), caller)
}

/// The block's data-size limit: the transaction that crosses it is packed, and the next one is
/// refused before it runs.
#[test]
fn test_block_custom_data_limit() {
    let mut state = state_with_log_contract(2_000);
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_data_limit(2_500)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let first = user_tx(0, log_call_gas(2_000));
    let gas = executor.execute_transaction(&first).expect("the first transaction is packed");
    assert!(gas.tx_gas_used() < first.gas_limit());

    let second = user_tx(1, log_call_gas(2_000));
    let gas = executor
        .execute_transaction(&second)
        .expect("the transaction that crosses the limit is packed too");
    assert!(gas.tx_gas_used() < second.gas_limit());
    assert!(executor.limiter().usage.data_size >= 2_500, "the block has crossed its limit");

    let err = executor
        .execute_transaction(&user_tx(2, log_call_gas(2_000)))
        .expect_err("the block has no data-size left");
    assert!(format!("{err}").contains("Block transactions data limit reached"), "{err}");
}

/// Code that adds one to each of the slots `1..=writes`, so every call keeps `writes` write
/// records whatever the calls before it wrote.
fn incrementing_contract(writes: u64) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for slot in 1..=writes {
        code = code
            .push_number(slot)
            .append(DUP1)
            .append(SLOAD)
            .push_number(1_u8)
            .append(ADD)
            .append(revm::bytecode::opcode::SWAP1)
            .append(SSTORE);
    }
    code.stop().build()
}

/// A gas limit for a call to the contract that increments `writes` fresh slots: 10,000,000 of
/// regular gas on top of what the slots, their records and the body cost at the byte prices in
/// effect.
fn incrementing_gas(writes: u64) -> u64 {
    10_000_000 +
        writes * common::slot_state_gas() +
        common::body_history(0) +
        mega_evm::write_record_history_gas(writes).expect("the records have a price")
}

/// A state whose callee keeps `writes` write records on every call.
fn state_with_incrementing_contract(
    writes: u64,
) -> revm::database::State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(CONTRACT, incrementing_contract(writes));
    revm::database::State::builder().with_database(db).build()
}

/// A contract that writes the fresh slots `from..from + count`, for a transaction that keeps that
/// many new slots and records.
fn fresh_writer(from: u64, count: u64) -> Bytes {
    (from..from + count)
        .fold(BytecodeBuilder::default(), |code, slot| code.sstore(U256::from(slot), U256::from(1)))
        .stop()
        .build()
}

/// A second contract, which a deposit calls.
const WRITER: Address = address!("0x1000000000000000000000000000000000000009");

/// A block over `state` with room in its gas for every transaction, whatever the slots cost,
/// packed under `policy`.
fn counting_executor(
    state: &mut revm::database::State<mega_evm::test_utils::MemoryDatabase>,
    policy: BlockLimits,
) -> common::TestExecutor<'_> {
    let mut env = common::evm_env();
    env.block_env.gas_limit = 10_000_000_000;
    common::executor_with_env(state, common::block_ctx(policy), env)
}

/// The block's KV count is the write records its transactions kept, summed, and its KV-update
/// budget packs the transaction that reaches it and refuses the next one before it runs: under a
/// builder's cap of 100, two transactions that keep fifty records each are packed and a third is
/// refused. A deposit is not refused, and counts.
#[test]
fn test_the_block_kv_budget_packs_the_transaction_that_reaches_it() {
    let mut state = state_with_incrementing_contract(50);
    state.database.set_account_code(WRITER, fresh_writer(1, 3));
    let mut executor =
        counting_executor(&mut state, BlockLimits::no_limits().with_block_kv_update_limit(100));
    executor.apply_pre_execution_changes().expect("the block starts");

    for nonce in 0..2 {
        executor
            .execute_transaction(&user_tx(nonce, incrementing_gas(50)))
            .unwrap_or_else(|err| panic!("transaction {nonce} is packed: {err}"));
    }
    assert_eq!(executor.limiter().usage.write_records, 100, "the budget is reached");
    let err = executor
        .execute_transaction(&user_tx(2, incrementing_gas(50)))
        .expect_err("the block has no KV updates left");
    assert!(format!("{err}").contains("Block KV update limit reached"), "{err}");

    let deposit = common::deposit_tx_to(WRITER, Bytes::new(), incrementing_gas(3));
    executor.execute_transaction(&deposit).expect("no packing budget refuses a deposit");
    assert_eq!(executor.limiter().usage.write_records, 103, "and it counts");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 3);
    assert_eq!(result.usage.write_records, 103, "the block reports its KV count");
}

/// The block's state growth is the new accounts and slots its transactions kept, and its
/// protocol budget is 1,000: a transaction that keeps 1,000 fresh slots reaches it and is packed,
/// the next ordinary transaction is refused before it runs, and a deposit after it is not, counts
/// its new slots, and leaves the block as full for the ordinary transactions that follow. The
/// block's gas limit, ten billion here, does not stand in for the budget.
#[test]
fn test_the_block_state_growth_budget_is_the_protocols_1000() {
    assert_eq!(mega_evm::constants::BLOCK_STATE_GROWTH_LIMIT, 1_000);
    let mut state = state_with_incrementing_contract(1_000);
    state.database.set_account_code(WRITER, fresh_writer(1, 3));
    let mut executor = counting_executor(&mut state, BlockLimits::no_limits());
    executor.apply_pre_execution_changes().expect("the block starts");

    // Each increment's regular gas, its cold load and its write, on top of what the helper gives.
    let gas = incrementing_gas(1_000) + 1_000 * 25_000;
    let outcome = executor
        .run_transaction(&user_tx(0, gas))
        .expect("the transaction that reaches the budget is packed");
    assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);
    executor.commit_transaction_outcome(outcome).expect("and committed");
    assert_eq!(executor.limiter().usage.state_growth, 1_000);
    let ordinary = || user_tx(1, incrementing_gas(1));
    let err = executor.execute_transaction(&ordinary()).expect_err("the budget is reached");
    assert!(format!("{err}").contains("Block state growth limit reached"), "{err}");

    let deposit = common::deposit_tx_to(WRITER, Bytes::new(), incrementing_gas(3));
    executor.execute_transaction(&deposit).expect("no packing budget refuses a deposit");
    assert_eq!(executor.limiter().usage.state_growth, 1_003, "the deposit's three new slots");
    let err = executor.execute_transaction(&ordinary()).expect_err("the block is still full");
    assert!(format!("{err}").contains("Block state growth limit reached"), "{err}");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 2);
    assert_eq!(result.usage.state_growth, 1_003, "the block reports its state growth");
}

/// Where more than one packing budget is reached, the refusal names the first of data size, KV
/// updates and state growth; the transaction is refused whichever it names.
#[test]
fn test_a_block_full_on_several_budgets_names_them_in_order() {
    let cases = [
        (
            BlockLimits::no_limits()
                .with_block_txs_data_limit(1)
                .with_block_kv_update_limit(1)
                .with_block_state_growth_limit(1),
            "Block transactions data limit reached",
        ),
        (
            BlockLimits::no_limits().with_block_kv_update_limit(1).with_block_state_growth_limit(1),
            "Block KV update limit reached",
        ),
        (BlockLimits::no_limits().with_block_state_growth_limit(1), "Block state growth limit"),
    ];
    for (policy, named) in cases {
        let mut state = state_with_incrementing_contract(1);
        let mut executor = counting_executor(&mut state, policy);
        executor.apply_pre_execution_changes().expect("the block starts");
        executor
            .execute_transaction(&user_tx(0, incrementing_gas(1)))
            .expect("the first transaction reaches every budget the case sets");
        let err = executor
            .execute_transaction(&user_tx(1, incrementing_gas(1)))
            .expect_err("the block is full");
        assert!(format!("{err}").contains(named), "{named}: {err}");
    }
}

/// A transaction refused before it runs changes nothing: no receipt, and no state.
#[test]
fn test_block_no_state_commit_on_limit_exceeded() {
    let mut db = common::database();
    db.set_account_code(
        CONTRACT,
        BytecodeBuilder::default().sstore(U256::ZERO, U256::from(42)).stop().build(),
    );
    let mut state = revm::database::State::builder().with_database(db).build();
    // A limit of zero is crossed before the block starts, so nothing is admitted.
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_data_limit(0)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    executor
        .execute_transaction(&user_tx(0, 10_000_000))
        .expect_err("a block with no data-size admits nothing");

    let stored = executor
        .evm_mut()
        .ctx_mut()
        .db_mut()
        .storage(CONTRACT, U256::ZERO)
        .expect("the slot is readable");
    assert_eq!(stored, U256::ZERO, "the transaction's write was never committed");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert!(result.receipts().is_empty());
}

/// The per-transaction data-size limit is not a block limit: a deposit whose body crosses it fails
/// validation, as any transaction with that body does, and is included as a failed deposit with a
/// failed receipt.
#[test]
fn test_a_deposit_over_the_transaction_data_size_limit_is_still_included() {
    let limit = mega_evm::TX_BODY_SIZE + 10;
    let mut state = common::state();
    let mut executor = common::executor_with_limits(
        &mut state,
        common::loosest()
            .with_tx_runtime_limits(common::loosest_tx().with_tx_data_size_limit(limit)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let deposit = common::deposit_tx(Bytes::from(vec![0xab; 11]), 1_000_000);
    let outcome = executor.run_transaction(&deposit).expect("a deposit is not refused");
    assert!(
        matches!(
            outcome.inner.result,
            ExecutionResult::Halt { reason: op_revm::OpHaltReason::FailedDeposit, .. }
        ),
        "{:?}",
        outcome.inner.result
    );
    assert_eq!(outcome.inner.limit_exceeded, None, "the failure is what the deposit reports");
    assert_eq!(outcome.inner.usage.data_size, limit + 1, "its body stays counted");
    executor.commit_transaction_outcome(outcome).expect("the deposit is included");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
    assert!(!result.receipts()[0].status(), "the failed deposit's receipt reports a failure");
}

/// With no size limit configured, a transaction of any size is admitted.
#[test]
fn test_block_tx_size_limit_default_unlimited() {
    let mut state = state_with_log_contract(100);
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let gas_limit = 3_000_000 + common::body_history(20_000) + log_call_gas(100);
    let big = common::user_tx_with_input(0, incompressible(20_000), gas_limit);
    assert!(MegaTransactionExt::tx_size(&big) > 20_000);

    executor.execute_transaction(&big).expect("no limit, no refusal");
}

/// A first transaction whose body is larger than the block's limit is refused, and the block
/// stays empty.
#[test]
fn test_block_tx_size_limit_exceeded_first_transaction() {
    let mut state = state_with_log_contract(100);
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_encode_size_limit(10)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let err = executor
        .execute_transaction(&user_tx(0, 1_000_000))
        .expect_err("the body does not fit in the block");
    assert!(format!("{err}").contains("Block transactions encode size limit exceeded"), "{err}");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert!(result.receipts().is_empty());
}

/// The same limit mid-block: the transaction that would cross it is refused before it runs, so
/// unlike a post-execution dimension the block never crosses this one.
#[test]
fn test_block_tx_size_limit_exceeded_mid_block() {
    let mut state = state_with_log_contract(100);
    let one = MegaTransactionExt::tx_size(&user_tx(0, 1_000_000));
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_encode_size_limit(one * 2 + 1)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    executor.execute_transaction(&user_tx(0, 1_000_000)).expect("the first body fits");
    executor.execute_transaction(&user_tx(1, 1_000_000)).expect("the second body fits");
    executor.execute_transaction(&user_tx(2, 1_000_000)).expect_err("the third body does not");

    assert!(executor.limiter().block_tx_size_used <= one * 2 + 1, "the limit was never crossed");
    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 2);
}

/// Transactions of different sizes accumulate by their own sizes, and the one that does not fit
/// is the one refused.
#[test]
fn test_block_tx_size_limit_with_varying_sizes() {
    let mut state = state_with_log_contract(100);
    let small = user_tx(0, log_call_gas(100));
    let large = common::user_tx_with_input(
        1,
        incompressible(4_096),
        3_000_000 + common::body_history(4_096) + log_call_gas(100),
    );
    let (small_size, large_size) =
        (MegaTransactionExt::tx_size(&small), MegaTransactionExt::tx_size(&large));
    assert!(large_size > small_size);

    let mut executor = executor(
        &mut state,
        common::block_ctx(
            BlockLimits::no_limits().with_block_txs_encode_size_limit(small_size + large_size),
        ),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    executor.execute_transaction(&small).expect("the small body fits");
    assert_eq!(executor.limiter().block_tx_size_used, small_size);
    executor.execute_transaction(&large).expect("the large body fits exactly");
    assert_eq!(executor.limiter().block_tx_size_used, small_size + large_size);

    executor
        .execute_transaction(&user_tx(2, log_call_gas(100)))
        .expect_err("nothing fits any more");
}

/// alloy-evm's commit cannot refuse, and its contract is that an outcome commits before the next
/// transaction executes. A builder that breaks it — two candidates executed against the same
/// pre-state, then both committed through the trait — would pack what the checked commit refuses.
/// The trait's commit makes the same check in a debug build and trips on it: here the block's
/// data-size budget has room for one of the two.
#[cfg(debug_assertions)]
#[test]
fn test_the_trait_commit_trips_on_an_outcome_the_block_has_no_room_for() {
    // The body and a log of 100 bytes: what each candidate keeps.
    let kept = mega_evm::TX_BODY_SIZE + mega_evm::LOG_BASE_SIZE + 100;
    let tx_gas_limit = log_call_gas(100);
    let mut db = common::database();
    db.set_account_code(CONTRACT, log_generating_contract(100));
    db.set_account_balance(CALLER2, U256::from(1_000_000_000_000_000_u64));
    let mut state = revm::database::State::builder().with_database(db).build();
    let limits = BlockLimits::no_limits().with_block_txs_data_limit(kept);
    let mut executor = executor(&mut state, common::block_ctx(limits));
    executor.apply_pre_execution_changes().expect("the block starts");

    let first = executor
        .execute_transaction_without_commit(&tx_from(CALLER, 0, tx_gas_limit))
        .expect("the block has room");
    let second = executor
        .execute_transaction_without_commit(&tx_from(CALLER2, 0, tx_gas_limit))
        .expect("nothing has committed yet");
    assert_eq!(second.usage.data_size, kept);

    BlockExecutor::commit_transaction(&mut executor, first);
    assert_eq!(executor.limiter().usage.data_size, kept, "the block has kept exactly its limit");
    let tripped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BlockExecutor::commit_transaction(&mut executor, second);
    }))
    .expect_err("the commit trips");
    let message = tripped
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| tripped.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(message.contains("an outcome the block no longer has room for"), "{message}");
}

/// The block's counters may move between a transaction executing and its commit, so what it
/// adds is checked against the block once more before it is committed.
///
/// Three transactions are executed while none has committed, so each passes admission; two
/// commit and fill the block, and the third is refused at commit.
#[test]
fn test_commit_time_pre_execution_check_parallel_simulation() {
    // 100,000 of regular gas on top of the history the body and the log cost.
    let tx_gas_limit = 100_000 +
        common::body_history(0) +
        mega_evm::history_gas(mega_evm::log_history_bytes(0, 100)).expect("the log has a price");

    let db = || {
        let mut db = common::database();
        db.set_account_code(CONTRACT, log_generating_contract(100));
        db.set_account_balance(CALLER2, U256::from(1_000_000_000_000_000_u64));
        db.set_account_balance(CALLER3, U256::from(1_000_000_000_000_000_u64));
        revm::database::State::builder().with_database(db).build()
    };

    // What one of these transactions actually spends, so the block below has room for exactly
    // two of them and the third finds it full.
    let spent = {
        let mut state = db();
        let mut probe = executor(&mut state, common::unlimited_ctx());
        probe.apply_pre_execution_changes().expect("the block starts");
        probe
            .execute_transaction(&tx_from(CALLER, 0, tx_gas_limit))
            .expect("the probe transaction executes")
            .tx_gas_used()
    };
    let block_gas_limit = spent + tx_gas_limit;

    let mut state = db();
    let mut env = common::evm_env();
    env.block_env = BlockEnv { gas_limit: block_gas_limit, ..env.block_env };
    let mut executor = common::executor_with_env(&mut state, common::unlimited_ctx(), env);
    executor.apply_pre_execution_changes().expect("the block starts");

    let first = tx_from(CALLER, 0, tx_gas_limit);
    let second = tx_from(CALLER2, 0, tx_gas_limit);
    let third = tx_from(CALLER3, 0, tx_gas_limit);

    let first = executor.run_transaction(&first).expect("the block has room");
    let second = executor.run_transaction(&second).expect("nothing has committed yet");
    let third = executor.run_transaction(&third).expect("nothing has committed yet");

    executor.commit_transaction_outcome(second).expect("the block has room");
    executor.commit_transaction_outcome(third).expect("the block is full now");

    let err = executor
        .commit_transaction_outcome(first)
        .expect_err("the block filled up while this transaction waited");
    assert!(format!("{err}").contains("more than blocks available gas"), "{err}");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 2);
}

/// A transaction that reports its own sizes is executed on the figures it reports. A debug build
/// cross-checks them against a fresh recompute, so an understated one trips here instead of
/// passing a limit it should have been refused by.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "do not match a fresh recompute")]
fn test_run_transaction_rejects_understated_cached_da_size() {
    const TX_DA_SIZE_LIMIT: u64 = 80;
    const REPORTED_DA_SIZE: u64 = 50;

    let mut state = common::state();
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_tx_da_size_limit(TX_DA_SIZE_LIMIT)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let envelope = common::tx(0, CONTRACT, Bytes::new(), 100_000);
    let real_da_size = MegaTransactionExt::estimated_da_size(&envelope);
    assert!(
        real_da_size > TX_DA_SIZE_LIMIT,
        "the test needs a recomputed size ({real_da_size}) above the limit"
    );
    const { assert!(REPORTED_DA_SIZE < TX_DA_SIZE_LIMIT, "the reported size must pass the limit") };

    let recovered = Recovered::new_unchecked(&envelope, CALLER);
    executor
        .run_transaction(&recovered)
        .expect_err("a transaction that recomputes its sizes is refused by the limit");

    let enriched = EnrichedMegaTx::new(
        recovered,
        MegaTransactionExt::tx_hash(&envelope),
        REPORTED_DA_SIZE,
        MegaTransactionExt::tx_size(&envelope),
    );
    let _ = executor.run_transaction(&enriched);
}

/// The common case of the same path: figures that agree with the transaction are used as they
/// are, and the outcome carries them.
#[test]
fn test_run_transaction_succeeds_with_accurate_cache() {
    let mut state = common::state();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let envelope = common::tx(0, CONTRACT, Bytes::new(), 1_000_000);
    let expected_da_size = MegaTransactionExt::estimated_da_size(&envelope);
    let expected_tx_size = MegaTransactionExt::tx_size(&envelope);
    let enriched = EnrichedMegaTx::new_slow(Recovered::new_unchecked(&envelope, CALLER));

    let outcome = executor
        .run_transaction(&enriched)
        .expect("figures that agree with the transaction are used as they are");

    assert_eq!(outcome.da_size, expected_da_size);
    assert_eq!(outcome.tx_size, expected_tx_size);
}

/// A deposit is exempt from its own data-availability size limit: the chain cannot censor it.
#[test]
fn test_deposit_transaction_exempt_from_single_tx_da_limit() {
    const DA_SIZE_LIMIT: u64 = 100;
    let mut state = common::state();
    let mut executor = executor(
        &mut state,
        common::block_ctx(
            BlockLimits::no_limits()
                .with_tx_da_size_limit(DA_SIZE_LIMIT)
                .with_block_da_size_limit(DA_SIZE_LIMIT * 10),
        ),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let deposit = common::deposit_tx(incompressible(100_000), 3_000_000);
    assert!(MegaTransactionExt::estimated_da_size(&deposit) > DA_SIZE_LIMIT * 10);

    executor.execute_transaction(&deposit).expect("a deposit is exempt");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
}

/// A user transaction is not exempt from the same limit.
#[test]
fn test_regular_transaction_rejected_by_single_tx_da_limit() {
    const DA_SIZE_LIMIT: u64 = 100;
    let mut state = common::state();
    let mut executor = executor(
        &mut state,
        common::block_ctx(
            BlockLimits::no_limits()
                .with_tx_da_size_limit(DA_SIZE_LIMIT)
                .with_block_da_size_limit(DA_SIZE_LIMIT * 10),
        ),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let tx = common::user_tx_with_input(0, incompressible(100_000), 3_000_000);
    assert!(MegaTransactionExt::estimated_da_size(&tx) > DA_SIZE_LIMIT);

    let err = executor.execute_transaction(&tx).expect_err("the limit binds a user transaction");
    assert!(
        format!("{err}").contains("Transaction data availability size limit exceeded"),
        "{err}"
    );
}

/// No building policy refuses a deposit, which the block derived from L1 must include: a builder
/// holding transactions to a declared gas, an encoded size and a block encoded size that one
/// deposit is over on each packs it, and counts its encoding towards the block's. A user
/// transaction over the same policy is refused.
#[test]
fn test_no_building_policy_refuses_a_deposit() {
    const GAS_LIMIT: u64 = 3_000_000;
    const CALLDATA: usize = 1_000;
    let policy = BlockLimits::no_limits()
        .with_tx_gas_limit(GAS_LIMIT - 1)
        .with_tx_encode_size_limit(CALLDATA as u64)
        .with_block_txs_encode_size_limit(CALLDATA as u64);
    let mut state = common::state();
    let mut executor = executor(&mut state, common::block_ctx(policy));
    executor.apply_pre_execution_changes().expect("the block starts");

    let deposit = common::deposit_tx(Bytes::from(vec![0xab; CALLDATA]), GAS_LIMIT);
    let tx_size = MegaTransactionExt::tx_size(&deposit);
    assert!(tx_size > CALLDATA as u64, "the deposit's encoding is over both size limits");
    executor.execute_transaction(&deposit).expect("no building policy refuses a deposit");
    assert_eq!(executor.limiter().block_tx_size_used, tx_size, "and it counts its encoding");

    for (tx, refusal) in [
        (common::user_tx_with_input(0, Bytes::new(), GAS_LIMIT), "Transaction gas limit exceeded"),
        (
            common::user_tx_with_input(0, Bytes::from(vec![0xab; CALLDATA]), GAS_LIMIT - 1),
            "Transaction encode size limit exceeded",
        ),
        (common::user_tx_with_input(0, Bytes::new(), GAS_LIMIT - 1), "block_used="),
    ] {
        let err = executor.execute_transaction(&tx).expect_err("the policy binds a transaction");
        assert!(format!("{err}").contains(refusal), "{refusal}: {err}");
    }

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1, "the deposit alone is packed");
}

/// The data-availability footprint is re-checked at commit too: two candidates that each fit the
/// block's footprint budget on their own do not both commit.
///
/// A builder simulates candidates against the same pre-state and then picks among them, so both
/// are executed before either commits. The first fills the budget; the second is refused before
/// the block's counters, receipts or state move.
#[test]
fn test_commit_time_da_footprint_check_parallel_simulation() {
    const SCALAR: u16 = u16::MAX;
    // Above what two thousand calldata bytes cost: the body's history at the byte prices in
    // effect, and 64 gas a calldata byte, which covers both its intrinsic charge and its floor.
    let tx_gas_limit = common::body_history(2_000) + 15_000 + 64 * 2_000;

    let first = tx_with_input(CALLER, incompressible(2_000), tx_gas_limit);
    let second = tx_with_input(CALLER2, incompressible(1_999), tx_gas_limit);
    let footprint = |tx: &Recovered<MegaTxEnvelope>| {
        MegaTransactionExt::estimated_da_size(tx) * u64::from(SCALAR)
    };
    // The block holds exactly the larger of the two, so each fits alone and the two do not.
    let budget = footprint(&first).max(footprint(&second));
    assert!(budget >= tx_gas_limit, "the budget is the block's gas limit, which must fit a tx");

    let mut db = common::database();
    db.set_account_balance(CALLER2, U256::from(1_000_000_000_000_000_u64));
    db.set_account_storage(
        L1_BLOCK_CONTRACT,
        DA_FOOTPRINT_GAS_SCALAR_SLOT,
        U256::from(SCALAR) << (8 * (32 - DA_FOOTPRINT_GAS_SCALAR_OFFSET - 2)),
    );
    let mut state = revm::database::State::builder().with_database(db).build();
    let mut env = common::evm_env();
    env.block_env = BlockEnv { gas_limit: budget, ..env.block_env };
    let mut executor = common::executor_with_env(&mut state, common::unlimited_ctx(), env);
    executor.apply_pre_execution_changes().expect("the block starts");

    let first = executor.run_transaction(&first).expect("the block's footprint budget is free");
    let second = executor.run_transaction(&second).expect("nothing has committed yet");

    executor.commit_transaction_outcome(first).expect("the block has the footprint for it");
    let err = executor
        .commit_transaction_outcome(second)
        .expect_err("the block's footprint was spent while this transaction waited");

    assert!(
        format!("{err}").contains("DA footprint exceeds available block DA footprint"),
        "{err}"
    );
    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1, "only the transaction that fit was packed");
    assert!(result.blob_gas_used <= budget, "the block stays within its footprint budget");
}

/// A database error, fatal as a storage failure or not as a read an EIP-7928 block access list
/// does not cover.
#[derive(Debug)]
struct AuthorityReadError {
    fatal: bool,
}

impl core::fmt::Display for AuthorityReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the authority's account cannot be read (fatal: {})", self.fatal)
    }
}

impl core::error::Error for AuthorityReadError {}

impl revm::context::DBErrorMarker for AuthorityReadError {
    fn is_fatal(&self) -> bool {
        self.fatal
    }
}

/// A database that refuses to read one account, if it names one, with an error that is fatal or
/// not.
#[derive(Debug)]
struct RefusesAnAccount {
    db: mega_evm::test_utils::MemoryDatabase,
    refused: Option<Address>,
    fatal: bool,
}

impl revm::Database for RefusesAnAccount {
    type Error = AuthorityReadError;

    fn basic(&mut self, address: Address) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        if self.refused == Some(address) {
            return Err(AuthorityReadError { fatal: self.fatal });
        }
        Ok(self.db.basic(address).expect("the memory database serves"))
    }

    fn code_by_hash(
        &mut self,
        code_hash: alloy_primitives::B256,
    ) -> Result<revm::state::Bytecode, Self::Error> {
        Ok(self.db.code_by_hash(code_hash).expect("the memory database serves"))
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.db.storage(address, index).expect("the memory database serves"))
    }

    fn block_hash(&mut self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
        Ok(self.db.block_hash(number).expect("the memory database serves"))
    }
}

/// An authorization list is simulated before the first frame when it may cross the transaction's
/// KV or state-growth limit, and a database error while it reads an authority gives the
/// transaction no outcome: the block executor reports it as an internal error when the database
/// declares it fatal, and refuses the block for any other, including when an earlier prefix
/// would already have crossed. Three new authorities under a state-growth limit of one: the first
/// two cross it, and the third cannot be read; served, the list is skipped and the transaction
/// included with the stop. A skipped list is not applied, so the simulation is the only read of
/// the third.
///
/// revm's `State`, which a block runs on, declares every error of the database below it fatal and
/// only a read its EIP-7928 block access list does not cover not fatal, so a block shows the fatal
/// half. The other half is the engine's part of the same rule: it hands back the database's own
/// error, which the executor maps by its fatality (`BlockExecutionError::evm`).
#[test]
fn test_a_database_error_in_the_authorization_list_simulation_is_internal_only_when_fatal() {
    use alloy_consensus::{SignableTransaction, TxEip7702};
    use alloy_eips::eip7702::{Authorization, SignedAuthorization};
    use alloy_evm::{
        block::{BlockExecutionError, BlockValidationError},
        Evm, EvmFactory,
    };
    use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
    use alloy_primitives::{Signature, B256};
    use mega_evm::{LimitCheck, LimitKind, MegaBlockExecutor, MegaEvmFactory};

    let authorization = |delegate: Address| {
        let word = U256::from_be_bytes([0x22; 32]);
        let auth = Authorization { chain_id: U256::ZERO, address: delegate, nonce: 0 };
        let signed = SignedAuthorization::new_unchecked(auth, 0, word, word);
        let authority = signed.recover_authority().expect("a Nick's-Method signature recovers");
        (signed, authority)
    };
    let (first, _) = authorization(CONTRACT);
    let (second, _) = authorization(CALLER3);
    let (third, refused) = authorization(CALLER2);
    let tx = TxEip7702 {
        chain_id: common::CHAIN_ID,
        nonce: 0,
        gas_limit: 1_000_000 + common::body_history(0) + 4 * common::new_account_state_gas(),
        max_fee_per_gas: 0,
        max_priority_fee_per_gas: 0,
        to: CONTRACT,
        value: U256::ZERO,
        access_list: Default::default(),
        authorization_list: vec![first, second, third],
        input: Bytes::new(),
    };
    let tx = Recovered::new_unchecked(
        MegaTxEnvelope::Eip7702(tx.into_signed(Signature::test_signature())),
        CALLER,
    );
    let limits = common::loosest()
        .with_tx_runtime_limits(common::loosest_tx().with_tx_state_growth_limit(1));
    let database = |refused: Option<Address>, fatal: bool| RefusesAnAccount {
        db: common::database(),
        refused,
        fatal,
    };
    let in_a_block = |db: RefusesAnAccount| {
        let mut state = revm::database::State::builder().with_database(db).build();
        let evm = MegaEvmFactory::new().create_evm(&mut state, common::evm_env());
        let mut executor = MegaBlockExecutor::new(
            evm,
            common::unlimited_ctx(),
            common::chain_spec(),
            OpAlloyReceiptBuilder::default(),
        )
        .with_protocol_limits(limits);
        executor.apply_pre_execution_changes().expect("the block starts");
        executor.execute_transaction_without_commit(&tx).map(|result| result.inner.limit_exceeded)
    };

    let stop = in_a_block(database(None, false)).expect("served, the transaction is included");
    assert!(
        matches!(stop, Some(LimitCheck::ExceedsLimit { kind: LimitKind::StateGrowth, .. })),
        "the list is skipped and the first frame stopped: {stop:?}"
    );
    let err = in_a_block(database(Some(refused), true)).expect_err("the read fails");
    assert!(matches!(err, BlockExecutionError::Internal(_)), "{err:?}");
    assert!(err.to_string().contains("cannot be read"), "{err}");

    for fatal in [true, false] {
        let mut evm =
            MegaEvmFactory::new().create_evm(database(Some(refused), fatal), common::evm_env());
        evm.set_tx_runtime_limits(limits.tx_runtime_limits);
        let err = evm.transact(&tx).expect_err("the read fails");
        let err = BlockExecutionError::evm(err, B256::ZERO);
        if fatal {
            assert!(matches!(err, BlockExecutionError::Internal(_)), "{err:?}");
        } else {
            assert!(
                matches!(err, BlockExecutionError::Validation(BlockValidationError::EVM { .. })),
                "{err:?}"
            );
        }
        assert!(err.to_string().contains("cannot be read"), "{err}");
    }
}
