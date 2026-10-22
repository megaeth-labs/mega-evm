//! The check Satin's activation block makes of the predeploys, and the EIP-7997 factory round
//! trip.
//!
//! The block in which Satin activates reads the six system contracts and the factory, which a chain
//! that ran Rex6 holds, and is invalid when one of them does not hold the code Satin expects; no
//! other block reads them, and no block deploys or upgrades them.

use alloy_eips::{eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS};
use alloy_evm::block::{BlockExecutionError, BlockExecutor, BlockValidationError};
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use mega_evm::{
    system::{
        storage_slots::{PENDING_SEQUENCER, PENDING_SYSTEM_ADDRESS},
        system_contract_specs, CREATE2_FACTORY_ADDRESS, ORACLE_CONTRACT_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
    },
    test_utils::{rex6_predeploys, CommittedStates, ErrorInjectingDatabase, MemoryDatabase},
    MegaBlockExecutionCtx, MegaBlockExecutionError,
};
use op_revm::constants::L1_BLOCK_CONTRACT;
use revm::{
    context::result::ExecutionResult, database::State, primitives::KECCAK_EMPTY, state::EvmState,
};

use crate::common::{self, executor, named_accounts, recovered, tx, unlimited_ctx};

/// The context of the block in which Satin activates, as a node sets it from the schedule and
/// the parent's timestamp: deposit-only, and the check of the predeploys.
fn activation_ctx() -> MegaBlockExecutionCtx {
    unlimited_ctx().with_no_user_tx_activation_block(true).with_satin_activation_block(true)
}

/// The states the pre-block steps of a block over `state` held to `ctx` committed, in order, as a
/// commit hook on `state` receives them.
fn start(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
) -> Result<Vec<EvmState>, BlockExecutionError> {
    let committed = CommittedStates::install(state);
    let mut executor = executor(state, ctx);
    executor.apply_pre_execution_changes()?;
    assert_eq!(executor.gas().execution, 0);
    assert_eq!(executor.gas().state, 0);
    assert_eq!(executor.gas().history, 0);
    Ok(committed.states())
}

/// The account each pre-block state of a block names, in commit order: the two EIP calls' targets,
/// in the activation block the seven predeploys in order, the registry, whose pending changes are
/// read and found none due on these blocks, and the L1 block contract, whose info is read.
fn expected_accounts(activation: bool) -> Vec<Address> {
    let mut expected = vec![HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS];
    if activation {
        expected.extend(system_contract_specs().map(|spec| spec.address));
    }
    expected.extend([SEQUENCER_REGISTRY_ADDRESS, L1_BLOCK_CONTRACT]);
    expected
}

/// [S18] Satin's activation block over a state shaped as Rex6 leaves it finds every predeploy with
/// Satin's code. Each pre-block step commits a state of its own, and a commit hook receives them in
/// order, read-only entries included: the two EIP calls, which touch their contracts; the seven
/// accounts, each a read-only entry as the chain holds it — no write, no created mark, the six at
/// nonce 0 and the factory at its own; the registry's two pending slots, read only; and the L1
/// block contract, read only. The block after it, on the committed state, reads none of the
/// predeploys, and neither block changes one.
///
/// Expected values: independent (the Rex6-shaped accounts the test puts in the state).
#[test]
fn test_the_activation_block_reads_the_seven_predeploys_once() {
    let mut state = common::state();
    let activation = start(&mut state, activation_ctx()).expect("the activation block starts");
    assert_eq!(named_accounts(&activation), expected_accounts(true));
    for (call, target) in activation.iter().zip([HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS]) {
        assert!(call[&target].is_touched(), "the call to {target} touches it");
    }
    let held = rex6_predeploys(common::registry_roles());
    for (i, (address, info, _)) in held.iter().enumerate() {
        let account = &activation[2 + i][address];
        assert!(!account.is_touched() && !account.is_created(), "{address} is read only");
        assert!(account.storage.is_empty(), "{address}: no slot is written");
        assert_eq!((account.info.code_hash, account.info.nonce), (info.code_hash, info.nonce));
    }
    let registry = &activation[2 + held.len()][&SEQUENCER_REGISTRY_ADDRESS];
    assert!(!registry.is_touched() && !registry.is_created(), "the registry is read only");
    assert_eq!(
        registry.storage.keys().copied().collect::<std::collections::BTreeSet<_>>(),
        [PENDING_SYSTEM_ADDRESS, PENDING_SEQUENCER].into(),
        "the read of its pending changes"
    );
    assert!(registry.storage.values().all(|slot| !slot.is_changed()));
    let l1_block = &activation[3 + held.len()][&L1_BLOCK_CONTRACT];
    assert!(!l1_block.is_touched() && !l1_block.is_created(), "the L1 block info is read only");

    let next = start(&mut state, unlimited_ctx()).expect("the next block starts");
    assert_eq!(named_accounts(&next), expected_accounts(false), "no other block reads them");
    for (address, info, _) in held {
        let now = revm::Database::basic(&mut state, address).unwrap().expect("still there");
        assert_eq!((now.code_hash, now.nonce), (info.code_hash, info.nonce), "{address}");
    }
}

/// [S18] A block that is not Satin's activation block reads no predeploy: over a database that
/// fails every load of the Oracle's account, it starts, where the activation block fails on the
/// load as an internal error, the database's and no verdict on the block.
///
/// Expected values: independent (the injected failure).
#[test]
fn test_no_other_block_reads_a_predeploy() {
    let failing = || {
        let mut db = ErrorInjectingDatabase::new(common::database());
        db.fail_on_account = Some(ORACLE_CONTRACT_ADDRESS);
        State::builder().with_database(db).build()
    };
    let start_on = |ctx: MegaBlockExecutionCtx| {
        let mut state = failing();
        let evm = alloy_evm::EvmFactory::create_evm(
            &mega_evm::MegaEvmFactory::new(),
            &mut state,
            common::evm_env(),
        );
        mega_evm::MegaBlockExecutor::new(
            evm,
            ctx,
            common::chain_spec(),
            alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder::default(),
        )
        .apply_pre_execution_changes()
    };
    start_on(unlimited_ctx()).expect("an ordinary block reads no predeploy");
    let error = start_on(activation_ctx()).expect_err("the activation block reads the Oracle");
    assert!(matches!(error, BlockExecutionError::Internal(_)), "{error:?}");
    assert!(error.to_string().contains("injected basic() error"), "{error}");
}

/// The refusal of Satin's activation block over `db`: a verdict on the block, before the refused
/// predeploy's state is committed, so the last state committed is the predeploy before it.
fn refused(db: MemoryDatabase, at: Address) -> MegaBlockExecutionError {
    let mut state = State::builder().with_database(db).build();
    let committed = CommittedStates::install(&mut state);
    let mut executor = executor(&mut state, activation_ctx());
    let error = executor.apply_pre_execution_changes().expect_err("the block is refused");
    let delivered = named_accounts(&committed.states());
    let position = system_contract_specs().iter().position(|spec| spec.address == at).unwrap();
    assert_eq!(delivered, expected_accounts(true)[..2 + position], "refused at {at}");
    let Some(BlockValidationError::Other(inner)) = error.as_validation() else {
        panic!("a verdict on the block: {error:?}");
    };
    inner.downcast_ref::<MegaBlockExecutionError>().expect("the engine's refusal").clone()
}

/// [S18] The activation block is invalid when a predeploy does not hold Satin's code, with one
/// error naming the address, the expected hash and the one held: other code at any of the seven,
/// no account, an account without code — funded, or at a used nonce — the Oracle at 1.0.0 or
/// 1.1.0, and the registry at 1.0.0. Nothing is created or upgraded.
///
/// Expected values: independent (the code hash each state holds).
#[test]
fn test_an_activation_block_without_the_expected_code_is_refused() {
    use mega_system_contracts::{oracle, sequencer_registry};
    let with = |edit: &dyn Fn(&mut MemoryDatabase)| {
        let mut db = common::database();
        edit(&mut db);
        db
    };
    let mut cases: Vec<(String, Address, MemoryDatabase, B256)> = system_contract_specs()
        .into_iter()
        .map(|spec| {
            let db = with(&|db| db.set_account_code(spec.address, Bytes::from_static(&[0xfe])));
            (format!("other code at {}", spec.address), spec.address, db, keccak256([0xfe]))
        })
        .collect();
    cases.extend([
        (
            "no account at the Oracle's address".to_string(),
            ORACLE_CONTRACT_ADDRESS,
            with(&|db| {
                db.cache.accounts.remove(&ORACLE_CONTRACT_ADDRESS);
            }),
            KECCAK_EMPTY,
        ),
        (
            "a funded registry without code".to_string(),
            SEQUENCER_REGISTRY_ADDRESS,
            with(&|db| {
                db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, Bytes::new());
                db.set_account_balance(SEQUENCER_REGISTRY_ADDRESS, U256::from(1));
            }),
            KECCAK_EMPTY,
        ),
        (
            "a factory without code at a used nonce".to_string(),
            CREATE2_FACTORY_ADDRESS,
            with(&|db| db.set_account_code(CREATE2_FACTORY_ADDRESS, Bytes::new())),
            KECCAK_EMPTY,
        ),
        (
            "the Oracle at 1.0.0".to_string(),
            ORACLE_CONTRACT_ADDRESS,
            with(&|db| db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle::V1_0_0_CODE)),
            oracle::V1_0_0_CODE_HASH,
        ),
        (
            "the Oracle at 1.1.0".to_string(),
            ORACLE_CONTRACT_ADDRESS,
            with(&|db| db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle::V1_1_0_CODE)),
            oracle::V1_1_0_CODE_HASH,
        ),
        (
            "the registry at 1.0.0".to_string(),
            SEQUENCER_REGISTRY_ADDRESS,
            with(&|db| {
                db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, sequencer_registry::V1_0_0_CODE)
            }),
            sequencer_registry::V1_0_0_CODE_HASH,
        ),
    ]);
    for (name, address, db, found) in cases {
        let expected = system_contract_specs()
            .into_iter()
            .find(|spec| spec.address == address)
            .expect("a predeploy")
            .code_hash;
        let refusal = refused(db, address);
        assert_eq!(
            refusal,
            MegaBlockExecutionError::UnexpectedSystemContractCode { address, expected, found },
            "{name}"
        );
        let message = refusal.to_string();
        assert!(message.contains(&address.to_string()), "{name}: {message}");
        assert!(message.contains("when Satin activates"), "{name}: {message}");
    }
}

/// Calling the factory with salt || initcode returns the CREATE2 address as 20 bytes.
#[test]
fn test_a_create2_round_trip_through_the_factory() {
    let mut state = common::state();
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

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
