//! The SALT buckets a block's execution asks about, which a stateless witness needs.
//!
//! A bucket's capacity is read through the SALT environment, a side channel no database sees, and
//! a validator that lacks a bucket's proof cannot price the charge that landed in it. The block
//! executor exports every bucket the block asked about, across all of its transactions: the
//! per-transaction multiplier cache is forgotten before every transaction, so a record that lived
//! there would lose every transaction's buckets but the last one's.

use alloy_eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_evm::{block::BlockExecutor, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, BucketId, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvm, MegaEvmFactory,
    MegaHardforkConfig, PreBlockStateSource, SaltEnv, TestExternalEnvs,
};
use revm::{database::State, inspector::NoOpInspector, state::EvmState};

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

/// The protocol's own work reads no bucket: a block whose EIP-2935 pre-block call writes a fresh
/// slot of the history contract and whose system transaction creates its caller leaves the
/// record empty, and the environment was never asked.
#[test]
fn test_the_pre_block_calls_and_a_system_transaction_add_no_bucket() {
    let envs = Envs::new();
    let mut db = common::database();
    db = db.sequencer_registry(mega_evm::system::MEGA_SYSTEM_ADDRESS);
    db.set_account_code(
        mega_evm::system::ORACLE_CONTRACT_ADDRESS,
        mega_evm::system::ORACLE_CONTRACT_CODE,
    );
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    let mut state = State::builder().with_database(db).build();
    // A non-zero parent hash, so the EIP-2935 call's write changes the ring slot it lands in.
    let evm = MegaEvmFactory::new()
        .with_external_env_factory(envs.clone())
        .create_evm(&mut state, common::evm_env());
    let ctx = MegaBlockExecutionCtx::new(
        B256::repeat_byte(0x11),
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    let mut executor =
        MegaBlockExecutor::new(evm, ctx, common::chain_spec(), OpAlloyReceiptBuilder::default());
    let written = std::sync::Arc::new(std::sync::Mutex::new(false));
    let seen = std::sync::Arc::clone(&written);
    executor.set_pre_block_observer(Some(Box::new(
        move |source: PreBlockStateSource, state: &EvmState| {
            if source == PreBlockStateSource::Eip2935 {
                let changed = state[&HISTORY_STORAGE_ADDRESS].changed_storage_slots().count();
                *seen.lock().expect("pre-block observer") = changed > 0;
            }
        },
    )));
    executor.apply_pre_execution_changes().expect("the block starts");
    assert!(*written.lock().expect("pre-block observer"), "the EIP-2935 call wrote a slot");
    assert!(executor.get_accessed_bucket_ids().is_empty(), "priced at the minimum bucket");
    let outcome = executor.run_transaction(&system_tx()).expect("it executes");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(executor.evm().ctx().is_system_originated());
    executor.commit_transaction_outcome(outcome).expect("the block has room");

    assert!(executor.get_accessed_bucket_ids().is_empty());
    assert_eq!(envs.total_bucket_queries(), 0);
}

/// A bucket whose lookup fails is not recorded: the transaction it fails is in no block, so a
/// validator never makes the lookup, and the builder is not held to proving a bucket its
/// environment could not answer.
#[test]
fn test_a_failed_lookup_is_not_recorded_and_its_transaction_is_not_in_the_block() {
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
    assert!(executor.get_accessed_bucket_ids().is_empty(), "a failed lookup is not recorded");

    // The block goes on: the next transaction's bucket is answered and recorded.
    let outcome = executor.run_transaction(&write(0, 2)).expect("it executes");
    executor.commit_transaction_outcome(outcome).expect("the block has room");
    assert_eq!(executor.get_accessed_bucket_ids(), vec![slot_bucket(2)]);
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
