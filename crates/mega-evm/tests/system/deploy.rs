//! The system-contract deploy helper: the spec list, a fresh deploy, the idempotent read, and
//! foreign code.

use alloy_primitives::{address, keccak256, Address, Bytes, U256};
use mega_evm::{
    system::{
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE_HASH},
        storage_slots::{
            ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, INITIAL_FROM_BLOCK,
            INITIAL_SEQUENCER, INITIAL_SYSTEM_ADDRESS,
        },
        system_contract_specs, transact_deploy, SequencerRegistryConfig, SystemContractDeployError,
        SystemContractSpec, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE_HASH,
        CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_CODE, CREATE2_FACTORY_CODE_HASH,
        CREATE2_FACTORY_NONCE, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH,
        MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE,
        ORACLE_CONTRACT_CODE_HASH, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE_HASH,
        SYSTEM_CONTRACT_DEPLOY_COUNT,
    },
    test_utils::{ErrorInjectingDatabase, MemoryDatabase},
    HardforkParams,
};
use revm::{primitives::KECCAK_EMPTY, DatabaseCommit};

const SEQUENCER: Address = address!("0x2222222222222222222222222222222222222222");
const ADMIN_ADDR: Address = address!("0x3333333333333333333333333333333333333333");

fn config() -> SequencerRegistryConfig {
    SequencerRegistryConfig {
        initial_system_address: MEGA_SYSTEM_ADDRESS,
        initial_sequencer: SEQUENCER,
        initial_admin: ADMIN_ADDR,
        initial_from_block: 7,
    }
}

fn oracle_spec() -> SystemContractSpec {
    SystemContractSpec::new(
        ORACLE_CONTRACT_ADDRESS,
        ORACLE_CONTRACT_CODE,
        ORACLE_CONTRACT_CODE_HASH,
    )
}

/// The factory's pinned hash is the hash of the runtime the spec installs.
#[test]
fn test_the_factory_code_hashes_to_its_pinned_hash() {
    assert_eq!(keccak256(CREATE2_FACTORY_CODE), CREATE2_FACTORY_CODE_HASH);
    assert_eq!(CREATE2_FACTORY_NONCE, 1);
}

/// The spec list is the six `MegaETH` contracts in address order, then the factory.
#[test]
fn test_the_spec_list_is_the_seven_predeploys_in_order() {
    let specs = system_contract_specs(&config());
    assert_eq!(specs.len(), SYSTEM_CONTRACT_DEPLOY_COUNT);
    assert_eq!(
        [
            specs[0].address,
            specs[1].address,
            specs[2].address,
            specs[3].address,
            specs[4].address,
            specs[5].address,
            specs[6].address,
        ],
        [
            ORACLE_CONTRACT_ADDRESS,
            HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
            KEYLESS_DEPLOY_ADDRESS,
            ACCESS_CONTROL_ADDRESS,
            LIMIT_CONTROL_ADDRESS,
            SEQUENCER_REGISTRY_ADDRESS,
            CREATE2_FACTORY_ADDRESS,
        ]
    );
    assert_eq!(
        [
            specs[0].code_hash,
            specs[1].code_hash,
            specs[2].code_hash,
            specs[3].code_hash,
            specs[4].code_hash,
            specs[5].code_hash,
            specs[6].code_hash,
        ],
        [
            ORACLE_CONTRACT_CODE_HASH,
            HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH,
            KEYLESS_DEPLOY_CODE_HASH,
            ACCESS_CONTROL_CODE_HASH,
            LIMIT_CONTROL_CODE_HASH,
            SEQUENCER_REGISTRY_CODE_HASH,
            CREATE2_FACTORY_CODE_HASH,
        ]
    );
    for spec in &specs {
        assert_eq!(spec.nonce, 1, "{} starts at nonce 1", spec.address);
        assert_eq!(keccak256(&spec.code), spec.code_hash);
    }
    assert!(specs[..5].iter().all(|spec| spec.seed.is_empty()));
    assert!(specs[6].seed.is_empty(), "the factory has no storage");
    assert_eq!(specs[5].seed.len(), 6, "the registry seeds the six bootstrap slots");
}

/// A fresh deploy creates the account, installs the code and seeds every slot.
#[test]
fn test_a_fresh_deploy_creates_the_account_and_seeds_storage() {
    let mut db = MemoryDatabase::default();
    let specs = system_contract_specs(&config());
    let registry = &specs[5];
    let factory = &specs[6];

    let state = transact_deploy(&mut db, registry).expect("the registry deploys");
    let account = state.get(&SEQUENCER_REGISTRY_ADDRESS).expect("the registry is in the witness");
    assert!(account.is_touched());
    assert!(account.is_created());
    assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
    assert_eq!(account.info.nonce, 1);
    assert_eq!(account.storage.len(), 6);
    assert_eq!(
        account.storage.get(&CURRENT_SYSTEM_ADDRESS).unwrap().present_value,
        U256::from_be_bytes(MEGA_SYSTEM_ADDRESS.into_word().0)
    );
    assert_eq!(
        account.storage.get(&CURRENT_SEQUENCER).unwrap().present_value,
        U256::from_be_bytes(SEQUENCER.into_word().0)
    );
    assert_eq!(
        account.storage.get(&ADMIN).unwrap().present_value,
        U256::from_be_bytes(ADMIN_ADDR.into_word().0)
    );
    assert_eq!(
        account.storage.get(&INITIAL_SYSTEM_ADDRESS).unwrap().present_value,
        U256::from_be_bytes(MEGA_SYSTEM_ADDRESS.into_word().0)
    );
    assert_eq!(
        account.storage.get(&INITIAL_SEQUENCER).unwrap().present_value,
        U256::from_be_bytes(SEQUENCER.into_word().0)
    );
    assert_eq!(account.storage.get(&INITIAL_FROM_BLOCK).unwrap().present_value, U256::from(7));
    db.commit(state);

    let state = transact_deploy(&mut db, factory).expect("the factory deploys");
    let account = state.get(&CREATE2_FACTORY_ADDRESS).expect("the factory is in the witness");
    assert!(account.is_created());
    assert_eq!(account.info.nonce, CREATE2_FACTORY_NONCE);
    assert!(account.storage.is_empty());
}

/// Already deployed with the matching code hash is a read-only witness entry.
#[test]
fn test_a_matching_deploy_is_a_read_only_witness_entry() {
    let mut db = MemoryDatabase::default();
    let spec = oracle_spec();
    let deployed = transact_deploy(&mut db, &spec).unwrap();
    db.commit(deployed);

    let state = transact_deploy(&mut db, &spec).expect("the second deploy is a no-op");
    let account = state.get(&ORACLE_CONTRACT_ADDRESS).expect("the read is in the witness");
    assert!(!account.is_touched(), "a matching deploy does not touch");
    assert!(!account.is_created(), "a matching deploy does not create");
    assert!(account.storage.is_empty(), "a matching deploy does not seed");
    assert_eq!(account.info.code_hash, ORACLE_CONTRACT_CODE_HASH);
}

/// Present with different code is an error, not an overwrite.
#[test]
fn test_foreign_code_at_a_system_address_is_an_error() {
    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, Bytes::from_static(&[0xfe]));

    let error = transact_deploy(&mut db, &oracle_spec()).expect_err("foreign code must fail");
    let message = error.to_string();
    assert!(message.contains("refusing to overwrite"), "the error names the refusal: {message}");
    match error {
        SystemContractDeployError::ForeignCode { address, expected, found } => {
            assert_eq!(address, ORACLE_CONTRACT_ADDRESS);
            assert_eq!(expected, ORACLE_CONTRACT_CODE_HASH);
            assert_eq!(found, keccak256([0xfe]));
            assert_ne!(found, KECCAK_EMPTY);
        }
        other => panic!("expected ForeignCode, got {other}"),
    }
}

/// An account that exists with empty code (an EOA that received value) is a fresh deploy: the
/// balance is kept and the slots are seeded.
#[test]
fn test_an_empty_account_at_the_address_is_a_fresh_deploy() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(SEQUENCER_REGISTRY_ADDRESS, U256::from(1_000));

    let spec = system_contract_specs(&config())
        .into_iter()
        .find(|spec| spec.address == SEQUENCER_REGISTRY_ADDRESS)
        .unwrap();
    let state = transact_deploy(&mut db, &spec).expect("empty code is a fresh deploy");
    let account = state.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
    assert!(account.is_created());
    assert_eq!(account.info.balance, U256::from(1_000));
    assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
    assert_eq!(account.storage.len(), 6);
}

/// Zero addresses in the registry config fail at load, not at the first block.
#[test]
fn test_registry_config_rejects_a_zero_address() {
    let mut config = config();
    config.initial_system_address = Address::ZERO;
    let err = config.validate().expect_err("zero system address");
    assert!(err.message.contains("initial_system_address must not be zero"));

    config = self::config();
    config.initial_sequencer = Address::ZERO;
    let err = config.validate().expect_err("zero sequencer");
    assert!(err.message.contains("initial_sequencer must not be zero"));

    config = self::config();
    config.initial_admin = Address::ZERO;
    let err = config.validate().expect_err("zero admin");
    assert!(err.message.contains("initial_admin must not be zero"));
}

/// The seeded current-system-address slot is slot 0, which is the slot a later rotation read
/// consults.
#[test]
fn test_the_seeded_system_address_slot_is_the_current_one() {
    assert_eq!(CURRENT_SYSTEM_ADDRESS, U256::ZERO);
}

/// A failed account load is [`SystemContractDeployError::Database`], and that inner error is
/// the `source` of the deploy error.
#[test]
fn test_a_database_error_is_the_source_of_the_deploy_error() {
    let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
    db.fail_on_account = Some(ORACLE_CONTRACT_ADDRESS);
    let err = transact_deploy(&mut db, &oracle_spec()).expect_err("the load fails");
    match &err {
        SystemContractDeployError::Database(_) => {}
        other => panic!("expected Database, got {other}"),
    }
    assert!(
        core::error::Error::source(&err).is_some(),
        "the database error is the source of the deploy error"
    );
}
