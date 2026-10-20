//! System contract deployment at the start of a block, the `SequencerRegistry` it requires, and
//! the EIP-7997 factory round trip.

use alloy_evm::block::{BlockExecutor, BlockValidationError};
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE_HASH},
        IOracle, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE_HASH, CREATE2_FACTORY_ADDRESS,
        CREATE2_FACTORY_CODE, CREATE2_FACTORY_CODE_HASH, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE_HASH, SYSTEM_CONTRACT_DEPLOY_COUNT,
    },
    test_utils::MemoryDatabase,
    MegaBlockExecutionError, PreBlockStateSource,
};
use revm::{
    context::{result::ExecutionResult, ContextTr},
    database::{states::bundle_state::BundleRetention, OriginalValuesKnown, State},
    primitives::KECCAK_EMPTY,
    Database,
};

use crate::common::{
    self, executor, pre_block_states, record_pre_block, recovered, tx, unlimited_ctx,
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

/// The first block deploys the six predeploys the chain does not hold yet with the pinned hashes
/// and nonce 1, and writes no storage; it finds the `SequencerRegistry` the legacy forks left and
/// only reads it. The executor's own observer sees the two EIP calls, the seven deploy states and
/// the two reads of the registry, in that order.
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
    for (i, (address, hash)) in expected_hashes().into_iter().enumerate() {
        let state = &outcomes[2 + i].1;
        assert_eq!(state.len(), 1, "{address} is the only account in its state");
        let account = &state[&address];
        let created = address != SEQUENCER_REGISTRY_ADDRESS;
        assert_eq!(account.is_touched(), created, "{address} is touched when it is created");
        assert_eq!(account.is_created(), created, "{address}");
        assert_eq!(account.info.code_hash, hash, "{address} has the wrong code");
        assert_eq!(account.info.nonce, 1, "{address} is a created contract");
        assert!(account.storage.is_empty(), "{address}: the deploy writes no storage");
    }
    assert_deployed(&mut state);
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
        assert!(account.storage.is_empty(), "{address} is not written");
        assert_eq!(account.info.code_hash, hash);
    }
    assert_deployed(&mut state);
}

/// The database of [`common::database`] without the `SequencerRegistry`.
fn without_registry() -> MemoryDatabase {
    let mut db = common::database();
    db.cache.accounts.remove(&SEQUENCER_REGISTRY_ADDRESS);
    db
}

/// The refusal of the block over `db`, which the pre-block observer receives no state of the
/// registry before.
fn refused(db: MemoryDatabase) -> MegaBlockExecutionError {
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor(&mut state, unlimited_ctx());
    let log = record_pre_block(&mut executor);
    let error = executor.apply_pre_execution_changes().expect_err("the block is refused");
    let registry = PreBlockStateSource::SystemContract(SEQUENCER_REGISTRY_ADDRESS);
    assert!(pre_block_states(&log).iter().all(|(source, _)| *source != registry));
    let Some(BlockValidationError::Other(inner)) = error.as_validation() else {
        panic!("a verdict on the block: {error:?}");
    };
    inner.downcast_ref::<MegaBlockExecutionError>().expect("the engine's refusal").clone()
}

/// [S18] The `SequencerRegistry` must already hold v2.0.0, which the legacy forks deployed: a
/// block whose state holds no account at its address, an account without code — one only funded,
/// or one whose nonce is used — or the legacy engine's v1.0.0 is invalid, refused before the
/// registry's step reaches the observer. The block neither creates the registry nor upgrades it.
///
/// Expected values: independent (the code hash each state holds).
#[test]
fn test_a_block_without_the_v2_registry_is_refused() {
    use mega_system_contracts::sequencer_registry::{V1_0_0_CODE, V1_0_0_CODE_HASH};
    let funded = {
        let mut db = without_registry();
        db.set_account_balance(SEQUENCER_REGISTRY_ADDRESS, U256::from(1_000));
        db
    };
    let used = {
        let mut db = common::database();
        db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, Bytes::new());
        db
    };
    let v1 = {
        let mut db = common::database();
        db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, V1_0_0_CODE);
        db
    };
    for (name, db, found) in [
        ("no account", without_registry(), KECCAK_EMPTY),
        ("a funded account without code", funded, KECCAK_EMPTY),
        ("an account without code at nonce 1", used, KECCAK_EMPTY),
        ("v1.0.0", v1, V1_0_0_CODE_HASH),
    ] {
        let refusal = refused(db);
        assert_eq!(
            refusal,
            MegaBlockExecutionError::SequencerRegistryNotDeployed {
                address: SEQUENCER_REGISTRY_ADDRESS,
                expected: SEQUENCER_REGISTRY_CODE_HASH,
                found,
            },
            "{name}"
        );
        let message = refusal.to_string();
        assert!(message.contains("must already be deployed at v2.0.0"), "{name}: {message}");
        assert!(message.contains(&SEQUENCER_REGISTRY_ADDRESS.to_string()), "{name}: {message}");
    }
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

/// Foreign code at any of the seven addresses fails the block, the registry's included.
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
