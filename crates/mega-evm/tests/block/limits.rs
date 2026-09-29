//! What a block admits, and what it does with a transaction it does not.

use alloy_consensus::{transaction::Recovered, Transaction};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::BytecodeBuilder, BlockLimits, EnrichedMegaTx, EvmTxRuntimeLimits, LimitCheck,
    LimitKind, MegaTransactionExt, MegaTxEnvelope, ProtocolLimits,
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

    let first = user_tx(0, 1_000_000);
    let gas = executor.execute_transaction(&first).expect("the first transaction is packed");
    assert!(gas.tx_gas_used() < first.gas_limit());

    let second = user_tx(1, 1_000_000);
    let gas = executor
        .execute_transaction(&second)
        .expect("the transaction that crosses the limit is packed too");
    assert!(gas.tx_gas_used() < second.gas_limit());
    assert!(executor.limiter().usage.data_size >= 2_500, "the block has crossed its limit");

    let err = executor
        .execute_transaction(&user_tx(2, 1_000_000))
        .expect_err("the block has no data-size left");
    assert!(format!("{err}").contains("Block transactions data limit reached"), "{err}");
}

/// Transactions that stay inside the block's limits are all packed.
#[test]
fn test_block_multiple_transactions_within_limits() {
    let mut state = state_with_log_contract(100);
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_data_limit(10_000)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    for nonce in 0..5 {
        let tx = user_tx(nonce, 1_000_000);
        let gas = executor.execute_transaction(&tx).expect("the transaction is packed");
        assert!(gas.tx_gas_used() < tx.gas_limit(), "transaction {nonce}");
    }

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 5);
}

/// The same limit mid-block: the transactions before the crossing are packed, the crossing one
/// is packed, and the block ends there.
#[test]
fn test_block_data_limit_exceeded_mid_block() {
    let mut state = state_with_log_contract(2_000);
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_data_limit(6_000)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    for nonce in 0..3 {
        executor.execute_transaction(&user_tx(nonce, 1_000_000)).expect("packed");
    }
    assert!(
        executor.execute_transaction(&user_tx(3, 1_000_000)).is_err(),
        "the block has no data-size left"
    );

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 3, "the fourth transaction never ran");
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

/// A state whose callee keeps `writes` write records on every call.
fn state_with_incrementing_contract(
    writes: u64,
) -> revm::database::State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(CONTRACT, incrementing_contract(writes));
    revm::database::State::builder().with_database(db).build()
}

/// The block's KV limit: the transaction that crosses it is packed, however far past the limit
/// it takes the block, and the next one is refused before it runs.
#[test]
fn test_block_custom_kv_update_limit() {
    let mut state = state_with_incrementing_contract(50);
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_kv_update_limit(1)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    executor.execute_transaction(&user_tx(0, 10_000_000)).expect("the crossing transaction");
    assert_eq!(executor.limiter().usage.write_records, 50, "the block has crossed its limit");

    let err = executor
        .execute_transaction(&user_tx(1, 10_000_000))
        .expect_err("the block has no write records left");
    assert!(format!("{err}").contains("Block KV update limit reached"), "{err}");
    assert!(format!("{err}").contains("block_used=50"), "{err}");
}

/// The same limit mid-block, one record a transaction: the transactions up to the one that
/// reaches the limit are packed, and the block ends there.
#[test]
fn test_block_kv_limit_exceeded_mid_block() {
    let mut state = state_with_incrementing_contract(1);
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_kv_update_limit(3)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    for nonce in 0..3 {
        let tx = user_tx(nonce, 10_000_000);
        let gas = executor.execute_transaction(&tx).expect("packed");
        assert!(gas.tx_gas_used() < tx.gas_limit(), "transaction {nonce}");
    }
    assert!(
        executor.execute_transaction(&user_tx(3, 10_000_000)).is_err(),
        "the block has no write records left"
    );

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 3, "the fourth transaction never ran");
    assert_eq!(result.usage.write_records, 3, "the block reports its KV count");
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

/// The block's data-size limit never refuses a deposit, and a deposit still counts towards it.
///
/// Two data-heavy deposits cross the limit between them, a third finds the block past it and is
/// still included, and the ordinary transaction after them is refused on the room they used. The
/// deposits add nothing to the data-availability size.
#[test]
fn test_the_block_data_size_limit_never_refuses_a_deposit() {
    const CALLDATA: usize = 1_000;
    let per_deposit = mega_evm::TX_BODY_SIZE + CALLDATA as u64;
    let limit = per_deposit + per_deposit / 2;
    let mut state = common::state();
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_data_limit(limit)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    for (n, what) in ["the first deposit", "the deposit that crosses", "the deposit past the limit"]
        .into_iter()
        .enumerate()
    {
        let deposit = common::deposit_tx(incompressible(CALLDATA), 1_000_000);
        let outcome =
            executor.run_transaction(&deposit).unwrap_or_else(|err| panic!("{what}: {err}"));
        assert!(outcome.inner.result.is_success(), "{what}: {:?}", outcome.inner.result);
        assert_eq!(outcome.inner.usage.data_size, per_deposit, "{what}");
        executor.commit_transaction_outcome(outcome).unwrap_or_else(|err| panic!("{what}: {err}"));
        assert_eq!(executor.limiter().usage.data_size, per_deposit * (n as u64 + 1), "{what}");
    }
    assert!(executor.limiter().usage.data_size > limit, "the deposits crossed the limit");
    assert_eq!(
        executor.limiter().block_da_size_used,
        0,
        "a deposit adds no data-availability size"
    );

    let err = executor
        .execute_transaction(&user_tx(3, 100_000))
        .expect_err("the deposits used the room an ordinary transaction would need");
    assert!(format!("{err}").contains("Block transactions data limit reached"), "{err}");
    assert!(format!("{err}").contains(&format!("block_used={}", per_deposit * 3)), "{err}");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 3, "every deposit is packed, and nothing after them");
}

/// The per-transaction data-size limit is not a block limit: it stops a deposit's own execution
/// the way it stops any transaction's, and the deposit is included with the stop as its result.
#[test]
fn test_the_transaction_data_size_limit_stops_a_deposit_that_is_still_included() {
    let limit = mega_evm::TX_BODY_SIZE + 10;
    let mut state = common::state();
    let mut executor = common::executor_with_limits(
        &mut state,
        ProtocolLimits::no_limits()
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let deposit = common::deposit_tx(Bytes::from(vec![0xab; 11]), 1_000_000);
    let outcome = executor.run_transaction(&deposit).expect("a deposit is not refused");
    assert!(
        matches!(outcome.inner.result, ExecutionResult::Revert { .. }),
        "{:?}",
        outcome.inner.result
    );
    let stop = outcome.inner.limit_exceeded.expect("the data-size limit stopped the deposit");
    assert!(
        matches!(
            stop,
            LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, limit: l, frame_local: false, .. }
                if l == limit
        ),
        "{stop:?}"
    );
    executor.commit_transaction_outcome(outcome).expect("the deposit is included");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
    assert!(!result.receipts()[0].status(), "the stopped deposit's receipt reports a failure");
}

/// With no size limit configured, a transaction of any size is admitted.
#[test]
fn test_block_tx_size_limit_default_unlimited() {
    let mut state = state_with_log_contract(100);
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let big = common::user_tx_with_input(0, incompressible(20_000), 3_000_000);
    assert!(MegaTransactionExt::tx_size(&big) > 20_000);

    executor.execute_transaction(&big).expect("no limit, no refusal");
}

/// Several transactions fit while their bodies together stay under the block's limit.
#[test]
fn test_block_tx_size_limit_allows_multiple_transactions() {
    let mut state = state_with_log_contract(100);
    let one = MegaTransactionExt::tx_size(&user_tx(0, 1_000_000));
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_encode_size_limit(one * 3)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    for nonce in 0..3 {
        executor.execute_transaction(&user_tx(nonce, 1_000_000)).expect("the body fits");
    }
    assert_eq!(executor.limiter().block_tx_size_used, one * 3);
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
    let small = user_tx(0, 1_000_000);
    let large = common::user_tx_with_input(1, incompressible(4_096), 3_000_000);
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

    executor.execute_transaction(&user_tx(2, 1_000_000)).expect_err("nothing fits any more");
}

/// The block's counters may move between a transaction executing and its commit, so what it
/// adds is checked against the block once more before it is committed.
///
/// Three transactions are executed while none has committed, so each passes admission; two
/// commit and fill the block, and the third is refused at commit.
#[test]
fn test_commit_time_pre_execution_check_parallel_simulation() {
    const TX_GAS_LIMIT: u64 = 100_000;

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
            .execute_transaction(&tx_from(CALLER, 0, TX_GAS_LIMIT))
            .expect("the probe transaction executes")
            .tx_gas_used()
    };
    let block_gas_limit = spent + TX_GAS_LIMIT;

    let mut state = db();
    let mut env = common::evm_env();
    env.block_env = BlockEnv { gas_limit: block_gas_limit, ..env.block_env };
    let mut executor = common::executor_with_env(&mut state, common::unlimited_ctx(), env);
    executor.apply_pre_execution_changes().expect("the block starts");

    let first = tx_from(CALLER, 0, TX_GAS_LIMIT);
    let second = tx_from(CALLER2, 0, TX_GAS_LIMIT);
    let third = tx_from(CALLER3, 0, TX_GAS_LIMIT);

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
fn test_execute_mega_transaction_succeeds_with_accurate_cache() {
    let mut state = common::state();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let envelope = common::tx(0, CONTRACT, Bytes::new(), 1_000_000);
    let expected_da_size = MegaTransactionExt::estimated_da_size(&envelope);
    let expected_tx_size = MegaTransactionExt::tx_size(&envelope);
    let enriched = EnrichedMegaTx::new_slow(Recovered::new_unchecked(&envelope, CALLER));

    let outcome = executor
        .execute_mega_transaction(&enriched)
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

/// A deposit is exempt from the block's data-availability size too, and adds nothing to it.
#[test]
fn test_deposit_exempt_from_block_da_limit() {
    const DA_SIZE_LIMIT: u64 = 100;
    let mut state = common::state();
    let mut executor = executor(
        &mut state,
        common::block_ctx(
            BlockLimits::no_limits()
                .with_tx_da_size_limit(DA_SIZE_LIMIT * 10_000)
                .with_block_da_size_limit(DA_SIZE_LIMIT),
        ),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    executor
        .execute_transaction(&common::deposit_tx(incompressible(100_000), 3_000_000))
        .expect("a deposit is exempt");

    assert_eq!(executor.limiter().block_da_size_used, 0, "and adds nothing to the block's");
    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
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

/// The three cases in one block: a small user transaction, a large deposit, a large user
/// transaction.
#[test]
fn test_mixed_deposit_and_regular_transactions() {
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

    executor.execute_transaction(&user_tx(0, 100_000)).expect("a small user transaction fits");
    executor
        .execute_transaction(&common::deposit_tx(incompressible(100_000), 3_000_000))
        .expect("a deposit is exempt");
    executor
        .execute_transaction(&common::user_tx_with_input(1, incompressible(100_000), 3_000_000))
        .expect_err("a large user transaction is not");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 2);
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
    // Above what two thousand calldata bytes cost: the intrinsic charge and the body's history.
    const TX_GAS_LIMIT: u64 = 300_000;

    let first = tx_with_input(CALLER, incompressible(2_000), TX_GAS_LIMIT);
    let second = tx_with_input(CALLER2, incompressible(1_999), TX_GAS_LIMIT);
    let footprint = |tx: &Recovered<MegaTxEnvelope>| {
        MegaTransactionExt::estimated_da_size(tx) * u64::from(SCALAR)
    };
    // The block holds exactly the larger of the two, so each fits alone and the two do not.
    let budget = footprint(&first).max(footprint(&second));
    assert!(budget >= TX_GAS_LIMIT, "the budget is the block's gas limit, which must fit a tx");

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
