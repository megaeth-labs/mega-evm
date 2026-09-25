//! The `SequencerRegistry`'s role changes, applied before a block's transactions, and the live
//! system address read out of it.
//!
//! The registry is deployed before every block. After the deploy, the executor reads whether a
//! role change is due in the block and, when one is, makes the `applyPendingChanges()` system
//! call. Each step's state reaches the pre-block observer before it is committed. The executor
//! does not read the system address: a transaction of the system shape reads it out of the
//! registry when it is validated, so every EVM — the block's, or one a node builds outside block
//! execution — promotes the address the state names.

use std::sync::{Arc, Mutex};

use alloy_consensus::transaction::Recovered;
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory, ToTxEnv};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::{SolCall, SolValue};
use mega_evm::{
    system::{
        storage_slots::{
            ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, MIN_ROTATION_DELAY, PENDING_ADMIN,
            PENDING_SEQUENCER, PENDING_SYSTEM_ADDRESS, SEQUENCER_ACTIVATION_BLOCK,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        },
        IOracle, ISequencerRegistry, SequencerRegistryConfig, MEGA_SYSTEM_ADDRESS,
        ORACLE_CONTRACT_ADDRESS, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
        SEQUENCER_REGISTRY_CODE_HASH,
    },
    test_utils::MemoryDatabase,
    BlockLimits, ExternalEnvFactory, MegaBlockExecutionCtx, MegaBlockExecutor, MegaContext,
    MegaEvm, MegaEvmFactory, MegaHardforkConfig, MegaTxEnvelope, PreBlockStateSource,
    TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{database::State, inspector::NoOpInspector, state::EvmState, Database};

use crate::common::{self, pre_block_states, PreBlockLog, BLOCK_NUMBER};

/// The outcome of a transaction the tests run.
type Outcome = mega_evm::MegaBlockTxResult<mega_evm::MegaTxType>;

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
                PreBlockStateSource::PendingChanges,
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
/// `sender` is the system address the registry names, an ordinary one otherwise. It carries a
/// gas price, so an ordinary one needs a balance its sender does not have here. `getSlot` does not
/// read the registry itself, so what the transaction's state holds of the registry is what its
/// validation read.
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
/// transaction, with no fee and no history gas, from an account holding nothing. Answers the
/// outcome, not committed.
fn assert_is_the_system_address(executor: &mut Executor<'_>, sender: Address) -> Outcome {
    let outcome = executor
        .run_transaction(&oracle_call_from(sender, 0))
        .expect("a system-address transaction is accepted without a balance");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.history, 0, "the protocol's own transaction pays no history");
    assert!(executor.evm().ctx().is_system_originated(), "it is the protocol's own transaction");
    outcome
}

/// Asserts `sender` is not the block's system address: the same call is an ordinary transaction
/// from an account that cannot pay its fee.
fn assert_is_not_the_system_address(executor: &mut Executor<'_>, sender: Address) {
    let err = executor
        .run_transaction(&oracle_call_from(sender, 0))
        .expect_err("an ordinary transaction from an empty account is refused");
    assert!(err.to_string().contains("lack of funds"), "{err}");
}

/// Asserts `state` holds what a system-address transaction's validation read: the registry's
/// account, untouched, with the one slot naming `system_address`, unchanged.
fn assert_carries_the_system_address_read(state: &EvmState, system_address: Address) {
    let registry = &state[&SEQUENCER_REGISTRY_ADDRESS];
    assert!(!registry.is_touched(), "a read-only entry");
    assert_eq!(registry.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
    assert_eq!(registry.storage.len(), 1, "exactly the one slot read");
    let slot = &registry.storage[&CURRENT_SYSTEM_ADDRESS];
    assert!(!slot.is_changed());
    assert_eq!(slot.present_value(), word(system_address));
}

/// A fresh chain's first block deploys the registry seeded with the system address its schedule
/// names, and that address is the system address from the block's first transaction: the
/// registry's, not the engine's default. No pre-block step reads it; the system-address
/// transaction does, and its state carries the read.
#[test]
fn test_the_bootstrap_block_promotes_the_seeded_system_address() {
    let mut state = common::state();
    let mut executor = executor_on(
        &mut state,
        envs_at(1),
        common::BLOCK_GAS_LIMIT,
        chain_seeding_genesis_system_address(),
    );
    let log = record(&mut executor);
    executor.apply_pre_execution_changes().expect("the block starts");
    let sources = sources(&registry_steps(&log));
    assert_eq!(
        sources,
        [
            PreBlockStateSource::SystemContract(SEQUENCER_REGISTRY_ADDRESS),
            PreBlockStateSource::PendingChanges,
        ]
    );
    let outcome = assert_is_the_system_address(&mut executor, GENESIS_SYSTEM_ADDRESS);
    assert_carries_the_system_address_read(&outcome.inner.state, GENESIS_SYSTEM_ADDRESS);
    assert_is_not_the_system_address(&mut executor, MEGA_SYSTEM_ADDRESS);
    drop(executor);

    // The registry is the one this engine ships, seeded with the schedule's roles and delay.
    assert_eq!(
        state.basic(SEQUENCER_REGISTRY_ADDRESS).unwrap().unwrap().code_hash,
        SEQUENCER_REGISTRY_CODE_HASH,
    );
    assert_eq!(slot(&mut state, CURRENT_SYSTEM_ADDRESS), word(GENESIS_SYSTEM_ADDRESS));
    assert_eq!(
        slot(&mut state, MIN_ROTATION_DELAY),
        U256::from(common::registry_config().min_rotation_delay)
    );
    assert_eq!(slot(&mut state, CURRENT_SEQUENCER), word(common::SEQUENCER));

    // The default schedule seeds the engine's default address, and it is the system address.
    let mut state = common::state();
    let mut default = self::executor(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    default.apply_pre_execution_changes().expect("the block starts");
    assert_is_the_system_address(&mut default, MEGA_SYSTEM_ADDRESS);
}

/// Once the registry is in place, no pre-block step carries the system address's slot: the
/// deploy leaves the registry as a read-only entry without storage, and the read of the pending
/// changes reads the pending slots alone. The slot is in the system-address transaction's own
/// state instead, read-only and cold, as its validation read it.
#[test]
fn test_the_slot_is_in_the_system_transactions_state_not_the_pre_block_witness() {
    let mut state = State::builder().with_database(registry_with(&[])).build();
    let mut executor = executor(&mut state, envs_at(1), common::BLOCK_GAS_LIMIT);
    let log = record(&mut executor);
    executor.apply_pre_execution_changes().expect("the block starts");
    for (source, state) in pre_block_states(&log) {
        let slot = state
            .get(&SEQUENCER_REGISTRY_ADDRESS)
            .and_then(|registry| registry.storage.get(&CURRENT_SYSTEM_ADDRESS));
        assert!(slot.is_none(), "{source:?} carries the system address's slot");
    }

    let outcome = assert_is_the_system_address(&mut executor, MEGA_SYSTEM_ADDRESS);
    assert_carries_the_system_address_read(&outcome.inner.state, MEGA_SYSTEM_ADDRESS);
    let registry = &outcome.inner.state[&SEQUENCER_REGISTRY_ADDRESS];
    assert!(registry.storage[&CURRENT_SYSTEM_ADDRESS].is_cold, "the read left the slot cold");
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

/// A rotation governs every block from its activation block on, and no block before it: the
/// block before promotes the old address and not the new one, the activation block and the block
/// after it the new one and not the old.
#[test]
fn test_a_rotation_governs_the_blocks_from_its_activation_block() {
    let activation = BLOCK_NUMBER + 1;
    let mut state =
        State::builder().with_database(registry_with(&system_address_change(activation))).build();

    let mut before = started_block_at(&mut state, activation - 1);
    assert_is_the_system_address(&mut before, MEGA_SYSTEM_ADDRESS);
    assert_is_not_the_system_address(&mut before, NEXT_SYSTEM_ADDRESS);
    drop(before);

    for number in [activation, activation + 1] {
        let mut block = started_block_at(&mut state, number);
        let outcome = assert_is_the_system_address(&mut block, NEXT_SYSTEM_ADDRESS);
        assert_carries_the_system_address_read(&outcome.inner.state, NEXT_SYSTEM_ADDRESS);
        assert_is_not_the_system_address(&mut block, MEGA_SYSTEM_ADDRESS);
    }
    assert_eq!(slot(&mut state, PENDING_SYSTEM_ADDRESS), U256::ZERO, "applied once");
}

/// An EVM a node builds outside block execution, over the state these tests run on.
type OutsideEvm<'a> = MegaEvm<&'a mut State<MemoryDatabase>, NoOpInspector, Envs>;

/// A context over `state` for the block [`executor`] runs, as a node builds one outside block
/// execution: no block is started on it and nothing tells it the system address.
fn context_outside_block<'a>(
    state: &'a mut State<MemoryDatabase>,
    envs: &Envs,
) -> MegaContext<&'a mut State<MemoryDatabase>, Envs> {
    let EvmEnv { cfg_env, block_env } = common::evm_env();
    MegaContext::new_with_external_envs(state, cfg_env.spec, envs.external_envs(BLOCK_NUMBER))
        .with_cfg(cfg_env)
        .with_block(block_env)
}

/// An EVM a node builds outside block execution — for an RPC call, or to replay a block's
/// transactions for a trace — starts no block and is told nothing, and on a state after a
/// rotation it promotes the rotated address's Oracle transaction exactly as the block executor
/// does: the same result, state, gas and usage, whether it was built from a context or by the
/// factory. The address the rotation retired sends an ordinary transaction, which its empty
/// account cannot pay for.
#[test]
fn test_an_evm_outside_block_execution_promotes_the_rotated_address() {
    let envs = envs_at(1);
    let mut state =
        State::builder().with_database(registry_with(&system_address_change(BLOCK_NUMBER))).build();
    let tx = oracle_call_from(NEXT_SYSTEM_ADDRESS, 0);

    let mut executor = executor(&mut state, envs.clone(), common::BLOCK_GAS_LIMIT);
    executor.apply_pre_execution_changes().expect("the block starts");
    let in_block = assert_is_the_system_address(&mut executor, NEXT_SYSTEM_ADDRESS).inner;
    drop(executor);

    // The state now holds the rotation the block applied, as the one a node's EVM runs on does.
    let assert_as_in_block = |evm: &mut OutsideEvm<'_>| {
        let outside = evm.execute_transaction(tx.to_tx_env()).expect("promoted");
        assert!(evm.ctx().is_system_originated(), "it is the protocol's own transaction");
        assert_eq!(outside.result, in_block.result);
        assert_eq!(outside.state, in_block.state);
        assert_eq!(outside.gas, in_block.gas);
        assert_eq!(outside.usage, in_block.usage);
        assert_eq!(outside.limit_exceeded, in_block.limit_exceeded);

        let retired = oracle_call_from(MEGA_SYSTEM_ADDRESS, 0);
        let err = evm
            .execute_transaction(retired.to_tx_env())
            .expect_err("an ordinary transaction from an empty account is refused");
        assert!(err.to_string().contains("lack of funds"), "{err}");
        assert!(!evm.ctx().is_system_originated());
    };
    assert_as_in_block(&mut MegaEvm::new(context_outside_block(&mut state, &envs)));
    assert_as_in_block(
        &mut MegaEvmFactory::new()
            .with_external_env_factory(envs.clone())
            .create_evm(&mut state, common::evm_env()),
    );
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

/// The EIP-712 digest of a rotation to `new_sequencer` at `activation_block`, built here from the
/// contract's domain and type strings rather than asked of the contract, so a test that the
/// contract accepts a proof over it cross-checks the deployed bytecode.
fn rotation_digest(new_sequencer: Address, activation_block: U256) -> B256 {
    let domain_typehash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let rotation_typehash =
        keccak256(b"SequencerRotation(address newSequencer,uint256 activationBlock)");
    let domain_separator = keccak256(
        (
            domain_typehash,
            keccak256(b"MegaETH SequencerRegistry"),
            keccak256(b"1"),
            U256::from(common::CHAIN_ID),
            SEQUENCER_REGISTRY_ADDRESS,
        )
            .abi_encode(),
    );
    let struct_hash = keccak256((rotation_typehash, new_sequencer, activation_block).abi_encode());
    let mut preimage = Vec::with_capacity(66);
    preimage.extend_from_slice(b"\x19\x01");
    preimage.extend_from_slice(domain_separator.as_slice());
    preimage.extend_from_slice(struct_hash.as_slice());
    keccak256(&preimage)
}

/// The key of the sequencer a rotation brings in, and its address.
fn next_sequencer_key() -> (Address, B256) {
    use alloy_consensus::crypto::secp256k1::{recover_signer, sign_message};
    let secret = B256::from(U256::from(0x5ec5_ec5e_c5ec_u64));
    let probe = B256::from(U256::from(1));
    let signature = sign_message(secret, probe).expect("the key signs");
    (recover_signer(&signature, probe).expect("the signer recovers"), secret)
}

/// The 65-byte `(r, s, v)` proof, signed by `secret`, that its key takes over at
/// `activation_block`.
fn rotation_proof(secret: B256, new_sequencer: Address, activation_block: U256) -> Bytes {
    let digest = rotation_digest(new_sequencer, activation_block);
    let signature =
        alloy_consensus::crypto::secp256k1::sign_message(secret, digest).expect("the key signs");
    let mut proof = Vec::with_capacity(65);
    proof.extend_from_slice(&signature.r().to_be_bytes::<32>());
    proof.extend_from_slice(&signature.s().to_be_bytes::<32>());
    proof.push(27 + u8::from(signature.v()));
    proof.into()
}

/// A block at `number` over `state`, its pre-block changes applied.
fn started_block_at(state: &mut State<MemoryDatabase>, number: u64) -> Executor<'_> {
    let mut env = common::evm_env();
    env.block_env.number = U256::from(number);
    let evm = MegaEvmFactory::new().with_external_env_factory(envs_at(1)).create_evm(state, env);
    let ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );
    let mut executor =
        MegaBlockExecutor::new(evm, ctx, common::chain_spec(), OpAlloyReceiptBuilder::default());
    executor.apply_pre_execution_changes().expect("the block starts");
    executor
}

/// The admin's `scheduleNextSequencerChange` call, carrying `proof`, run and committed in a
/// block at [`BLOCK_NUMBER`] over `state`. Answers whether the call succeeded.
fn schedule(
    state: &mut State<MemoryDatabase>,
    next: Address,
    activation: u64,
    proof: Bytes,
) -> bool {
    let mut executor = started_block_at(state, BLOCK_NUMBER);
    let input = ISequencerRegistry::scheduleNextSequencerChangeCall {
        newSequencer: next,
        activationBlock: U256::from(activation),
        newSequencerSignature: proof,
    }
    .abi_encode();
    let outcome =
        executor.run_transaction(&registry_call_from(common::ADMIN, input)).expect("it executes");
    let succeeded = outcome.result.is_success();
    executor.commit_transaction(outcome);
    succeeded
}

/// A state whose registry the first block deploys, with a funded admin.
fn state_with_funded_admin() -> State<MemoryDatabase> {
    let mut db = common::database();
    db.set_account_balance(common::ADMIN, U256::from(10_u64.pow(18)));
    State::builder().with_database(db).build()
}

/// A sequencer rotation end to end: the admin schedules it in one block with the new key's
/// EIP-712 proof of possession, through an ordinary transaction, and the block at its activation
/// block applies it before its transactions; the block before does not.
#[test]
fn test_a_rotation_with_a_valid_proof_activates_at_its_block() {
    let (next, secret) = next_sequencer_key();
    let activation = BLOCK_NUMBER + common::registry_config().min_rotation_delay;
    let mut state = state_with_funded_admin();

    let proof = rotation_proof(secret, next, U256::from(activation));
    assert!(schedule(&mut state, next, activation, proof), "the proof is accepted");
    assert_eq!(slot(&mut state, PENDING_SEQUENCER), word(next));
    assert_eq!(slot(&mut state, SEQUENCER_ACTIVATION_BLOCK), U256::from(activation));

    drop(started_block_at(&mut state, activation - 1));
    assert_eq!(slot(&mut state, CURRENT_SEQUENCER), word(common::SEQUENCER), "not before");

    drop(started_block_at(&mut state, activation));
    assert_eq!(slot(&mut state, CURRENT_SEQUENCER), word(next), "applied at its block");
    assert_eq!(slot(&mut state, PENDING_SEQUENCER), U256::ZERO);
}

/// A schedule without a proof of possession is refused by the contract: the transaction is
/// included, its call reverts, and nothing is pending.
#[test]
fn test_a_schedule_without_a_proof_reverts() {
    let (next, _) = next_sequencer_key();
    let activation = BLOCK_NUMBER + common::registry_config().min_rotation_delay;
    let mut state = state_with_funded_admin();

    assert!(!schedule(&mut state, next, activation, Bytes::new()), "the call reverts");
    assert_eq!(slot(&mut state, PENDING_SEQUENCER), U256::ZERO);
}
