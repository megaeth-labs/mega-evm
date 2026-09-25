//! The `SequencerRegistry`'s role changes, applied before a block's transactions.
//!
//! The registry is deployed before every block. After the deploy, the executor reads whether a
//! role change is due in the block and, when one is, makes the `applyPendingChanges()` system
//! call. Each step's state reaches the pre-block observer before it is committed.

use std::sync::{Arc, Mutex};

use alloy_evm::{block::BlockExecutor, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    system::{
        storage_slots::{
            CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, PENDING_SEQUENCER, PENDING_SYSTEM_ADDRESS,
            SEQUENCER_ACTIVATION_BLOCK, SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        },
        MEGA_SYSTEM_ADDRESS, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
    },
    test_utils::MemoryDatabase,
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvm, MegaEvmFactory,
    MegaHardforkConfig, PreBlockStateSource, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{database::State, inspector::NoOpInspector, state::EvmState, Database};

use crate::common::{self, pre_block_states, PreBlockLog, BLOCK_NUMBER};

/// The system address a pending change rotates to.
const NEXT_SYSTEM_ADDRESS: Address = address!("0x3000000000000000000000000000000000000003");

/// The sequencer a pending change rotates to.
const NEXT_SEQUENCER: Address = address!("0x4000000000000000000000000000000000000004");

/// The capacity a crowded arm puts every bucket at, in minimum buckets: a fresh slot would cost
/// 2,000 times the schedule's entry, far past what a 30M call could pay.
const CROWDED: u64 = 2_000;

/// A gas limit large enough for the crowded price, had the call paid it.
const LARGE_BLOCK_GAS_LIMIT: u64 = 250_000_000;

/// The external environments of these tests: every bucket at `m` times the minimum.
type Envs = TestExternalEnvs;

/// The executor these tests drive.
type Executor<'a> = MegaBlockExecutor<
    MegaEvm<&'a mut State<MemoryDatabase>, NoOpInspector, Envs>,
    OpAlloyReceiptBuilder,
    MegaHardforkConfig,
>;

fn word(address: Address) -> U256 {
    U256::from_be_bytes(address.into_word().0)
}

/// A database holding a deployed registry with `slots` on top of the bootstrap roles, and the
/// accounts the tests' transactions come from.
fn registry_with(slots: &[(U256, U256)]) -> MemoryDatabase {
    let mut db = common::database();
    db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE);
    db.set_account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SYSTEM_ADDRESS,
        word(MEGA_SYSTEM_ADDRESS),
    );
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SEQUENCER, word(common::SEQUENCER));
    for &(slot, value) in slots {
        db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, slot, value);
    }
    db
}

/// The slots of a system-address change due at `activation`.
fn system_address_change(activation: u64) -> [(U256, U256); 2] {
    [
        (PENDING_SYSTEM_ADDRESS, word(NEXT_SYSTEM_ADDRESS)),
        (SYSTEM_ADDRESS_ACTIVATION_BLOCK, U256::from(activation)),
    ]
}

/// The slots of a sequencer change due at `activation`.
fn sequencer_change(activation: u64) -> [(U256, U256); 2] {
    [
        (PENDING_SEQUENCER, word(NEXT_SEQUENCER)),
        (SEQUENCER_ACTIVATION_BLOCK, U256::from(activation)),
    ]
}

/// Every bucket at `m` times the minimum capacity.
fn envs_at(m: u64) -> Envs {
    TestExternalEnvs::new().with_default_bucket_capacity(MIN_BUCKET_SIZE as u64 * m)
}

/// An executor over `state` reading `envs`, in a block at [`BLOCK_NUMBER`] whose gas limit is
/// `gas_limit`.
fn executor(state: &mut State<MemoryDatabase>, envs: Envs, gas_limit: u64) -> Executor<'_> {
    let mut env = common::evm_env();
    env.block_env.gas_limit = gas_limit;
    let evm = MegaEvmFactory::new().with_external_env_factory(envs).create_evm(state, env);
    let ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    MegaBlockExecutor::new(evm, ctx, common::chain_spec(), OpAlloyReceiptBuilder::default())
}

/// Installs a recording observer on `executor` and returns the log it writes.
fn record(executor: &mut Executor<'_>) -> PreBlockLog {
    let log = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&log);
    executor.set_pre_block_observer(Some(Box::new(
        move |source: PreBlockStateSource, state: &EvmState| {
            captured.lock().expect("pre-block observer").push((source, state.clone()));
        },
    )));
    log
}

/// The registry's own steps the observer received, in order: the deploy of it, the read of its
/// pending changes and the call that applies them.
fn registry_steps(log: &PreBlockLog) -> Vec<(PreBlockStateSource, EvmState)> {
    pre_block_states(log)
        .into_iter()
        .filter(|(source, _)| {
            matches!(
                source,
                PreBlockStateSource::SystemContract(SEQUENCER_REGISTRY_ADDRESS) |
                    PreBlockStateSource::PendingChanges |
                    PreBlockStateSource::ApplyPendingChanges
            )
        })
        .collect()
}

/// Starts a block at [`BLOCK_NUMBER`] over `state` and answers the registry's steps the observer
/// received.
fn start_block(
    state: &mut State<MemoryDatabase>,
    envs: Envs,
    gas_limit: u64,
) -> Vec<(PreBlockStateSource, EvmState)> {
    let mut executor = executor(state, envs, gas_limit);
    let log = record(&mut executor);
    executor.apply_pre_execution_changes().expect("the block starts");
    registry_steps(&log)
}

/// The registry's `slot`, as the block left it.
fn slot(state: &mut State<MemoryDatabase>, slot: U256) -> U256 {
    state.storage(SEQUENCER_REGISTRY_ADDRESS, slot).expect("the registry is readable")
}

fn sources(steps: &[(PreBlockStateSource, EvmState)]) -> Vec<PreBlockStateSource> {
    steps.iter().map(|(source, _)| *source).collect()
}

/// A system-address change due in this block is applied before the block's transactions, in a
/// block whose every bucket is crowded: the call is the protocol's own, priced at the minimum
/// bucket, so the crowding neither halts it nor makes it read a capacity. The observer receives
/// the read that found the change due, then the call's state.
#[test]
fn test_a_due_change_is_applied_under_a_crowded_bucket() {
    let envs = envs_at(CROWDED);
    let mut state =
        State::builder().with_database(registry_with(&system_address_change(BLOCK_NUMBER))).build();
    let steps = start_block(&mut state, envs.clone(), LARGE_BLOCK_GAS_LIMIT);

    assert_eq!(
        sources(&steps),
        [
            PreBlockStateSource::SystemContract(SEQUENCER_REGISTRY_ADDRESS),
            PreBlockStateSource::PendingChanges,
            PreBlockStateSource::ApplyPendingChanges,
        ]
    );
    assert_eq!(envs.total_bucket_queries(), 0, "no pre-block step read a capacity");
    assert_eq!(slot(&mut state, CURRENT_SYSTEM_ADDRESS), word(NEXT_SYSTEM_ADDRESS));
    assert_eq!(slot(&mut state, PENDING_SYSTEM_ADDRESS), U256::ZERO, "the pending slot is cleared");
    assert_eq!(slot(&mut state, SYSTEM_ADDRESS_ACTIVATION_BLOCK), U256::ZERO);
}

/// A block with nothing due makes no call: the observer receives the read that found nothing
/// due, both pending slots and nothing else, read-only. On the chain's first block that read
/// finds the registry the deploy before it just created.
#[test]
fn test_a_block_with_nothing_due_makes_no_call() {
    for db in [common::database(), registry_with(&[])] {
        let mut state = State::builder().with_database(db).build();
        let steps = start_block(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
        assert_eq!(
            sources(&steps),
            [
                PreBlockStateSource::SystemContract(SEQUENCER_REGISTRY_ADDRESS),
                PreBlockStateSource::PendingChanges
            ]
        );
        let registry = &steps[1].1[&SEQUENCER_REGISTRY_ADDRESS];
        assert!(!registry.is_touched(), "a read-only entry");
        assert_eq!(registry.storage.len(), 2);
        assert!(registry.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(registry.storage.contains_key(&PENDING_SEQUENCER));
    }
}

/// A change scheduled for a later block is left pending: nothing is due, and no call is made.
#[test]
fn test_a_change_not_yet_due_makes_no_call() {
    let db = registry_with(&system_address_change(BLOCK_NUMBER + 1));
    let mut state = State::builder().with_database(db).build();
    let steps = start_block(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    assert_eq!(sources(&steps).last(), Some(&PreBlockStateSource::PendingChanges));
    assert_eq!(slot(&mut state, CURRENT_SYSTEM_ADDRESS), word(MEGA_SYSTEM_ADDRESS));
    assert_eq!(slot(&mut state, PENDING_SYSTEM_ADDRESS), word(NEXT_SYSTEM_ADDRESS));
}

/// A sequencer change due in this block is applied by the same call, and leaves the system
/// address as it was.
#[test]
fn test_a_due_sequencer_change_is_applied() {
    let mut state =
        State::builder().with_database(registry_with(&sequencer_change(BLOCK_NUMBER))).build();
    let steps = start_block(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    assert_eq!(sources(&steps).last(), Some(&PreBlockStateSource::ApplyPendingChanges));
    assert_eq!(slot(&mut state, CURRENT_SEQUENCER), word(NEXT_SEQUENCER));
    assert_eq!(slot(&mut state, PENDING_SEQUENCER), U256::ZERO);
    assert_eq!(slot(&mut state, SEQUENCER_ACTIVATION_BLOCK), U256::ZERO);
    assert_eq!(slot(&mut state, CURRENT_SYSTEM_ADDRESS), word(MEGA_SYSTEM_ADDRESS));
}
