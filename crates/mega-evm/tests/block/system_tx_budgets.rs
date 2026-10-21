//! A Mega System Transaction against the block's packing budgets: like a deposit, no data-size
//! budget and no building policy refuses it, and it counts towards both; the header's gas limit and
//! the data-availability footprint still hold it.

use alloy_consensus::transaction::Recovered;
use alloy_eips::Encodable2718;
use alloy_evm::block::{BlockExecutionError, BlockExecutor, BlockValidationError};
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    BlockLimits, MegaTransactionExt, MegaTxEnvelope,
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

/// The whole of the policy a builder could tighten, at its tightest.
fn tightest_policy() -> BlockLimits {
    BlockLimits::no_limits()
        .with_tx_gas_limit(1)
        .with_tx_encode_size_limit(1)
        .with_block_txs_encode_size_limit(1)
        .with_tx_da_size_limit(0)
        .with_block_da_size_limit(0)
        .with_block_txs_data_limit(0)
}

/// A block whose data-size budget is spent before it starts refuses an ordinary transaction and
/// admits a Mega System Transaction, which counts towards the budget all the same.
#[test]
fn test_a_mega_system_transaction_is_admitted_at_a_full_data_size_budget() {
    let mut state = common::state();
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_data_limit(0)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    executor
        .execute_transaction(&user_tx(0, empty_call_gas()))
        .expect_err("an ordinary transaction finds the budget spent");

    let outcome =
        executor.run_transaction(&system_tx()).expect("the system transaction is admitted");
    assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);
    let data_size = outcome.inner.usage.data_size;
    assert!(data_size > 0);
    executor.commit_transaction_outcome(outcome).expect("and committed");

    assert_eq!(executor.limiter().usage.data_size, data_size, "it counts towards the budget");
    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
}

/// A building policy at its tightest refuses an ordinary transaction and no system transaction:
/// the system transaction's size counts towards the block's encoded and data-availability sizes.
#[test]
fn test_a_mega_system_transaction_is_admitted_under_a_tightened_building_policy() {
    let mut state = common::state();
    let mut executor = executor(&mut state, common::block_ctx(tightest_policy()));
    executor.apply_pre_execution_changes().expect("the block starts");

    executor
        .execute_transaction(&user_tx(0, empty_call_gas()))
        .expect_err("the policy refuses an ordinary transaction");

    let system = system_tx();
    let outcome = executor.run_transaction(&system).expect("the policy does not refuse it");
    assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);
    executor.commit_transaction_outcome(outcome).expect("and it commits");

    assert_eq!(executor.limiter().block_tx_size_used, system.inner().encode_2718_len() as u64);
    assert_eq!(
        executor.limiter().block_da_size_used,
        MegaTransactionExt::estimated_da_size(&system)
    );
}

/// The commit-time check reads the same decision: an outcome executed while the budget had room
/// still commits after another commit spent it, where an ordinary transaction's outcome does not.
#[test]
fn test_a_delayed_commit_of_a_mega_system_transaction_ignores_the_spent_budget() {
    let mut db = common::database();
    db.set_account_balance(CALLER2, U256::from(1_000_000_000_000_000_u64));
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(
        &mut state,
        common::block_ctx(BlockLimits::no_limits().with_block_txs_data_limit(1)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let first = executor.run_transaction(&user_tx(0, empty_call_gas())).expect("room");
    let second = executor
        .run_transaction(&Recovered::new_unchecked(
            common::tx(0, CONTRACT, Bytes::new(), empty_call_gas()),
            CALLER2,
        ))
        .expect("room");
    let system = executor.run_transaction(&system_tx()).expect("room");

    executor.commit_transaction_outcome(first).expect("the first fills the budget");
    executor.commit_transaction_outcome(system).expect("a system transaction does not need room");
    executor.commit_transaction_outcome(second).expect_err("an ordinary transaction does");
}

/// The header's gas limit still holds a Mega System Transaction.
#[test]
fn test_a_mega_system_transaction_is_still_refused_by_the_header_gas_limit() {
    let mut state = common::state();
    let mut executor = executor(&mut state, common::block_ctx(tightest_policy()));
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
    let mut executor = executor(&mut state, common::block_ctx(tightest_policy()));
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
