//! The system contracts every block deploys, the check of the EIP-7997 factory Satin's activation
//! block makes, the resolution of the live system address that ends every block's pre-block
//! steps, and the factory round trip.
//!
//! Every block, after the EIP calls and, in the activation block alone, the factory check, deploys
//! the Oracle, the high-precision timestamp, `KeylessDeploy`, `MegaAccessControl` and
//! `MegaLimitControl`: a missing one is installed, one holding other code has its code replaced and
//! keeps its storage, one holding its code is read. Then come the registry's due-change read and
//! call, the registry's own deployment — seeded where the state holds none, migrated from 1.0.0,
//! refused over other code — and the resolution of the live system address, last. Each step
//! commits a state of its own, before the next runs; nothing is charged.

use alloy_eips::{eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS};
use alloy_evm::block::{BlockExecutionError, BlockExecutor, BlockValidationError};
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use mega_evm::{
    system::{
        flat_system_contract_specs,
        keyless::KEYLESS_DEPLOY_ADDRESS,
        storage_slots::{
            ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, INITIAL_FROM_BLOCK,
            INITIAL_SEQUENCER, INITIAL_SYSTEM_ADDRESS, MIN_ROTATION_DELAY, PENDING_SEQUENCER,
            PENDING_SYSTEM_ADDRESS,
        },
        SequencerRegistryRefusal, ACCESS_CONTROL_ADDRESS, CREATE2_FACTORY_ADDRESS,
        CREATE2_FACTORY_CODE_HASH, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, LIMIT_CONTROL_ADDRESS,
        LIMIT_CONTROL_CODE_HASH, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
        ORACLE_CONTRACT_CODE_HASH, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE_HASH,
    },
    test_utils::{rex6_predeploys, CommittedStates, ErrorInjectingDatabase, MemoryDatabase},
    MegaBlockExecutionCtx, MegaBlockExecutionError, MegaHardfork, MegaHardforkConfig,
    MegaHardforks,
};
use revm::{
    context::result::ExecutionResult, database::State, primitives::KECCAK_EMPTY, state::EvmState,
    Database,
};

use crate::common::{
    self, activation_ctx, activation_spec, after_activation_ctx, executor_with_env_and_spec,
    executor_with_spec, named_accounts, recovered, tx, unlimited_ctx, word,
    BOOTSTRAP_MIN_ROTATION_DELAY,
};

/// The states the pre-block steps of a block over `state` held to `ctx` on the chain `spec`
/// describes committed, in order, as a commit hook on `state` receives them, and the steps'
/// answer.
fn start_on(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
    spec: MegaHardforkConfig,
) -> (Vec<EvmState>, Result<(), BlockExecutionError>) {
    let committed = CommittedStates::install(state);
    let mut executor = executor_with_spec(state, ctx, spec);
    let started = executor.apply_pre_execution_changes();
    assert_eq!(executor.gas().execution, 0, "no pre-block step is counted");
    assert_eq!(executor.gas().state, 0);
    assert_eq!(executor.gas().history, 0);
    (committed.states(), started)
}

/// The states the pre-block steps of a block over `state` held to `ctx` committed, the block on
/// [`activation_spec`].
fn start(state: &mut State<MemoryDatabase>, ctx: MegaBlockExecutionCtx) -> Vec<EvmState> {
    let (states, started) = start_on(state, ctx, activation_spec());
    started.expect("the block starts");
    states
}

/// The account each pre-block state of a block names, in commit order: the two EIP calls'
/// targets, in the activation block the factory, the five system contracts, then the registry
/// three times — the read of its pending changes, found none due on these blocks, its deployment
/// and the resolution of the live system address. Nothing reads the L1 block contract before the
/// block's transactions.
fn expected_accounts(activation: bool) -> Vec<Address> {
    let mut expected = vec![HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS];
    if activation {
        expected.push(CREATE2_FACTORY_ADDRESS);
    }
    expected.extend(flat_system_contract_specs().map(|spec| spec.address));
    expected.extend([SEQUENCER_REGISTRY_ADDRESS; 3]);
    expected
}

/// The index of the state the deployment of the system contract at `address` committed in a block
/// that is not the activation block, in [`expected_accounts`] order.
fn deployment_step(address: Address) -> usize {
    let position = flat_system_contract_specs().iter().position(|spec| spec.address == address);
    2 + position.expect("one of the five system contracts")
}

/// The state the registry's deployment committed in a block that is not the activation block.
const REGISTRY_DEPLOYMENT: usize = 8;

/// The state the resolution of the live system address committed: the last.
const RESOLUTION: usize = 9;

/// Asserts `state` is the resolution of the live system address: the registry alone, read-only,
/// holding version 2.0.0, with its one slot naming `system_address`.
fn assert_resolves(state: &EvmState, system_address: Address) {
    assert_eq!(state.len(), 1);
    let registry = &state[&SEQUENCER_REGISTRY_ADDRESS];
    assert!(!registry.is_touched() && !registry.is_created(), "the resolution is read only");
    assert_eq!(registry.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
    assert_eq!(registry.storage.keys().copied().collect::<Vec<_>>(), [CURRENT_SYSTEM_ADDRESS]);
    assert_eq!(registry.storage[&CURRENT_SYSTEM_ADDRESS].present_value(), word(system_address));
}

/// The verdict a refused block's error carries.
fn verdict(error: &BlockExecutionError) -> MegaBlockExecutionError {
    let Some(BlockValidationError::Other(inner)) = error.as_validation() else {
        panic!("a verdict on the block: {error:?}");
    };
    inner.downcast_ref::<MegaBlockExecutionError>().expect("the engine's refusal").clone()
}

/// The internal error a block's start failed with.
fn internal(error: &BlockExecutionError) -> MegaBlockExecutionError {
    error
        .as_internal()
        .and_then(|error| error.downcast_other::<MegaBlockExecutionError>())
        .unwrap_or_else(|| panic!("an internal error of the engine: {error:?}"))
        .clone()
}

/// Over a state shaped as Rex6 leaves it, Satin's activation block and the block after it
/// deploy every system contract and find each holding Satin's code. Each pre-block step commits a
/// state of its own, and a commit hook receives them in order, read-only entries included: the two
/// EIP calls, which touch their contracts; in the activation block alone the factory, read only;
/// the five contracts, each a read-only entry as the chain holds it — no write, no created mark,
/// nonce 0; the registry's two pending slots, read only; the registry's deployment, a read-only
/// entry without a slot; and the resolution, the registry's system-address slot. No step changes a
/// contract.
///
/// Expected values: independent (the Rex6-shaped accounts the test puts in the state).
#[test]
fn test_every_block_deploys_the_system_contracts_a_rex6_chain_holds_as_read_only_entries() {
    let mut state = common::state();
    let held = rex6_predeploys(common::registry_roles());
    for (activation, ctx) in [(true, activation_ctx()), (false, after_activation_ctx())] {
        let states = start(&mut state, ctx);
        assert_eq!(named_accounts(&states), expected_accounts(activation), "{activation}");
        for (call, target) in states.iter().zip([HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS]) {
            assert!(call[&target].is_touched(), "the call to {target} touches it");
        }
        let offset = usize::from(activation);
        if activation {
            let factory = &states[2][&CREATE2_FACTORY_ADDRESS];
            assert!(!factory.is_touched() && !factory.is_created(), "the check is read only");
            assert_eq!(factory.info.code_hash, CREATE2_FACTORY_CODE_HASH);
        }
        for (address, info, _) in &held[..5] {
            let account = &states[deployment_step(*address) + offset][address];
            assert!(!account.is_touched() && !account.is_created(), "{address} is read only");
            assert!(account.storage.is_empty(), "{address}: no slot is written");
            assert_eq!((account.info.code_hash, account.info.nonce), (info.code_hash, info.nonce));
        }
        let pending = &states[7 + offset][&SEQUENCER_REGISTRY_ADDRESS];
        assert!(!pending.is_touched(), "the due-change read is read only");
        assert_eq!(
            pending.storage.keys().copied().collect::<std::collections::BTreeSet<_>>(),
            [PENDING_SYSTEM_ADDRESS, PENDING_SEQUENCER].into(),
        );
        let registry = &states[REGISTRY_DEPLOYMENT + offset][&SEQUENCER_REGISTRY_ADDRESS];
        assert!(!registry.is_touched() && !registry.is_created(), "version 2.0.0 is read");
        assert!(registry.storage.is_empty(), "the deployment reads no slot of it");
        assert_resolves(&states[RESOLUTION + offset], MEGA_SYSTEM_ADDRESS);
    }
    for (address, info, _) in held {
        let now = state.basic(address).unwrap().expect("still there");
        assert_eq!((now.code_hash, now.nonce), (info.code_hash, info.nonce), "{address}");
    }
}

/// A system contract the state does not hold is installed, in every block that finds it
/// missing: `KeylessDeploy`, `MegaAccessControl` and `MegaLimitControl`, which a chain that never
/// ran their forks lacks, each get an account holding their code, at nonce 0, marked created; the
/// next block reads them as the chain then holds them.
///
/// Expected values: independent (the code each contract ships).
#[test]
fn test_a_missing_system_contract_is_installed() {
    let missing = [KEYLESS_DEPLOY_ADDRESS, ACCESS_CONTROL_ADDRESS, LIMIT_CONTROL_ADDRESS];
    let mut db = common::database();
    for address in missing {
        db.cache.accounts.remove(&address);
    }
    let mut state = State::builder().with_database(db).build();
    let states = start(&mut state, after_activation_ctx());
    assert_eq!(named_accounts(&states), expected_accounts(false));
    for spec in
        flat_system_contract_specs().into_iter().filter(|spec| missing.contains(&spec.address))
    {
        let account = &states[deployment_step(spec.address)][&spec.address];
        assert!(account.is_touched() && account.is_created(), "{} is installed", spec.address);
        assert_eq!(account.info.code_hash, spec.code_hash);
        assert_eq!((account.info.nonce, account.info.balance), (0, U256::ZERO));
        let now = state.basic(spec.address).unwrap().expect("installed");
        assert_eq!(now.code_hash, spec.code_hash, "{} holds its code", spec.address);
    }

    let next = start(&mut state, after_activation_ctx());
    for address in missing {
        let account = &next[deployment_step(address)][&address];
        assert!(!account.is_touched() && !account.is_created(), "{address} is now only read");
    }
}

/// A system contract holding other code has its code replaced by Satin's and keeps its
/// nonce, its balance and its storage: the Oracle at 1.0.0, which an earlier fork left with a slot
/// written, becomes 2.0.0 with that slot, and `MegaLimitControl` holding foreign code gets its
/// own. The account is touched and not created, which would clear the storage.
///
/// Expected values: independent (the code hashes and the storage the test puts in the state).
#[test]
fn test_a_system_contract_holding_other_code_is_rewritten_keeping_its_storage() {
    use mega_system_contracts::oracle;
    let mut db = common::database();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle::V1_0_0_CODE);
    db.set_account_storage(ORACLE_CONTRACT_ADDRESS, U256::from(7), U256::from(9));
    db.set_account_balance(ORACLE_CONTRACT_ADDRESS, U256::from(5));
    db.set_account_code(LIMIT_CONTROL_ADDRESS, Bytes::from_static(&[0xfe]));
    let mut state = State::builder().with_database(db).build();
    let states = start(&mut state, after_activation_ctx());
    for (address, expected) in [
        (ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH),
        (LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH),
    ] {
        let account = &states[deployment_step(address)][&address];
        assert!(account.is_touched() && !account.is_created(), "{address}: rewritten, not created");
        assert_eq!(account.info.code_hash, expected, "{address}");
        assert!(account.storage.is_empty(), "{address}: no slot is written");
    }
    let oracle = state.basic(ORACLE_CONTRACT_ADDRESS).unwrap().expect("the Oracle");
    assert_eq!(oracle.code_hash, ORACLE_CONTRACT_CODE_HASH, "the Oracle runs 2.0.0");
    assert_eq!(oracle.balance, U256::from(5), "its balance is kept");
    assert_eq!(
        state.storage(ORACLE_CONTRACT_ADDRESS, U256::from(7)).unwrap(),
        U256::from(9),
        "its storage is kept"
    );
}

/// Where the state holds no registry, or an account without code at its address, the block
/// deploys version 2.0.0, seeded from the configuration's bootstrap values: the system address
/// both current and initial, the configured sequencer both current and initial, the configured
/// admin, the block's number as its first block and the configured minimum rotation delay. Where
/// no account stood the registry is created; an existing account is not, and keeps its nonce and
/// its balance. The resolution then names the system address the seed wrote, and the next block
/// finds the registry as the block left it.
///
/// Expected values: independent (the bootstrap values and the block number the test uses).
#[test]
fn test_a_registry_the_state_does_not_hold_is_seeded_from_the_bootstrap_values() {
    let without = |edit: &dyn Fn(&mut MemoryDatabase)| {
        let mut db = common::database();
        db.cache.accounts.remove(&SEQUENCER_REGISTRY_ADDRESS);
        edit(&mut db);
        db
    };
    let cases = [
        ("no account", without(&|_| {}), U256::ZERO),
        (
            "a funded account without code",
            without(&|db| db.set_account_balance(SEQUENCER_REGISTRY_ADDRESS, U256::from(3))),
            U256::from(3),
        ),
    ];
    for (case, db, balance) in cases {
        let mut state = State::builder().with_database(db).build();
        let states = start(&mut state, after_activation_ctx());
        assert_eq!(named_accounts(&states), expected_accounts(false), "{case}");
        let registry = &states[REGISTRY_DEPLOYMENT][&SEQUENCER_REGISTRY_ADDRESS];
        assert!(registry.is_touched(), "{case}: deployed");
        assert_eq!(registry.is_created(), case == "no account", "{case}: created where none stood");
        assert_eq!(registry.info.balance, balance, "{case}: the balance is kept");
        let seeded: std::collections::BTreeMap<_, _> =
            registry.storage.iter().map(|(slot, value)| (*slot, value.present_value())).collect();
        let expected = std::collections::BTreeMap::from([
            (CURRENT_SYSTEM_ADDRESS, word(MEGA_SYSTEM_ADDRESS)),
            (CURRENT_SEQUENCER, word(common::SEQUENCER)),
            (ADMIN, word(common::ADMIN)),
            (INITIAL_SYSTEM_ADDRESS, word(MEGA_SYSTEM_ADDRESS)),
            (INITIAL_SEQUENCER, word(common::SEQUENCER)),
            (INITIAL_FROM_BLOCK, U256::from(common::BLOCK_NUMBER)),
            (MIN_ROTATION_DELAY, U256::from(BOOTSTRAP_MIN_ROTATION_DELAY)),
        ]);
        assert_eq!(seeded, expected, "{case}");
        assert_resolves(&states[RESOLUTION], MEGA_SYSTEM_ADDRESS);

        let next = start(&mut state, after_activation_ctx());
        let registry = &next[REGISTRY_DEPLOYMENT][&SEQUENCER_REGISTRY_ADDRESS];
        assert!(!registry.is_touched(), "{case}: the next block reads what the block deployed");
    }
}

/// Seeding the registry over an account without code keeps the account's storage, as every system
/// contract's deployment and the migration from 1.0.0 keep it: committed through revm's `State`, as
/// a node commits, a slot the seed does not write reads what it held in the seeding block and in
/// the next, and the block's bundle neither destroys the account nor claims to know its storage,
/// so what a node persists holds the slot too. A seeded slot that held a value has it as its
/// original and the seed as its present value; the nonce and the balance are kept.
///
/// Expected values: independent (the slots, the nonce and the balance the test puts in the state,
/// and the bootstrap values).
#[test]
fn test_seeding_the_registry_over_an_account_without_code_keeps_its_storage() {
    use revm::database::states::bundle_state::BundleRetention;
    const KEPT: U256 = U256::from_limbs([42, 0, 0, 0]);
    let mut db = common::database();
    db.cache.accounts.remove(&SEQUENCER_REGISTRY_ADDRESS);
    db.set_account_nonce(SEQUENCER_REGISTRY_ADDRESS, 2);
    db.set_account_balance(SEQUENCER_REGISTRY_ADDRESS, U256::from(3));
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, KEPT, U256::ONE);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, ADMIN, U256::from(77));
    // With its transitions kept for the bundle, as a node's state keeps them.
    let mut state = State::builder().with_database(db).with_bundle_update().build();

    let states = start(&mut state, after_activation_ctx());
    let registry = &states[REGISTRY_DEPLOYMENT][&SEQUENCER_REGISTRY_ADDRESS];
    assert!(registry.is_touched() && !registry.is_created(), "installed in the account, kept");
    let admin = &registry.storage[&ADMIN];
    assert_eq!(
        (admin.original_value(), admin.present_value()),
        (U256::from(77), word(common::ADMIN)),
        "a seeded slot is written over what it held"
    );
    assert!(!registry.storage.contains_key(&KEPT), "a slot the seed does not write is not touched");
    assert_eq!(state.storage(SEQUENCER_REGISTRY_ADDRESS, KEPT).unwrap(), U256::ONE, "in the block");
    assert_eq!(state.storage(SEQUENCER_REGISTRY_ADDRESS, ADMIN).unwrap(), word(common::ADMIN));
    let info = state.basic(SEQUENCER_REGISTRY_ADDRESS).unwrap().expect("the registry");
    assert_eq!((info.nonce, info.balance), (2, U256::from(3)), "the nonce and balance are kept");
    assert_eq!(info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
    assert_resolves(&states[RESOLUTION], MEGA_SYSTEM_ADDRESS);

    state.merge_transitions(BundleRetention::Reverts);
    let persisted = state.bundle_state.account(&SEQUENCER_REGISTRY_ADDRESS).expect("changed");
    assert!(!persisted.was_destroyed(), "the bundle does not destroy the account");
    assert_eq!(persisted.storage_slot(KEPT), None, "nor claims its unwritten slots are empty");
    assert_eq!(persisted.storage_slot(ADMIN), Some(word(common::ADMIN)));

    let next = start(&mut state, after_activation_ctx());
    assert!(!next[REGISTRY_DEPLOYMENT][&SEQUENCER_REGISTRY_ADDRESS].is_touched(), "read-only");
    let kept = state.storage(SEQUENCER_REGISTRY_ADDRESS, KEPT).unwrap();
    assert_eq!(kept, U256::ONE, "in the next block");
}

/// A registry at version 1.0.0 is migrated in place: its code becomes 2.0.0 and its slot 13
/// the configured minimum rotation delay, the one slot written; its roles and every other slot are
/// what they were, so the resolution names the system address it already held.
///
/// Expected values: independent (the roles and the delay the test puts in the state and the
/// configuration).
#[test]
fn test_a_registry_at_1_0_0_is_migrated_writing_slot_13_alone() {
    use mega_system_contracts::sequencer_registry::V1_0_0_CODE;
    let chain_address = alloy_primitives::address!("0x5000000000000000000000000000000000000005");
    let mut db = common::database();
    db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, V1_0_0_CODE);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, MIN_ROTATION_DELAY, U256::ZERO);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS, word(chain_address));
    let mut state = State::builder().with_database(db).build();
    let states = start(&mut state, after_activation_ctx());
    let registry = &states[REGISTRY_DEPLOYMENT][&SEQUENCER_REGISTRY_ADDRESS];
    assert!(registry.is_touched() && !registry.is_created(), "migrated, not created");
    assert_eq!(registry.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
    assert_eq!(registry.storage.keys().copied().collect::<Vec<_>>(), [MIN_ROTATION_DELAY]);
    let delay = &registry.storage[&MIN_ROTATION_DELAY];
    assert_eq!(
        (delay.original_value(), delay.present_value()),
        (U256::ZERO, U256::from(BOOTSTRAP_MIN_ROTATION_DELAY))
    );
    assert_resolves(&states[RESOLUTION], chain_address);
    let slot = |state: &mut State<MemoryDatabase>, slot| {
        state.storage(SEQUENCER_REGISTRY_ADDRESS, slot).unwrap()
    };
    assert_eq!(slot(&mut state, CURRENT_SEQUENCER), word(common::SEQUENCER), "the roles stay");
    assert_eq!(slot(&mut state, ADMIN), word(common::ADMIN));
    assert_eq!(slot(&mut state, MIN_ROTATION_DELAY), U256::from(BOOTSTRAP_MIN_ROTATION_DELAY));
}

/// A registry holding any other code than version 2.0.0, version 1.0.0 or none makes the
/// block invalid, with an error naming the code hash: its storage is never overwritten. The
/// refusal comes at the registry's deployment, after its due-change read was committed.
///
/// Expected values: independent (the code hash the state holds).
#[test]
fn test_a_registry_holding_other_code_refuses_the_block() {
    let mut db = common::database();
    db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, Bytes::from_static(&[0xfe]));
    let mut state = State::builder().with_database(db).build();
    let (states, started) = start_on(&mut state, after_activation_ctx(), activation_spec());
    let error = started.expect_err("the block is refused");
    let found = keccak256([0xfe]);
    assert_eq!(
        verdict(&error),
        MegaBlockExecutionError::SequencerRegistry(SequencerRegistryRefusal::UnmigratableCode {
            found
        })
    );
    assert!(error.to_string().contains(&found.to_string()), "{error}");
    assert_eq!(named_accounts(&states), expected_accounts(false)[..REGISTRY_DEPLOYMENT]);
    assert_eq!(
        state.basic(SEQUENCER_REGISTRY_ADDRESS).unwrap().unwrap().code_hash,
        found,
        "nothing was written"
    );
}

/// A registry that names no system address — version 2.0.0 whose `_currentSystemAddress` is
/// zero — makes the block invalid at the resolution, the last pre-block step, after the
/// registry's deployment read it.
///
/// Expected values: independent (the slot the test clears).
#[test]
fn test_a_registry_naming_no_system_address_refuses_the_block() {
    let mut db = common::database();
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS, U256::ZERO);
    let mut state = State::builder().with_database(db).build();
    let (states, started) = start_on(&mut state, after_activation_ctx(), activation_spec());
    let error = started.expect_err("the block is refused");
    assert_eq!(
        verdict(&error),
        MegaBlockExecutionError::SequencerRegistry(SequencerRegistryRefusal::NoSystemAddress)
    );
    assert_eq!(named_accounts(&states), expected_accounts(false)[..RESOLUTION]);
}

/// A block that must seed the registry, or migrate it from 1.0.0, on a schedule that
/// carries no bootstrap values — one that skipped `validate_schedule`, which refuses it at load —
/// cannot be executed: an internal error of the node, not a verdict on the block. A block whose
/// registry holds version 2.0.0 needs none, and starts.
///
/// Expected values: independent (the registry each state holds).
#[test]
fn test_a_registry_to_seed_or_migrate_without_bootstrap_values_is_an_internal_error() {
    use mega_system_contracts::sequencer_registry::V1_0_0_CODE;
    let without_bootstrap = MegaHardforkConfig::default()
        .with(MegaHardfork::Satin, alloy_hardforks::ForkCondition::Timestamp(0));
    assert!(without_bootstrap.validate_schedule().is_err(), "refused when it is loaded");
    let absent = {
        let mut db = common::database();
        db.cache.accounts.remove(&SEQUENCER_REGISTRY_ADDRESS);
        db
    };
    let migrating = common::database().account_code(SEQUENCER_REGISTRY_ADDRESS, V1_0_0_CODE);
    for (case, db) in [("no registry", absent), ("version 1.0.0", migrating)] {
        let mut state = State::builder().with_database(db).build();
        let (_, started) = start_on(&mut state, unlimited_ctx(), without_bootstrap.clone());
        let error = started.expect_err("the registry cannot be deployed");
        assert_eq!(internal(&error), MegaBlockExecutionError::UnusableRegistryBootstrap, "{case}");
        assert_eq!(
            internal(&error).to_string(),
            "the SequencerRegistry must be seeded or migrated, and the chain configuration carries no usable bootstrap values for it",
            "{case}"
        );
    }
    let (_, started) = start_on(&mut common::state(), unlimited_ctx(), without_bootstrap);
    started.expect("version 2.0.0 needs no bootstrap values");
}

/// The activation block is invalid when the EIP-7997 factory's address does not hold the
/// factory's code, with one error naming the address, the expected hash and the one held: no
/// account, an account without code at a used nonce, and other code. The refusal comes after the
/// two EIP calls and before anything is deployed. The block after the activation block does not
/// read the factory, and starts over the same state.
///
/// Expected values: independent (the code hash each state holds).
#[test]
fn test_an_activation_block_without_the_factory_is_refused() {
    let with = |edit: &dyn Fn(&mut MemoryDatabase)| {
        let mut db = common::database();
        edit(&mut db);
        db
    };
    let cases = [
        (
            "no account",
            with(&|db| {
                db.cache.accounts.remove(&CREATE2_FACTORY_ADDRESS);
            }),
            KECCAK_EMPTY,
        ),
        (
            "an account without code at a used nonce",
            with(&|db| db.set_account_code(CREATE2_FACTORY_ADDRESS, Bytes::new())),
            KECCAK_EMPTY,
        ),
        (
            "other code",
            with(&|db| db.set_account_code(CREATE2_FACTORY_ADDRESS, Bytes::from_static(&[0xfe]))),
            keccak256([0xfe]),
        ),
    ];
    for (name, db, found) in cases {
        let mut state = State::builder().with_database(db.clone()).build();
        let (states, started) = start_on(&mut state, activation_ctx(), activation_spec());
        let error = started.expect_err("the activation block is refused");
        let refusal = verdict(&error);
        assert_eq!(
            refusal,
            MegaBlockExecutionError::UnexpectedSystemContractCode {
                address: CREATE2_FACTORY_ADDRESS,
                expected: CREATE2_FACTORY_CODE_HASH,
                found,
            },
            "{name}"
        );
        let message = refusal.to_string();
        assert!(message.contains(&CREATE2_FACTORY_ADDRESS.to_string()), "{name}: {message}");
        assert!(message.contains("when Satin activates"), "{name}: {message}");
        assert_eq!(named_accounts(&states), [HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS]);

        let mut state = State::builder().with_database(db).build();
        start(&mut state, after_activation_ctx());
    }
}

/// No block but Satin's activation block reads the factory: over a database that fails
/// every load of the factory's account, an ordinary block starts, where the activation block fails
/// on the load as an internal error, the database's and no verdict on the block.
///
/// Expected values: independent (the injected failure).
#[test]
fn test_no_other_block_reads_the_factory() {
    let start_on = |ctx: MegaBlockExecutionCtx| {
        let mut db = ErrorInjectingDatabase::new(common::database());
        db.fail_on_account = Some(CREATE2_FACTORY_ADDRESS);
        let mut state = State::builder().with_database(db).build();
        let evm = alloy_evm::EvmFactory::create_evm(
            &mega_evm::MegaEvmFactory::new(),
            &mut state,
            common::evm_env(),
        );
        mega_evm::MegaBlockExecutor::new(
            evm,
            ctx,
            activation_spec(),
            alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder::default(),
        )
        .apply_pre_execution_changes()
    };
    start_on(after_activation_ctx()).expect("an ordinary block reads no factory");
    let error = start_on(activation_ctx()).expect_err("the activation block reads the factory");
    assert!(matches!(error, BlockExecutionError::Internal(_)), "{error:?}");
    assert!(error.to_string().contains("injected basic() error"), "{error}");
}

/// Satin may be active from genesis, its `satinTime` at or before the genesis block's
/// timestamp. The genesis block, which has no parent, is no activation block, whatever that time:
/// it makes no EIP-2935 or EIP-4788 call and checks no factory, and on a state that holds none of
/// the system contracts it installs the five and seeds the registry, at block number zero, from
/// the bootstrap values. A non-zero parent beacon block root refuses it, as EIP-4788 defines. The
/// block after it makes both calls and reads what the genesis block deployed.
///
/// Expected values: independent (the schedule and the empty state the test uses).
#[test]
fn test_satin_active_from_genesis_deploys_every_system_contract_in_the_genesis_block() {
    let genesis_time = common::BLOCK_TIMESTAMP;
    let spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Satin, alloy_hardforks::ForkCondition::Timestamp(genesis_time - 100))
        .with_sequencer_registry_bootstrap(common::bootstrap());
    let env_at = |number: u64, timestamp: u64| {
        let mut env = common::evm_env();
        env.block_env.number = U256::from(number);
        env.block_env.timestamp = U256::from(timestamp);
        env
    };
    let ctx = |root: B256, parent_timestamp| {
        MegaBlockExecutionCtx::new(
            B256::ZERO,
            Some(root),
            Bytes::new(),
            mega_evm::BlockLimits::no_limits(),
            parent_timestamp,
        )
    };

    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let committed = CommittedStates::install(&mut state);
    let mut executor = executor_with_env_and_spec(
        &mut state,
        ctx(B256::ZERO, None),
        env_at(0, genesis_time),
        spec.clone(),
    );
    assert_eq!(executor.is_satin_activation_block(), Ok(false));
    assert_eq!(executor.admits_only_deposits(), Ok(false));
    executor.apply_pre_execution_changes().expect("the genesis block starts");
    drop(executor);
    let states = committed.states();
    let mut expected: Vec<Address> = flat_system_contract_specs().map(|spec| spec.address).into();
    expected.extend([SEQUENCER_REGISTRY_ADDRESS; 3]);
    assert_eq!(named_accounts(&states), expected, "no EIP call, no factory check");
    for (i, spec) in flat_system_contract_specs().into_iter().enumerate() {
        let account = &states[i][&spec.address];
        assert!(account.is_created(), "{} is installed", spec.address);
        assert_eq!(account.info.code_hash, spec.code_hash);
    }
    let registry = &states[6][&SEQUENCER_REGISTRY_ADDRESS];
    assert!(registry.is_created(), "the registry is seeded");
    assert_eq!(registry.storage[&INITIAL_FROM_BLOCK].present_value(), U256::ZERO);
    assert_resolves(&states[7], MEGA_SYSTEM_ADDRESS);
    for address in [HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, SEQUENCER_REGISTRY_ADDRESS] {
        assert!(state.basic(address).unwrap().is_some(), "{address} is in the state");
    }

    let mut fresh = State::builder().with_database(MemoryDatabase::default()).build();
    let error = executor_with_env_and_spec(
        &mut fresh,
        ctx(B256::repeat_byte(1), None),
        env_at(0, genesis_time),
        spec.clone(),
    )
    .apply_pre_execution_changes()
    .expect_err("a genesis block's parent beacon block root is zero");
    assert!(
        matches!(
            error,
            BlockExecutionError::Validation(
                BlockValidationError::CancunGenesisParentBeaconBlockRootNotZero { .. }
            )
        ),
        "{error:?}"
    );

    let committed = CommittedStates::install(&mut state);
    let mut next = executor_with_env_and_spec(
        &mut state,
        ctx(B256::ZERO, Some(genesis_time)),
        env_at(1, genesis_time + 1),
        spec,
    );
    assert_eq!(next.is_satin_activation_block(), Ok(false), "Satin was active at the parent");
    next.apply_pre_execution_changes().expect("the block after genesis starts");
    drop(next);
    let states = committed.states();
    assert_eq!(named_accounts(&states)[..2], [HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS]);
    for state in &states[2..] {
        let (_, account) = state.iter().next().expect("one account");
        assert!(
            !account.is_created() && !account.is_touched(),
            "read as the genesis block left it"
        );
    }
}

/// Calling the factory with salt || initcode returns the CREATE2 address as 20 bytes.
#[test]
fn test_a_create2_round_trip_through_the_factory() {
    let mut state = common::state();
    let mut executor = common::executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let salt = B256::repeat_byte(0x11);
    // Constructor: RETURN empty runtime.
    let init = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
    let mut input = salt.to_vec();
    input.extend_from_slice(&init);
    let expected = CREATE2_FACTORY_ADDRESS.create2_from_code(salt, init.as_ref());
    // 1,000,000 of regular gas on top of the account the creation adds, the history of the body,
    // and 50 times the history of the creation's two write records, at the byte prices in effect:
    // the factory forwards all but 2 per cent of its gas to the creation and pays the records from
    // the fiftieth it keeps.
    let gas_limit = 1_000_000 +
        common::new_account_state_gas() +
        common::body_history(input.len() as u64) +
        50 * mega_evm::write_record_history_gas(2).expect("two records have a price");

    let outcome = executor
        .execute_transaction_without_commit(&recovered(tx(
            0,
            CREATE2_FACTORY_ADDRESS,
            Bytes::from(input),
            gas_limit,
        )))
        .expect("the factory call runs");
    match &outcome.result {
        ExecutionResult::Success { output, .. } => {
            let data = output.data();
            assert_eq!(data.len(), 20, "the factory returns 20 bytes");
            assert_eq!(Address::from_slice(data), expected);
        }
        other => panic!("the factory call should succeed, got {other:?}"),
    }
}

/// A migration's storage read failure remains a database error, with no verdict on the block.
#[test]
fn test_a_registry_migration_storage_failure_is_an_internal_database_error() {
    use mega_system_contracts::sequencer_registry::V1_0_0_CODE;
    let mut db = ErrorInjectingDatabase::new(
        common::database().account_code(SEQUENCER_REGISTRY_ADDRESS, V1_0_0_CODE),
    );
    db.fail_on_storage = Some((SEQUENCER_REGISTRY_ADDRESS, MIN_ROTATION_DELAY));
    let mut state = State::builder().with_database(db).build();
    let evm = alloy_evm::EvmFactory::create_evm(
        &mega_evm::MegaEvmFactory::new(),
        &mut state,
        common::evm_env(),
    );
    let error = mega_evm::MegaBlockExecutor::new(
        evm,
        after_activation_ctx(),
        activation_spec(),
        alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder::default(),
    )
    .apply_pre_execution_changes()
    .expect_err("the migration cannot read the original delay");

    assert!(matches!(error, BlockExecutionError::Internal(_)), "{error:?}");
    assert!(error.to_string().contains("injected storage() error"), "{error}");
    assert_eq!(
        state.basic(SEQUENCER_REGISTRY_ADDRESS).unwrap().unwrap().code_hash,
        keccak256(V1_0_0_CODE),
        "a failed migration commits no new code"
    );
}
