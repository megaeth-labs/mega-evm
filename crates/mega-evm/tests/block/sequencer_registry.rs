//! The `SequencerRegistry`'s role changes, applied before a block's transactions, and the live
//! system address read out of it.
//!
//! The registry is deployed before every block. After the deploy, the executor reads whether a
//! role change is due in the block and, when one is, makes the `applyPendingChanges()` system
//! call; then it reads the system address the block's system-address transactions must come
//! from. Each step's state reaches the pre-block observer before it is committed.

use std::sync::{Arc, Mutex};

use alloy_consensus::transaction::Recovered;
use alloy_evm::{block::BlockExecutor, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        storage_slots::{
            ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, PENDING_ADMIN, PENDING_SEQUENCER,
            PENDING_SYSTEM_ADDRESS, SEQUENCER_ACTIVATION_BLOCK, SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        },
        IOracle, ISequencerRegistry, SequencerRegistryConfig, MEGA_SYSTEM_ADDRESS,
        ORACLE_CONTRACT_ADDRESS, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
    },
    test_utils::MemoryDatabase,
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvm, MegaEvmFactory,
    MegaHardforkConfig, MegaTxEnvelope, PreBlockStateSource, TestExternalEnvs, MIN_BUCKET_SIZE,
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
    executor_on(state, envs, gas_limit, common::chain_spec())
}

/// [`executor`] on the chain `spec` describes.
fn executor_on(
    state: &mut State<MemoryDatabase>,
    envs: Envs,
    gas_limit: u64,
    spec: MegaHardforkConfig,
) -> Executor<'_> {
    let mut env = common::evm_env();
    env.block_env.gas_limit = gas_limit;
    let evm = MegaEvmFactory::new().with_external_env_factory(envs).create_evm(state, env);
    let ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    MegaBlockExecutor::new(evm, ctx, spec, OpAlloyReceiptBuilder::default())
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
/// pending changes, the call that applies them and the read of the system address.
fn registry_steps(log: &PreBlockLog) -> Vec<(PreBlockStateSource, EvmState)> {
    pre_block_states(log)
        .into_iter()
        .filter(|(source, _)| {
            matches!(
                source,
                PreBlockStateSource::SystemContract(SEQUENCER_REGISTRY_ADDRESS) |
                    PreBlockStateSource::PendingChanges |
                    PreBlockStateSource::ApplyPendingChanges |
                    PreBlockStateSource::SystemAddress
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
    assert_eq!(
        executor.evm().ctx().system_address(),
        Address::from_word(slot_of(&registry_steps(&log), CURRENT_SYSTEM_ADDRESS).into()),
        "the EVM holds the system address the registry read found",
    );
    registry_steps(&log)
}

/// The value the last registry step read or wrote for `slot`.
fn slot_of(steps: &[(PreBlockStateSource, EvmState)], slot: U256) -> U256 {
    steps
        .iter()
        .rev()
        .find_map(|(_, state)| state.get(&SEQUENCER_REGISTRY_ADDRESS)?.storage.get(&slot))
        .map(|slot| slot.present_value())
        .expect("a registry step touched the slot")
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
            PreBlockStateSource::SystemAddress,
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
                PreBlockStateSource::PendingChanges,
                PreBlockStateSource::SystemAddress,
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
    assert!(!sources(&steps).contains(&PreBlockStateSource::ApplyPendingChanges));
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
    assert!(sources(&steps).contains(&PreBlockStateSource::ApplyPendingChanges));
    assert_eq!(slot(&mut state, CURRENT_SEQUENCER), word(NEXT_SEQUENCER));
    assert_eq!(slot(&mut state, PENDING_SEQUENCER), U256::ZERO);
    assert_eq!(slot(&mut state, SEQUENCER_ACTIVATION_BLOCK), U256::ZERO);
    assert_eq!(slot(&mut state, CURRENT_SYSTEM_ADDRESS), word(MEGA_SYSTEM_ADDRESS));
}

/// The system address a chain's schedule seeds the registry with, distinct from
/// [`MEGA_SYSTEM_ADDRESS`], so a test can tell the one read out of the registry from the default.
const GENESIS_SYSTEM_ADDRESS: Address = address!("0x5000000000000000000000000000000000000005");

/// The admin a handoff moves the registry to.
const NEXT_ADMIN: Address = address!("0x6000000000000000000000000000000000000006");

/// A Satin-at-genesis schedule whose registry is seeded with [`GENESIS_SYSTEM_ADDRESS`].
fn chain_seeding_genesis_system_address() -> MegaHardforkConfig {
    MegaHardforkConfig::default().with_all_activated().with_params(SequencerRegistryConfig {
        initial_system_address: GENESIS_SYSTEM_ADDRESS,
        ..common::registry_config()
    })
}

/// A legacy call from `sender` to the Oracle's `getSlot(0)`: a system-address transaction when
/// `sender` is the block's system address, an ordinary one otherwise. It carries a gas price, so
/// an ordinary one needs a balance its sender does not have here.
fn oracle_call_from(sender: Address, nonce: u64) -> Recovered<MegaTxEnvelope> {
    let input = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    Recovered::new_unchecked(
        common::tx(nonce, ORACLE_CONTRACT_ADDRESS, input.into(), 1_000_000),
        sender,
    )
}

/// A transaction from `sender` to the registry carrying `input`.
fn registry_call_from(sender: Address, input: Vec<u8>) -> Recovered<MegaTxEnvelope> {
    Recovered::new_unchecked(
        common::tx(0, SEQUENCER_REGISTRY_ADDRESS, input.into(), 1_000_000),
        sender,
    )
}

/// Asserts `sender` is the block's system address: its Oracle call runs as the protocol's own
/// transaction, with no fee and no history gas, from an account holding nothing.
fn assert_is_the_system_address(executor: &mut Executor<'_>, sender: Address) {
    assert_eq!(executor.evm().ctx().system_address(), sender);
    let outcome = executor
        .run_transaction(&oracle_call_from(sender, 0))
        .expect("a system-address transaction is accepted without a balance");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.history, 0, "the protocol's own transaction pays no history");
}

/// Asserts `sender` is not the block's system address: the same call is an ordinary transaction
/// from an account that cannot pay its fee.
fn assert_is_not_the_system_address(executor: &mut Executor<'_>, sender: Address) {
    assert_ne!(executor.evm().ctx().system_address(), sender);
    let err = executor
        .run_transaction(&oracle_call_from(sender, 0))
        .expect_err("an ordinary transaction from an empty account is refused");
    assert!(err.to_string().contains("lack of funds"), "{err}");
}

/// A fresh chain's first block deploys the registry and reads the system address its schedule
/// seeded it with: the registry's, not the engine's default. The read reaches the observer as a
/// read-only entry of the one slot.
#[test]
fn test_bootstrap_block_resolves_system_address() {
    let mut state = common::state();
    let mut executor = executor_on(
        &mut state,
        envs_at(1),
        common::BLOCK_GAS_LIMIT,
        chain_seeding_genesis_system_address(),
    );
    let log = record(&mut executor);
    executor.apply_pre_execution_changes().expect("the block starts");
    assert_eq!(executor.evm().ctx().system_address(), GENESIS_SYSTEM_ADDRESS);

    let steps = registry_steps(&log);
    let (source, read) = steps.last().expect("the read reached the observer");
    assert_eq!(*source, PreBlockStateSource::SystemAddress);
    let registry = &read[&SEQUENCER_REGISTRY_ADDRESS];
    assert!(!registry.is_touched(), "a read-only entry");
    assert_eq!(registry.storage.len(), 1);
    assert_eq!(
        registry.storage[&CURRENT_SYSTEM_ADDRESS].present_value(),
        word(GENESIS_SYSTEM_ADDRESS)
    );

    drop(executor);

    // The default schedule seeds the engine's default address, and the read finds it.
    let mut state = common::state();
    let mut default = self::executor(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    default.apply_pre_execution_changes().expect("the block starts");
    assert_eq!(default.evm().ctx().system_address(), MEGA_SYSTEM_ADDRESS);
}

/// A system-address transaction is recognised by the address read out of the registry: from it,
/// the Oracle call is the protocol's own; from the engine's default, which is not this chain's
/// system address, it is an ordinary transaction.
#[test]
fn test_system_tx_uses_resolved_system_address() {
    let mut state = common::state();
    let mut executor = executor_on(
        &mut state,
        envs_at(1),
        common::BLOCK_GAS_LIMIT,
        chain_seeding_genesis_system_address(),
    );
    executor.apply_pre_execution_changes().expect("the block starts");
    assert_is_the_system_address(&mut executor, GENESIS_SYSTEM_ADDRESS);
    assert_is_not_the_system_address(&mut executor, MEGA_SYSTEM_ADDRESS);
}

/// A system-address rotation due in this block governs this block: the new address sends the
/// protocol's transactions, and the old one no longer can.
#[test]
fn test_system_address_change() {
    let mut state =
        State::builder().with_database(registry_with(&system_address_change(BLOCK_NUMBER))).build();
    let mut executor = executor(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    executor.apply_pre_execution_changes().expect("the block starts");
    assert_is_the_system_address(&mut executor, NEXT_SYSTEM_ADDRESS);
    assert_is_not_the_system_address(&mut executor, MEGA_SYSTEM_ADDRESS);
}

/// A sequencer rotation leaves the system address as it was.
#[test]
fn test_sequencer_change_does_not_affect_system_address() {
    let mut state =
        State::builder().with_database(registry_with(&sequencer_change(BLOCK_NUMBER))).build();
    let mut executor = executor(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    executor.apply_pre_execution_changes().expect("the block starts");
    assert_is_the_system_address(&mut executor, MEGA_SYSTEM_ADDRESS);
    assert_is_not_the_system_address(&mut executor, NEXT_SEQUENCER);
}

/// Both rotations due in one block: the system address is the new system address, not the new
/// sequencer.
#[test]
fn test_dual_change_in_same_block() {
    let slots: Vec<_> = system_address_change(BLOCK_NUMBER)
        .into_iter()
        .chain(sequencer_change(BLOCK_NUMBER))
        .collect();
    let mut state = State::builder().with_database(registry_with(&slots)).build();
    let mut executor = executor(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    executor.apply_pre_execution_changes().expect("the block starts");
    assert_is_the_system_address(&mut executor, NEXT_SYSTEM_ADDRESS);
    drop(executor);
    assert_eq!(slot(&mut state, CURRENT_SEQUENCER), word(NEXT_SEQUENCER));
}

/// A rotation scheduled for a later block leaves this block's system address as it was.
#[test]
fn test_pending_not_yet_due_is_noop() {
    let mut state = State::builder()
        .with_database(registry_with(&system_address_change(BLOCK_NUMBER + 8_999)))
        .build();
    let mut executor = executor(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    executor.apply_pre_execution_changes().expect("the block starts");
    assert_is_the_system_address(&mut executor, MEGA_SYSTEM_ADDRESS);
    assert_is_not_the_system_address(&mut executor, NEXT_SYSTEM_ADDRESS);
}

/// The two-step admin handoff through ordinary transactions: the admin names the next one, the
/// next one accepts, and the registry's admin slot moves while the pending one is cleared.
#[test]
fn test_admin_handoff_via_block_executor() {
    let mut db = registry_with(&[(ADMIN, word(common::ADMIN))]);
    db.set_account_balance(common::ADMIN, U256::from(10_u64.pow(18)));
    db.set_account_balance(NEXT_ADMIN, U256::from(10_u64.pow(18)));
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    executor.apply_pre_execution_changes().expect("the block starts");

    for (sender, input) in [
        (
            common::ADMIN,
            ISequencerRegistry::transferAdminCall { newAdmin: NEXT_ADMIN }.abi_encode(),
        ),
        (NEXT_ADMIN, ISequencerRegistry::acceptAdminCall {}.abi_encode()),
    ] {
        let outcome =
            executor.run_transaction(&registry_call_from(sender, input)).expect("it executes");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        executor.commit_transaction(outcome);
    }
    drop(executor);

    assert_eq!(slot(&mut state, ADMIN), word(NEXT_ADMIN));
    assert_eq!(slot(&mut state, PENDING_ADMIN), U256::ZERO);
}
