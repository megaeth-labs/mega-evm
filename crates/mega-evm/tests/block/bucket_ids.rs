//! The SALT buckets a block's execution asks about, which a stateless witness needs.
//!
//! A bucket's capacity is read through the SALT environment, a side channel no database sees, and
//! a validator that lacks a bucket's proof cannot price the charge that landed in it. The block
//! executor exports every bucket the block asked about, across all of its transactions: the
//! per-transaction multiplier cache is forgotten before every transaction, so a record that lived
//! there would lose every transaction's buckets but the last one's.

use alloy_evm::{block::BlockExecutor, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BucketId, MegaBlockExecutor, MegaEvm, MegaEvmFactory, MegaHardforkConfig, SaltEnv,
    TestExternalEnvs,
};
use revm::{database::State, inspector::NoOpInspector};

use crate::common::{self, system_tx, CALLER, CONTRACT};

/// The contract whose slots the block's transactions write.
const WRITER: Address = address!("0x1000000000000000000000000000000000000b1d");

/// Room for a slot's first write at the byte prices in effect, and for a keyless-sized body.
const TX_GAS_LIMIT: u64 = 10_000_000;

/// The external environments of these tests, which can make a bucket's lookup fail.
type Envs = TestExternalEnvs<String>;

/// The executor these tests drive.
type Executor<'a> = MegaBlockExecutor<
    MegaEvm<&'a mut State<MemoryDatabase>, NoOpInspector, Envs>,
    OpAlloyReceiptBuilder,
    MegaHardforkConfig,
>;

/// The bucket the slot `slot` of [`WRITER`] lives in.
fn slot_bucket(slot: u8) -> BucketId {
    <Envs as SaltEnv>::bucket_id_for_slot(WRITER, U256::from(slot))
}

/// Code that writes 1 into the slot named by the first byte of its calldata.
fn writer() -> Bytes {
    use revm::bytecode::opcode::{CALLDATALOAD, PUSH0, SHR, SSTORE, STOP};
    BytecodeBuilder::default()
        .push_number(1_u8)
        .append_many([PUSH0, CALLDATALOAD])
        .push_number(248_u8)
        .append_many([SHR, SSTORE, STOP])
        .build()
}

/// A state holding the writer, the empty callee and the caller's balance.
fn state() -> State<MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(WRITER, writer());
    State::builder().with_database(db).build()
}

/// An executor over `state` reading `envs`.
fn executor(state: &mut State<MemoryDatabase>, envs: Envs) -> Executor<'_> {
    let evm =
        MegaEvmFactory::new().with_external_env_factory(envs).create_evm(state, common::evm_env());
    MegaBlockExecutor::new(
        evm,
        common::unlimited_ctx(),
        common::chain_spec(),
        OpAlloyReceiptBuilder::default(),
    )
}

/// The transaction of `nonce` that writes the fresh slot `slot` of [`WRITER`].
fn write(
    nonce: u64,
    slot: u8,
) -> alloy_consensus::transaction::Recovered<mega_evm::MegaTxEnvelope> {
    common::recovered(common::tx(nonce, WRITER, vec![slot].into(), TX_GAS_LIMIT))
}

/// The record accumulates over the block's transactions where the per-transaction cache does
/// not, holds each bucket once, and can be cleared between transactions to attribute the asks.
/// Clearing changes no execution result.
#[test]
fn test_accessed_bucket_ids_accumulate_over_the_block() {
    // Where a state byte is free the engine prices no charge and asks about no bucket.
    if common::state_is_free() {
        return;
    }
    let envs = Envs::new();
    let mut state = state();
    let mut executor = executor(&mut state, envs.clone());
    executor.apply_pre_execution_changes().expect("the block starts");
    assert!(executor.get_accessed_bucket_ids().is_empty(), "the pre-block phase asked nothing");

    let mut expected = vec![];
    for (nonce, slot) in [(0, 1), (1, 2), (2, 3)] {
        let outcome = executor.run_transaction(&write(nonce, slot)).expect("it executes");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(outcome.gas.state, common::slot_state_gas(), "a fresh slot, priced");
        executor.commit_transaction_outcome(outcome).expect("the block has room");
        expected.push(slot_bucket(slot));
        expected.sort_unstable();
        assert_eq!(
            executor.get_accessed_bucket_ids(),
            expected,
            "after the write of slot {slot}: every bucket the block asked about so far"
        );
        assert_eq!(
            executor.evm().ctx().bucket_multipliers().cached_buckets().collect::<Vec<_>>(),
            vec![slot_bucket(slot)],
            "the cache holds the running transaction's bucket alone"
        );
    }
    for slot in [1, 2, 3] {
        assert_eq!(envs.bucket_queries(slot_bucket(slot)), 1, "the environment was asked once");
    }

    // Clearing attributes the asks that follow to one transaction.
    executor.clear_accessed_bucket_ids();
    assert!(executor.get_accessed_bucket_ids().is_empty());
    let outcome = executor.run_transaction(&write(3, 7)).expect("it executes");
    executor.commit_transaction_outcome(outcome).expect("the block has room");
    assert_eq!(executor.get_accessed_bucket_ids(), vec![slot_bucket(7)]);

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 4, "clearing packed nothing differently");
}

/// The record covers one block: an EVM carried over to the next block reports that block's
/// buckets and not the ones before it.
#[test]
fn test_two_blocks_over_the_same_evm_report_only_their_own_buckets() {
    if common::state_is_free() {
        return;
    }
    let mut state = state();
    let mut executor = executor(&mut state, Envs::new());
    executor.apply_pre_execution_changes().expect("the block starts");
    let outcome = executor.run_transaction(&write(0, 1)).expect("it executes");
    executor.commit_transaction_outcome(outcome).expect("the block has room");
    assert_eq!(executor.get_accessed_bucket_ids(), vec![slot_bucket(1)]);

    let (evm, _) = executor.finish_with_counters().expect("the block finishes");
    let mut executor = MegaBlockExecutor::new(
        evm,
        common::unlimited_ctx(),
        common::chain_spec(),
        OpAlloyReceiptBuilder::default(),
    );
    executor.apply_pre_execution_changes().expect("the next block starts");
    assert!(executor.get_accessed_bucket_ids().is_empty(), "the next block starts empty");
    let outcome = executor.run_transaction(&write(1, 2)).expect("it executes");
    executor.commit_transaction_outcome(outcome).expect("the block has room");
    assert_eq!(
        executor.get_accessed_bucket_ids(),
        vec![slot_bucket(2)],
        "what the block before asked about is not this block's set"
    );
}

/// The protocol's own work reads no bucket: a block of pre-block calls and a system transaction
/// leaves the record empty, and the environment was never asked.
#[test]
fn test_the_pre_block_calls_and_a_system_transaction_add_no_bucket() {
    let envs = Envs::new();
    let mut db = common::database();
    db = db.sequencer_registry(mega_evm::system::MEGA_SYSTEM_ADDRESS);
    db.set_account_code(
        mega_evm::system::ORACLE_CONTRACT_ADDRESS,
        mega_evm::system::ORACLE_CONTRACT_CODE,
    );
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(&mut state, envs.clone());
    executor.apply_pre_execution_changes().expect("the block starts");
    let outcome = executor.run_transaction(&system_tx()).expect("it executes");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(executor.evm().ctx().is_system_originated());
    executor.commit_transaction_outcome(outcome).expect("the block has room");

    assert!(executor.get_accessed_bucket_ids().is_empty());
    assert_eq!(envs.total_bucket_queries(), 0);
}

/// A bucket whose lookup fails is recorded all the same: the ask is made, and the transaction it
/// fails is in no block, so a validator never repeats it.
#[test]
fn test_a_failed_lookup_is_recorded_and_its_transaction_is_not_in_the_block() {
    if common::state_is_free() {
        return;
    }
    let envs = Envs::new().with_failing_bucket(slot_bucket(1), "salt backend unreachable".into());
    let mut state = state();
    let mut executor = executor(&mut state, envs);
    executor.apply_pre_execution_changes().expect("the block starts");

    let error =
        executor.run_transaction(&write(0, 1)).expect_err("the lookup fails the transaction");
    assert!(error.to_string().contains("salt backend unreachable"), "{error}");
    assert_eq!(executor.get_accessed_bucket_ids(), vec![slot_bucket(1)], "the ask is recorded");

    // The block goes on: the next transaction's bucket is answered, and both are recorded.
    let outcome = executor.run_transaction(&write(0, 2)).expect("it executes");
    executor.commit_transaction_outcome(outcome).expect("the block has room");
    let mut expected = vec![slot_bucket(1), slot_bucket(2)];
    expected.sort_unstable();
    assert_eq!(executor.get_accessed_bucket_ids(), expected);
    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1, "the failed transaction is not in the block");
}

/// A transaction that adds no state — a call to an empty account — asks about no bucket, so a
/// block of such transactions exports an empty set.
#[test]
fn test_a_transaction_without_state_charges_asks_about_no_bucket() {
    let envs = Envs::new();
    let mut state = state();
    let mut executor = executor(&mut state, envs.clone());
    executor.apply_pre_execution_changes().expect("the block starts");
    let tx = common::recovered(common::tx(0, CONTRACT, Bytes::new(), common::empty_call_gas()));
    let outcome = executor.run_transaction(&tx).expect("it executes");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0, "{CALLER} adds no state");
    executor.commit_transaction_outcome(outcome).expect("the block has room");

    assert!(executor.get_accessed_bucket_ids().is_empty());
    assert_eq!(envs.total_bucket_queries(), 0);
}
