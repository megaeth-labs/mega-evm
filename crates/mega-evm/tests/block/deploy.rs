//! System contract deployment at the start of a block, and the EIP-7997 factory round trip.

use alloy_evm::block::BlockExecutor;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE_HASH},
        storage_slots::{
            ADMIN as ADMIN_SLOT, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, INITIAL_FROM_BLOCK,
            INITIAL_SEQUENCER, INITIAL_SYSTEM_ADDRESS, MIN_ROTATION_DELAY,
        },
        IOracle, SequencerRegistryConfig, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE_HASH,
        CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_CODE, CREATE2_FACTORY_CODE_HASH,
        HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH,
        LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH, MEGA_SYSTEM_ADDRESS,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE_HASH, SYSTEM_CONTRACT_DEPLOY_COUNT,
    },
    test_utils::MemoryDatabase,
    MegaHardfork, MegaHardforkConfig, MegaHardforks, PreBlockStateSource,
};
use revm::{
    context::{result::ExecutionResult, ContextTr},
    database::{states::bundle_state::BundleRetention, OriginalValuesKnown, State},
    state::Account,
    Database,
};

use crate::common::{
    self, executor, executor_with_spec, pre_block_states, record_pre_block, recovered,
    registry_config, tx, unlimited_ctx, ADMIN, BLOCK_NUMBER, SEQUENCER,
};

fn expected_hashes() -> [(Address, B256); SYSTEM_CONTRACT_DEPLOY_COUNT] {
    [
        (ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH),
        (HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH),
        (KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE_HASH),
        (ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE_HASH),
        (LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH),
        (SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE_HASH),
        (CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_CODE_HASH),
    ]
}

fn assert_deployed(state: &mut State<MemoryDatabase>) {
    for (address, hash) in expected_hashes() {
        let info = state.basic(address).expect("readable").expect("deployed");
        assert_eq!(info.code_hash, hash, "{address} has the wrong code");
        assert_eq!(info.nonce, 1, "{address} is a created contract");
        assert!(info.code.is_some(), "{address} carries its bytecode");
    }
}

fn deploy_sources() -> [PreBlockStateSource; SYSTEM_CONTRACT_DEPLOY_COUNT] {
    expected_hashes().map(|(address, _)| PreBlockStateSource::SystemContract(address))
}

/// The two EIP calls, the seven deploys, the read of the registry's pending changes, which
/// finds none due on these blocks, then the read of the L1 block info. The live system address
/// is not read before the block: a system-address transaction reads it itself.
fn assert_pre_block_order(outcomes: &[(PreBlockStateSource, revm::state::EvmState)]) {
    assert_eq!(outcomes.len(), 2 + SYSTEM_CONTRACT_DEPLOY_COUNT + 2);
    assert_eq!(outcomes[0].0, PreBlockStateSource::Eip2935);
    assert_eq!(outcomes[1].0, PreBlockStateSource::Eip4788);
    for (i, source) in deploy_sources().iter().enumerate() {
        assert_eq!(outcomes[2 + i].0, *source);
    }
    assert_eq!(outcomes[2 + SYSTEM_CONTRACT_DEPLOY_COUNT].0, PreBlockStateSource::PendingChanges);
    assert_eq!(outcomes[3 + SYSTEM_CONTRACT_DEPLOY_COUNT].0, PreBlockStateSource::L1BlockInfo);
}

/// The registry's seeded slots: the roles `config` names, and `_initialFromBlock` the number of
/// the block that created the registry, `created_in`.
fn assert_registry_account_seed(
    account: &Account,
    config: &SequencerRegistryConfig,
    created_in: u64,
) {
    let addr_val = |address: Address| U256::from_be_bytes(address.into_word().0);
    let slot = |key: U256| account.storage.get(&key).expect("seeded").present_value;
    assert_eq!(slot(CURRENT_SYSTEM_ADDRESS), addr_val(config.initial_system_address));
    assert_eq!(slot(CURRENT_SEQUENCER), addr_val(config.initial_sequencer));
    assert_eq!(slot(ADMIN_SLOT), addr_val(config.initial_admin));
    assert_eq!(slot(INITIAL_SYSTEM_ADDRESS), addr_val(config.initial_system_address));
    assert_eq!(slot(INITIAL_SEQUENCER), addr_val(config.initial_sequencer));
    assert_eq!(slot(INITIAL_FROM_BLOCK), U256::from(created_in));
    assert_eq!(slot(MIN_ROTATION_DELAY), U256::from(config.min_rotation_delay));
}

/// [`assert_registry_account_seed`], on the state a block committed.
fn assert_registry_seed(
    state: &mut State<MemoryDatabase>,
    config: &SequencerRegistryConfig,
    created_in: u64,
) {
    let addr_val = |address: Address| U256::from_be_bytes(address.into_word().0);
    assert_eq!(
        state.storage(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS).unwrap(),
        addr_val(config.initial_system_address)
    );
    assert_eq!(
        state.storage(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SEQUENCER).unwrap(),
        addr_val(config.initial_sequencer)
    );
    assert_eq!(
        state.storage(SEQUENCER_REGISTRY_ADDRESS, ADMIN_SLOT).unwrap(),
        addr_val(config.initial_admin)
    );
    assert_eq!(
        state.storage(SEQUENCER_REGISTRY_ADDRESS, INITIAL_SYSTEM_ADDRESS).unwrap(),
        addr_val(config.initial_system_address)
    );
    assert_eq!(
        state.storage(SEQUENCER_REGISTRY_ADDRESS, INITIAL_SEQUENCER).unwrap(),
        addr_val(config.initial_sequencer)
    );
    assert_eq!(
        state.storage(SEQUENCER_REGISTRY_ADDRESS, INITIAL_FROM_BLOCK).unwrap(),
        U256::from(created_in)
    );
    assert_eq!(
        state.storage(SEQUENCER_REGISTRY_ADDRESS, MIN_ROTATION_DELAY).unwrap(),
        U256::from(config.min_rotation_delay)
    );
}

/// The first block deploys all seven predeploys with the pinned hashes, nonce 1, and the
/// registry's seeded slots. The executor's own observer sees the two EIP calls, the seven deploy
/// states and the two reads of the registry, in that order.
#[test]
fn test_the_first_block_deploys_every_system_contract() {
    let mut state = common::state();
    let outcomes = {
        let mut executor = executor(&mut state, unlimited_ctx());
        let log = record_pre_block(&mut executor);
        executor.apply_pre_execution_changes().expect("the block starts");
        assert_eq!(executor.gas().execution, 0);
        assert_eq!(executor.gas().state, 0);
        assert_eq!(executor.gas().history, 0);
        pre_block_states(&log)
    };
    assert_pre_block_order(&outcomes);
    let config = registry_config();
    for (i, (address, hash)) in expected_hashes().into_iter().enumerate() {
        let account = outcomes[2 + i]
            .1
            .get(&address)
            .unwrap_or_else(|| panic!("{address} is in the deploy state"));
        assert!(account.is_touched(), "{address} is touched");
        assert!(account.is_created(), "{address} is created");
        assert_eq!(account.info.code_hash, hash, "{address} has the wrong code");
        assert_eq!(account.info.nonce, 1, "{address} is a created contract");
        if address == SEQUENCER_REGISTRY_ADDRESS {
            assert_registry_account_seed(account, &config, BLOCK_NUMBER);
        } else {
            assert!(account.storage.is_empty(), "{address} has no seeded storage");
        }
    }
    assert_deployed(&mut state);
    assert_registry_seed(&mut state, &config, BLOCK_NUMBER);
    assert_eq!(config.initial_system_address, MEGA_SYSTEM_ADDRESS);
    assert_eq!(config.initial_sequencer, SEQUENCER);
    assert_eq!(config.initial_admin, ADMIN);
}

/// A second block changes nothing in the seven deploys: the executor's own observer reports
/// exactly seven read-only account entries, the two EIP calls still precede them and the two
/// reads of the registry follow them.
#[test]
fn test_the_second_block_is_seven_read_only_entries() {
    let mut state = common::state();
    {
        let mut executor = executor(&mut state, unlimited_ctx());
        executor.apply_pre_execution_changes().expect("the first block deploys");
    }

    let outcomes = {
        let mut executor = executor(&mut state, unlimited_ctx());
        let log = record_pre_block(&mut executor);
        executor.apply_pre_execution_changes().expect("the second block is a no-op");
        assert_eq!(executor.gas().execution, 0);
        assert_eq!(executor.gas().state, 0);
        assert_eq!(executor.gas().history, 0);
        pre_block_states(&log)
    };
    assert_pre_block_order(&outcomes);
    for (i, (address, hash)) in expected_hashes().into_iter().enumerate() {
        let state = &outcomes[2 + i].1;
        assert_eq!(state.len(), 1, "{address} is the only account in its witness");
        let account = state.get(&address).unwrap();
        assert!(!account.is_touched(), "{address} is not touched");
        assert!(!account.is_created(), "{address} is not created");
        assert!(account.storage.is_empty(), "{address} is not re-seeded");
        assert_eq!(account.info.code_hash, hash);
    }
    assert_deployed(&mut state);
    assert_registry_seed(&mut state, &registry_config(), BLOCK_NUMBER);
}

/// The registry's `_initialFromBlock` is the number of the block that creates it, whatever that
/// number is, as the legacy engine seeded it: its historical lookups are valid from there, and
/// below it they revert. A later block finds the registry and keeps the seed.
#[test]
fn test_the_registry_is_seeded_with_the_number_of_the_block_that_creates_it() {
    let env_at = |number: u64| {
        let mut env = common::evm_env();
        env.block_env.number = U256::from(number);
        env
    };
    let created_in = BLOCK_NUMBER + 7;
    let mut state = common::state();
    {
        let mut executor =
            common::executor_with_env(&mut state, unlimited_ctx(), env_at(created_in));
        executor.apply_pre_execution_changes().expect("the block deploys the registry");
    }
    assert_registry_seed(&mut state, &registry_config(), created_in);

    {
        let mut executor =
            common::executor_with_env(&mut state, unlimited_ctx(), env_at(created_in + 1));
        executor.apply_pre_execution_changes().expect("a later block finds it deployed");
    }
    assert_registry_seed(&mut state, &registry_config(), created_in);
}

/// [S18.12] [S18] An account a genesis allocation gave storage but no code, at a predeploy
/// address, is deployed over: it gets the code and nonce 1, and the slot it held is neither
/// written nor cleared. For the rest of the block the slot reads as zero, to the block's state and
/// to a transaction that reads it through the contract, whether or not the state's cache already
/// held the slot when the block started; the state the block returns carries no change to the
/// slot and no wipe of the account's storage, so the state it commits still holds the old value.
///
/// Expected values: independent (zero, no entry, no wipe).
#[test]
fn test_a_deploy_over_storage_neither_writes_nor_clears_it() {
    const SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);
    const HELD: U256 = U256::from_limbs([7, 0, 0, 0]);
    for cached in [false, true] {
        let mut db = common::database();
        db.set_account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, HELD);
        db.set_account_balance(ORACLE_CONTRACT_ADDRESS, U256::from(9));
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        if cached {
            // A state cache an earlier read filled, as a node's cache can be.
            assert_eq!(state.storage(ORACLE_CONTRACT_ADDRESS, SLOT).unwrap(), HELD);
        }

        {
            let mut executor = executor(&mut state, unlimited_ctx());
            let log = record_pre_block(&mut executor);
            executor.apply_pre_execution_changes().expect("the block starts");
            let deploy = pre_block_states(&log)
                .into_iter()
                .find(|(source, _)| {
                    *source == PreBlockStateSource::SystemContract(ORACLE_CONTRACT_ADDRESS)
                })
                .expect("the Oracle's deploy")
                .1;
            let account = &deploy[&ORACLE_CONTRACT_ADDRESS];
            assert!(account.is_created(), "cached {cached}: a fresh deploy");
            assert_eq!(account.info.code_hash, ORACLE_CONTRACT_CODE_HASH);
            assert_eq!(account.info.nonce, 1);
            assert_eq!(account.info.balance, U256::from(9), "the balance is kept");
            assert!(account.storage.is_empty(), "the deploy writes no slot of the Oracle");

            let read = executor.evm_mut().ctx_mut().db_mut().storage(ORACLE_CONTRACT_ADDRESS, SLOT);
            assert_eq!(read.unwrap(), U256::ZERO, "cached {cached}: the block's state reads zero");
            let input = IOracle::getSlotCall { slot: SLOT }.abi_encode();
            let gas_limit = 1_000_000 + common::body_history(input.len() as u64);
            let outcome = executor
                .execute_transaction_without_commit(&recovered(tx(
                    0,
                    ORACLE_CONTRACT_ADDRESS,
                    input.into(),
                    gas_limit,
                )))
                .expect("the read runs");
            let ExecutionResult::Success { output, .. } = &outcome.result else {
                panic!("the read succeeds: {:?}", outcome.result);
            };
            assert_eq!(
                output.data().as_ref(),
                B256::ZERO.as_slice(),
                "cached {cached}: a transaction reads zero"
            );
            executor.finish_with_counters().expect("the block finishes");
        }

        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        let oracle = &bundle.state[&ORACLE_CONTRACT_ADDRESS];
        assert!(!oracle.was_destroyed(), "cached {cached}: the storage is not wiped");
        assert!(!oracle.storage.contains_key(&SLOT), "cached {cached}: the slot is not written");
        let changes = bundle.to_plain_state(OriginalValuesKnown::Yes);
        let oracle_info = changes
            .accounts
            .iter()
            .find(|(address, _)| *address == ORACLE_CONTRACT_ADDRESS)
            .and_then(|(_, info)| info.clone())
            .expect("the deploy changes the account");
        assert_eq!((oracle_info.code_hash, oracle_info.nonce), (ORACLE_CONTRACT_CODE_HASH, 1));
        for change in changes.storage.iter().filter(|c| c.address == ORACLE_CONTRACT_ADDRESS) {
            assert!(!change.wipe_storage, "cached {cached}: the committed state wipes nothing");
            assert!(change.storage.iter().all(|(slot, _)| *slot != SLOT), "nor writes the slot");
        }
    }
}

/// A factory that already holds the right code is left as it is, including a balance it already
/// had.
#[test]
fn test_a_pre_existing_factory_with_the_right_code_is_untouched() {
    let mut db = common::database();
    db.set_account_code(CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_CODE);
    db.set_account_nonce(CREATE2_FACTORY_ADDRESS, 1);
    db.set_account_balance(CREATE2_FACTORY_ADDRESS, U256::from(42));
    let mut state = State::builder().with_database(db).build();

    {
        let mut executor = executor(&mut state, unlimited_ctx());
        executor.apply_pre_execution_changes().expect("the block starts");
    }

    let info = state.basic(CREATE2_FACTORY_ADDRESS).unwrap().unwrap();
    assert_eq!(info.code_hash, CREATE2_FACTORY_CODE_HASH);
    assert_eq!(info.nonce, 1);
    assert_eq!(info.balance, U256::from(42), "a matching factory keeps its balance");
}

/// A factory that already holds the right code but nonce 0 fails the block: EIP-7997 requires a
/// nonzero nonce, and the matching-code path will not rewrite it.
#[test]
fn test_a_factory_with_matching_code_and_nonce_zero_fails_the_block() {
    let mut db = common::database();
    db.set_account_code(CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_CODE);
    db.set_account_nonce(CREATE2_FACTORY_ADDRESS, 0);
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(&mut state, unlimited_ctx());
    let err = executor
        .apply_pre_execution_changes()
        .expect_err("a zero-nonce factory must fail the block");
    let message = err.to_string();
    assert!(message.contains("nonce 0"), "unexpected error: {message}");
    assert!(message.contains(&CREATE2_FACTORY_ADDRESS.to_string()));
}

/// An empty-code account with a used nonce at a system address fails the block rather than
/// being reset to a fresh deploy.
#[test]
fn test_a_used_empty_account_at_a_system_address_fails_the_block() {
    let mut db = common::database();
    db.set_account_nonce(ORACLE_CONTRACT_ADDRESS, 42);
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(&mut state, unlimited_ctx());
    let err = executor
        .apply_pre_execution_changes()
        .expect_err("a used empty account must fail the block");
    let message = err.to_string();
    assert!(message.contains("nonce 42"), "unexpected error: {message}");
    assert!(message.contains(&ORACLE_CONTRACT_ADDRESS.to_string()));
}

/// Foreign code at any of the seven addresses fails the block.
#[test]
fn test_foreign_code_at_a_system_address_fails_the_block() {
    for (address, _) in expected_hashes() {
        let mut db = common::database();
        db.set_account_code(address, Bytes::from_static(&[0xfe]));
        let mut state = State::builder().with_database(db).build();
        let mut executor = executor(&mut state, unlimited_ctx());
        let err =
            executor.apply_pre_execution_changes().expect_err("foreign code must fail the block");
        let message = err.to_string();
        assert!(
            message.contains("refusing to overwrite"),
            "{address}: unexpected error: {message}"
        );
        assert!(message.contains(&address.to_string()), "{address}: error names the address");
    }
}

/// A chain that holds an older version of a contract is upgraded by the block's deploy step, in
/// place: the step delivers the account as a change to the pre-block observer, and the block's
/// state holds the newer code with the storage the contract had.
#[test]
fn test_an_older_oracle_is_upgraded_by_the_pre_block_step_with_its_storage() {
    const SLOT: U256 = U256::from_limbs([0x51, 0, 0, 0]);
    let value = U256::from(0xfeed_u64);
    let mut db = common::database();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, mega_system_contracts::oracle::V1_1_0_CODE);
    db.set_account_nonce(ORACLE_CONTRACT_ADDRESS, 3);
    db.set_account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, value);
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(&mut state, unlimited_ctx());
    let log = record_pre_block(&mut executor);

    executor.apply_pre_execution_changes().expect("the block starts");
    drop(executor);

    let states = pre_block_states(&log);
    let (_, upgrade) = states
        .iter()
        .find(|(source, _)| *source == PreBlockStateSource::SystemContract(ORACLE_CONTRACT_ADDRESS))
        .expect("the Oracle's step is delivered");
    let account = upgrade.get(&ORACLE_CONTRACT_ADDRESS).expect("the Oracle is in its state");
    assert!(account.is_touched() && !account.is_created());
    assert_eq!(account.info.code_hash, ORACLE_CONTRACT_CODE_HASH);
    assert!(account.storage.is_empty());

    let info = state.basic(ORACLE_CONTRACT_ADDRESS).unwrap().expect("the Oracle is there");
    assert_eq!(info.code_hash, ORACLE_CONTRACT_CODE_HASH, "the block holds the newer code");
    assert_eq!(info.nonce, 3, "the nonce is kept");
    assert_eq!(state.storage(ORACLE_CONTRACT_ADDRESS, SLOT).unwrap(), value, "the storage is kept");
}

/// A scheduled Satin without registry params fails at load, and a block built from one anyway
/// fails when it tries to deploy.
#[test]
fn test_missing_registry_params_fail_at_load_and_at_the_block() {
    let missing = MegaHardforkConfig::default()
        .with(MegaHardfork::Satin, alloy_hardforks::ForkCondition::Timestamp(0));
    assert_eq!(
        missing.validate_schedule().unwrap_err().to_string(),
        "hardfork Satin is scheduled but its SequencerRegistryConfig params are not configured"
    );

    let mut state = common::state();
    let mut executor = executor_with_spec(&mut state, unlimited_ctx(), missing);
    let err = executor
        .apply_pre_execution_changes()
        .expect_err("a block without registry params cannot deploy");
    assert!(err.to_string().contains("SequencerRegistryConfig"), "unexpected error: {err}");
}

/// Calling the factory with salt || initcode returns the CREATE2 address as 20 bytes.
#[test]
fn test_a_create2_round_trip_through_the_factory() {
    let mut state = common::state();
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the factory is deployed");

    let salt = B256::repeat_byte(0x11);
    // Constructor: RETURN empty runtime.
    let init = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
    let mut input = salt.to_vec();
    input.extend_from_slice(&init);
    let expected = CREATE2_FACTORY_ADDRESS.create2_from_code(salt, init.as_ref());
    // 1,000,000 of regular gas on top of the account the creation adds, the history of the body,
    // and 64 times the history of the creation's two write records, at the byte prices in effect:
    // the factory forwards all but a 64th of its gas to the creation and pays the records from
    // the 64th it keeps.
    let gas_limit = 1_000_000 +
        common::new_account_state_gas() +
        common::body_history(input.len() as u64) +
        64 * mega_evm::write_record_history_gas(2).expect("two records have a price");

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
