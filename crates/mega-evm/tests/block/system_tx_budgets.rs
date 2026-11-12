//! Mega System Transactions obey packing budgets and building policy at admission and commit.
//! Deposits remain exempt; both classes count towards the budgets they consume.

use alloy_consensus::{transaction::Recovered, Transaction};
use alloy_eips::Encodable2718;
use alloy_evm::block::{BlockExecutionError, BlockExecutor, BlockValidationError};
use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    BlockLimits, MegaSpecId, MegaTransactionExt, MegaTxEnvelope, ProtocolLimits, TX_BODY_SIZE,
    WRITE_RECORD_SIZE,
};
use op_revm::constants::{
    DA_FOOTPRINT_GAS_SCALAR_OFFSET, DA_FOOTPRINT_GAS_SCALAR_SLOT, L1_BLOCK_CONTRACT,
};
use revm::database::State;

use crate::common::{
    self, empty_call_gas, executor, system_tx, user_tx, BLOCK_GAS_LIMIT, CONTRACT,
};

/// A second funded sender, so two ordinary transactions can execute against the same state.
const CALLER2: Address = address!("0x2000000000000000000000000000000000000003");

/// A legacy call from the live system address to the Oracle that declares `gas_limit`, with
/// `input` as calldata.
fn system_call(input: Bytes, gas_limit: u64) -> Recovered<MegaTxEnvelope> {
    Recovered::new_unchecked(
        common::tx(0, ORACLE_CONTRACT_ADDRESS, input, gas_limit),
        MEGA_SYSTEM_ADDRESS,
    )
}

/// A full packing budget refuses a system transaction before execution and still admits a
/// deposit. A refusal changes neither counters nor receipts.
fn assert_full_budget_refuses_system(policy: BlockLimits, refusal: &str) {
    let mut state = common::state();
    let mut builder = executor(&mut state, common::block_ctx(policy));
    builder.apply_pre_execution_changes().expect("the block starts");
    let before = builder.limiter().usage;
    let error = builder.run_transaction(&system_tx()).expect_err("the budget is full");
    assert!(matches!(error, BlockExecutionError::Validation(_)), "{error}");
    assert!(error.to_string().contains(refusal), "{error}");
    assert_eq!(builder.limiter().usage, before);
    assert!(builder.receipts().is_empty());
    builder
        .execute_transaction(&common::deposit_tx(Bytes::new(), empty_call_gas()))
        .expect("the same budget cannot refuse a deposit");
    assert_eq!(builder.receipts().len(), 1);

    // Apply the same budget as a protocol limit, with no building policy. Whole-block
    // validation must reject inclusion, rather than silently omit the transaction.
    let limits = ProtocolLimits::for_spec(MegaSpecId::SATIN)
        .with_block_txs_data_limit(policy.block_txs_data_limit)
        .with_block_kv_update_limit(policy.block_kv_update_limit)
        .with_block_state_growth_limit(policy.block_state_growth_limit);
    let mut state = common::state();
    let validator = common::executor_with_limits(&mut state, limits);
    let error = validator.execute_block([&system_tx()]).expect_err("the block is invalid");
    assert!(matches!(error, BlockExecutionError::Validation(_)), "{error}");
    assert!(error.to_string().contains(refusal), "{error}");

    let mut state = common::state();
    let validator = common::executor_with_limits(&mut state, limits);
    let block = validator
        .execute_block([&common::deposit_tx(Bytes::new(), empty_call_gas())])
        .expect("a block holding a deposit is valid under the same budget");
    assert_eq!(block.receipts.len(), 1);
}

#[test]
fn test_a_mega_system_transaction_is_refused_at_a_full_data_budget() {
    assert_full_budget_refuses_system(
        BlockLimits::no_limits().with_block_txs_data_limit(0),
        "Block transactions data limit reached",
    );
}

#[test]
fn test_a_mega_system_transaction_is_refused_at_a_full_kv_budget() {
    assert_full_budget_refuses_system(
        BlockLimits::no_limits().with_block_kv_update_limit(0),
        "Block KV update limit reached",
    );
}

#[test]
fn test_a_mega_system_transaction_is_refused_at_a_full_state_growth_budget() {
    assert_full_budget_refuses_system(
        BlockLimits::no_limits().with_block_state_growth_limit(0),
        "Block state growth limit reached",
    );
}

/// Each policy rejects the system transaction one unit below its size or gas and admits it at
/// equality. Deposits ignore it, and validation without that policy includes the system call.
fn assert_system_obeys_policy(
    set_limit: fn(BlockLimits, u64) -> BlockLimits,
    measured: u64,
    refusal: &str,
) {
    let system = system_tx();
    let mut state = common::state();
    let mut builder =
        executor(&mut state, common::block_ctx(set_limit(BlockLimits::default(), measured - 1)));
    builder.apply_pre_execution_changes().expect("the block starts");
    let error = builder.run_transaction(&system).expect_err("the policy refuses the system call");
    assert!(error.to_string().contains(refusal), "{error}");
    assert!(builder.receipts().is_empty());
    assert_eq!(builder.limiter().usage, mega_evm::LimitUsage::ZERO);
    builder
        .execute_transaction(&common::deposit_tx(system.input().clone(), system.gas_limit()))
        .expect("a deposit in the same position is exempt");
    assert_eq!(builder.receipts().len(), 1);

    let mut state = common::state();
    let mut builder =
        executor(&mut state, common::block_ctx(set_limit(BlockLimits::default(), measured)));
    builder.apply_pre_execution_changes().expect("the block starts");
    let outcome = builder.run_transaction(&system).expect("equality fits");
    assert!(outcome.is_system_transaction && !outcome.is_deposit);
    assert!(outcome.inner.result.is_success());
    builder.commit_transaction_outcome(outcome).expect("the exact fit commits");
    assert_eq!(builder.limiter().block_tx_size_used, system.inner().encode_2718_len() as u64);
    assert_eq!(
        builder.limiter().block_da_size_used,
        MegaTransactionExt::estimated_da_size(&system)
    );
    let (_, built) = builder.finish_with_counters().expect("the block finishes");

    let mut state = common::state();
    let validator = executor(&mut state, common::unlimited_ctx());
    let validated = validator.execute_block([&system]).expect("policy does not invalidate a block");
    assert_eq!(validated.receipts, built.inner.receipts);
    assert_eq!(validated.gas_used, built.inner.gas_used);
}

#[test]
fn test_a_mega_system_transaction_obeys_the_transaction_gas_policy() {
    assert_system_obeys_policy(
        BlockLimits::with_tx_gas_limit,
        system_tx().gas_limit(),
        "Transaction gas limit exceeded",
    );
}

#[test]
fn test_a_mega_system_transaction_obeys_the_transaction_encoded_size_policy() {
    assert_system_obeys_policy(
        BlockLimits::with_tx_encode_size_limit,
        MegaTransactionExt::tx_size(&system_tx()),
        "Transaction encode size limit exceeded",
    );
}

#[test]
fn test_a_mega_system_transaction_obeys_the_block_encoded_size_policy() {
    assert_system_obeys_policy(
        BlockLimits::with_block_txs_encode_size_limit,
        MegaTransactionExt::tx_size(&system_tx()),
        "Block transactions encode size limit exceeded",
    );
}

#[test]
fn test_a_mega_system_transaction_obeys_the_transaction_da_size_policy() {
    assert_system_obeys_policy(
        BlockLimits::with_tx_da_size_limit,
        MegaTransactionExt::estimated_da_size(&system_tx()),
        "Transaction data availability size limit exceeded",
    );
}

#[test]
fn test_a_mega_system_transaction_obeys_the_block_da_size_policy() {
    assert_system_obeys_policy(
        BlockLimits::with_block_da_size_limit,
        MegaTransactionExt::estimated_da_size(&system_tx()),
        "Block data availability size limit exceeded",
    );
}

/// A delayed system outcome and a delayed user outcome both lose their room to another
/// commit. A deposit executed at the same time still commits after the budget is reached.
#[test]
fn test_a_delayed_commit_of_a_mega_system_transaction_is_refused_at_a_spent_budget() {
    for (name, policy, refusal) in [
        (
            "data",
            BlockLimits::default().with_block_txs_data_limit(1),
            "Block transactions data limit reached",
        ),
        (
            "KV",
            BlockLimits::default().with_block_kv_update_limit(1),
            "Block KV update limit reached",
        ),
        (
            "growth",
            BlockLimits::default().with_block_state_growth_limit(1),
            "Block state growth limit reached",
        ),
    ] {
        let mut db = common::database();
        db.set_account_code(CONTRACT, common::writer());
        db.set_account_balance(CALLER2, U256::from(1_000_000_000_000_000_u64));
        let mut state = State::builder().with_database(db).build();
        let mut executor = executor(&mut state, common::block_ctx(policy));
        executor.apply_pre_execution_changes().expect("the block starts");
        let gas = empty_call_gas() +
            common::slot_state_gas() +
            mega_evm::write_record_history_gas(1).expect("the record has a price");
        let first = executor
            .run_transaction(&Recovered::new_unchecked(
                common::tx(0, CONTRACT, Bytes::new(), gas),
                CALLER2,
            ))
            .expect("room");
        assert!(first.inner.result.is_success());
        let second = executor.run_transaction(&user_tx(0, gas)).expect("room");
        let system = executor.run_transaction(&system_tx()).expect("room");
        let deposit =
            executor.run_transaction(&common::deposit_tx(Bytes::new(), gas)).expect("room");
        executor.commit_transaction_outcome(first).expect("the first fills the budget");
        let before = executor.limiter().usage;
        let error = executor.commit_transaction_outcome(system).expect_err("the budget is spent");
        assert!(error.to_string().contains(refusal), "{name}: {error}");
        assert_eq!(executor.limiter().usage, before, "{name}");
        assert_eq!(executor.receipts().len(), 1, "{name}");
        executor
            .commit_transaction_outcome(second)
            .expect_err("an ordinary transaction also needs room");
        executor.commit_transaction_outcome(deposit).expect("a delayed deposit is still exempt");
        assert_eq!(executor.receipts().len(), 2, "{name}");
    }
}

/// A system write fills or overshoots a budget and is kept; subsequent user and system
/// transactions see the room it used, and are refused without undoing that write.
fn assert_system_fills_budget(policy: BlockLimits, refusal: &str) {
    let mut state = common::state();
    let mut executor = executor(&mut state, common::block_ctx(policy));
    executor.apply_pre_execution_changes().expect("the block starts");
    let input =
        IOracle::setSlotCall { slot: U256::from(7), value: B256::repeat_byte(1) }.abi_encode();
    let gas = 1_000_000 + common::new_account_state_gas() + common::slot_state_gas();
    let system = system_call(input.into(), gas);
    let outcome =
        executor.run_transaction(&system).expect("a counter below its budget admits the call");
    assert!(outcome.inner.result.is_success());
    let usage = outcome.inner.usage;
    assert_eq!(usage.data_size, TX_BODY_SIZE + 68 + WRITE_RECORD_SIZE);
    assert_eq!(usage.write_records, 1);
    assert_eq!(usage.state_growth, 2, "the system sender and the new Oracle slot");
    executor.commit_transaction_outcome(outcome).expect("the call reaching the budget is included");
    assert_eq!(executor.limiter().usage, usage);
    for candidate in [
        user_tx(0, empty_call_gas()),
        common::legacy_from(
            MEGA_SYSTEM_ADDRESS,
            1,
            alloy_primitives::TxKind::Call(ORACLE_CONTRACT_ADDRESS),
            U256::ZERO,
            system.input().clone(),
            gas,
            0,
        ),
    ] {
        let error =
            executor.run_transaction(&candidate).expect_err("the committed write used the room");
        assert!(error.to_string().contains(refusal), "{error}");
        assert_eq!(executor.limiter().usage, usage);
        assert_eq!(executor.receipts().len(), 1);
    }
    drop(executor);
    assert_eq!(
        revm::Database::storage(&mut state, ORACLE_CONTRACT_ADDRESS, U256::from(7)).unwrap(),
        U256::from_be_bytes([1; 32])
    );
}

#[test]
fn test_a_mega_system_transaction_fills_or_overshoots_the_data_budget() {
    for limit in [1, TX_BODY_SIZE + 68 + WRITE_RECORD_SIZE] {
        assert_system_fills_budget(
            BlockLimits::default().with_block_txs_data_limit(limit),
            "Block transactions data limit reached",
        );
    }
}

#[test]
fn test_a_mega_system_transaction_fills_the_kv_budget() {
    assert_system_fills_budget(
        BlockLimits::default().with_block_kv_update_limit(1),
        "Block KV update limit reached",
    );
}

#[test]
fn test_a_mega_system_transaction_fills_or_overshoots_the_state_growth_budget() {
    for limit in [1, 2] {
        assert_system_fills_budget(
            BlockLimits::default().with_block_state_growth_limit(limit),
            "Block state growth limit reached",
        );
    }
}

/// A candidate that fits an empty block can lose encoded-size or DA room while waiting to
/// commit. Both a fresh execution and the delayed commit must refuse it on the new counters.
fn assert_system_loses_size_room(
    set_limit: fn(BlockLimits, u64) -> BlockLimits,
    size: fn(&Recovered<MegaTxEnvelope>) -> u64,
    refusal: &str,
) {
    let system = system_tx();
    let user = user_tx(0, empty_call_gas());
    let policy = set_limit(BlockLimits::default(), size(&system).max(size(&user)));
    let mut state = common::state();
    let mut executor = executor(&mut state, common::block_ctx(policy));
    executor.apply_pre_execution_changes().expect("the block starts");
    let outcome = executor.run_transaction(&system).expect("the system call fits by itself");
    executor.execute_transaction(&user).expect("the user's encoding consumes the room");
    let before = executor.limiter().usage;
    let error =
        executor.commit_transaction_outcome(outcome).expect_err("the encoding no longer fits");
    assert!(error.to_string().contains(refusal), "{error}");
    let error = executor.run_transaction(&system).expect_err("a fresh execution also cannot fit");
    assert!(error.to_string().contains(refusal), "{error}");
    assert_eq!(executor.limiter().usage, before);
    assert_eq!(executor.receipts().len(), 1);
}

#[test]
fn test_a_delayed_system_transaction_obeys_remaining_encoded_size() {
    assert_system_loses_size_room(
        BlockLimits::with_block_txs_encode_size_limit,
        MegaTransactionExt::tx_size,
        "Block transactions encode size limit exceeded",
    );
}

#[test]
fn test_a_delayed_system_transaction_obeys_remaining_da_size() {
    assert_system_loses_size_room(
        BlockLimits::with_block_da_size_limit,
        MegaTransactionExt::estimated_da_size,
        "Block data availability size limit exceeded",
    );
}

/// The header's gas limit still holds a Mega System Transaction.
#[test]
fn test_a_mega_system_transaction_is_still_refused_by_the_header_gas_limit() {
    let mut state = common::state();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let err = executor
        .execute_transaction(&system_call(Bytes::new(), BLOCK_GAS_LIMIT + 1))
        .expect_err("it declares more gas than the header allows");
    assert!(
        matches!(
            err,
            BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. }
            )
        ),
        "{err}"
    );
    assert!(executor.receipts().is_empty());
}

/// The data-availability footprint still holds a Mega System Transaction, as it holds any
/// transaction of a legacy envelope.
#[test]
fn test_a_mega_system_transaction_is_still_refused_by_the_da_footprint() {
    let mut db = common::database();
    let mut word = [0_u8; 32];
    word[DA_FOOTPRINT_GAS_SCALAR_OFFSET..DA_FOOTPRINT_GAS_SCALAR_OFFSET + 2]
        .copy_from_slice(&u16::MAX.to_be_bytes());
    db.set_account_storage(
        L1_BLOCK_CONTRACT,
        DA_FOOTPRINT_GAS_SCALAR_SLOT,
        U256::from_be_bytes(word),
    );
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let big = system_call(common::incompressible(100_000), 3_000_000);
    let err = executor.execute_transaction(&big).expect_err("the footprint does not fit");
    assert!(
        format!("{err}").contains("DA footprint exceeds available block DA footprint"),
        "{err}"
    );
}

/// The live system address sends the system shape or nothing: a transaction from it to a contract
/// off the whitelist is no Mega System Transaction, so the budget refuses it before it runs.
#[test]
fn test_an_unshaped_transaction_from_the_system_address_is_no_system_transaction() {
    let mut state = common::state();
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_data_limit(0)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let input = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let off_whitelist = Recovered::new_unchecked(
        common::tx(0, CONTRACT, input.into(), 1_000_000),
        MEGA_SYSTEM_ADDRESS,
    );
    let err = executor.execute_transaction(&off_whitelist).expect_err("not a system transaction");
    assert!(format!("{err}").contains("Block transactions data limit reached"), "{err}");
}
