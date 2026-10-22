//! The predeploys and the check Satin's activation block makes of them: the list, a matching
//! account read as it is, and every other account refused with one error.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use mega_evm::{
    system::{
        check_system_contract,
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE_HASH},
        system_contract_specs, SystemContractCheckError, SystemContractSpec,
        ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE_HASH, CREATE2_FACTORY_ADDRESS,
        CREATE2_FACTORY_CODE, CREATE2_FACTORY_CODE_HASH, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE_HASH, SYSTEM_CONTRACT_ADDRESSES, SYSTEM_CONTRACT_COUNT,
    },
    test_utils::{ErrorInjectingDatabase, MemoryDatabase},
};
use revm::primitives::KECCAK_EMPTY;

/// The factory's pinned hash is the hash of the runtime the list names.
#[test]
fn test_the_factory_code_hashes_to_its_pinned_hash() {
    assert_eq!(keccak256(CREATE2_FACTORY_CODE), CREATE2_FACTORY_CODE_HASH);
}

/// [S18.8] The list is the six `MegaETH` contracts in address order, then the factory, each with
/// the code hash of its code, and the public address list is the list's addresses in that order.
///
/// Expected values: constants (the addresses and code hashes are the production constants).
#[test]
fn test_the_spec_list_is_the_seven_predeploys_in_order() {
    let specs = system_contract_specs();
    assert_eq!(specs.len(), SYSTEM_CONTRACT_COUNT);
    assert_eq!(
        specs.clone().map(|spec| (spec.address, spec.code_hash)),
        [
            (ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH),
            (HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH),
            (KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE_HASH),
            (ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE_HASH),
            (LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH),
            (SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE_HASH),
            (CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_CODE_HASH),
        ]
    );
    assert_eq!(specs.clone().map(|spec| spec.address), SYSTEM_CONTRACT_ADDRESSES);
    for spec in &specs {
        assert_eq!(keccak256(&spec.code), spec.code_hash, "{}", spec.address);
    }
}

/// The spec of `address` in the list.
fn spec_at(address: Address) -> SystemContractSpec {
    system_contract_specs()
        .into_iter()
        .find(|spec| spec.address == address)
        .expect("the address is a predeploy")
}

/// [S18] An address holding the expected code passes whatever else it holds — the nonce mainnet's
/// six hold (0), the factory's (in the thousands), a balance, storage — and the check returns the
/// account as a read-only entry, as held: neither touched nor created, no storage, the nonce kept.
///
/// Expected values: independent (the account each test puts in the database).
#[test]
fn test_the_expected_code_is_read_as_it_is() {
    for spec in system_contract_specs() {
        for nonce in [0, 1, 1_443] {
            let mut db = MemoryDatabase::default()
                .account_code(spec.address, spec.code.clone())
                .account_nonce(spec.address, nonce)
                .account_balance(spec.address, U256::from(9))
                .account_storage(spec.address, U256::from(1), U256::from(7));
            let state = check_system_contract(&mut db, &spec)
                .unwrap_or_else(|error| panic!("{} at nonce {nonce}: {error}", spec.address));
            assert_eq!(state.len(), 1, "{}: the account alone", spec.address);
            let account = &state[&spec.address];
            assert!(!account.is_touched() && !account.is_created(), "{}", spec.address);
            assert!(account.storage.is_empty(), "{}: nothing written", spec.address);
            assert_eq!(account.info.code_hash, spec.code_hash);
            assert_eq!((account.info.nonce, account.info.balance), (nonce, U256::from(9)));
        }
    }
}

/// [S18] Anything but the expected code is refused with the one error, naming the address, the
/// expected hash and the one held: no account, an account without code — funded, or at a used
/// nonce — an older version of the contract, another contract's version and other code. Satin
/// creates and upgrades nothing.
///
/// Expected values: independent (the code hash each state holds).
#[test]
fn test_anything_else_is_unexpected_code() {
    use mega_system_contracts::{oracle, sequencer_registry};
    let mut near_miss = oracle::V1_1_0_CODE.to_vec();
    *near_miss.last_mut().unwrap() ^= 1;
    let near_miss = Bytes::from(near_miss);
    let cases: Vec<(&str, Address, MemoryDatabase, B256)> = vec![
        ("no account", ORACLE_CONTRACT_ADDRESS, MemoryDatabase::default(), KECCAK_EMPTY),
        (
            "a funded account without code",
            SEQUENCER_REGISTRY_ADDRESS,
            MemoryDatabase::default().account_balance(SEQUENCER_REGISTRY_ADDRESS, U256::from(1)),
            KECCAK_EMPTY,
        ),
        (
            "an account without code at a used nonce",
            CREATE2_FACTORY_ADDRESS,
            MemoryDatabase::default().account_nonce(CREATE2_FACTORY_ADDRESS, 42),
            KECCAK_EMPTY,
        ),
        (
            "the Oracle at 1.0.0",
            ORACLE_CONTRACT_ADDRESS,
            MemoryDatabase::default().account_code(ORACLE_CONTRACT_ADDRESS, oracle::V1_0_0_CODE),
            oracle::V1_0_0_CODE_HASH,
        ),
        (
            "the Oracle at 1.1.0",
            ORACLE_CONTRACT_ADDRESS,
            MemoryDatabase::default().account_code(ORACLE_CONTRACT_ADDRESS, oracle::V1_1_0_CODE),
            oracle::V1_1_0_CODE_HASH,
        ),
        (
            "the registry at 1.0.0",
            SEQUENCER_REGISTRY_ADDRESS,
            MemoryDatabase::default()
                .account_code(SEQUENCER_REGISTRY_ADDRESS, sequencer_registry::V1_0_0_CODE),
            sequencer_registry::V1_0_0_CODE_HASH,
        ),
        (
            "the Oracle's 1.0.0 at the registry's address",
            SEQUENCER_REGISTRY_ADDRESS,
            MemoryDatabase::default().account_code(SEQUENCER_REGISTRY_ADDRESS, oracle::V1_0_0_CODE),
            oracle::V1_0_0_CODE_HASH,
        ),
        (
            "a near miss of the Oracle's 1.1.0",
            ORACLE_CONTRACT_ADDRESS,
            MemoryDatabase::default().account_code(ORACLE_CONTRACT_ADDRESS, near_miss.clone()),
            keccak256(&near_miss),
        ),
        (
            "other code at the factory's address",
            CREATE2_FACTORY_ADDRESS,
            MemoryDatabase::default()
                .account_code(CREATE2_FACTORY_ADDRESS, Bytes::from_static(&[0xfe])),
            keccak256([0xfe]),
        ),
    ];
    for (name, address, mut db, held) in cases {
        let spec = spec_at(address);
        let error = check_system_contract(&mut db, &spec).expect_err(name);
        match &error {
            SystemContractCheckError::UnexpectedCode { address: at, expected, found } => {
                assert_eq!((*at, *expected, *found), (address, spec.code_hash, held), "{name}")
            }
            other => panic!("{name}: expected UnexpectedCode, got {other}"),
        }
        let message = error.to_string();
        assert!(message.contains(&address.to_string()), "{name}: {message}");
        assert!(message.contains(&spec.code_hash.to_string()), "{name}: {message}");
        let holding = if held == KECCAK_EMPTY {
            "it holds no code".to_string()
        } else {
            format!("it holds code hash {held}")
        };
        assert!(message.ends_with(&holding), "{name}: {message}");
    }
}

/// A failed account load is [`SystemContractCheckError::Database`], and that inner error is the
/// `source` of the check's error.
#[test]
fn test_a_database_error_is_the_source_of_the_check_error() {
    let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
    db.fail_on_account = Some(ORACLE_CONTRACT_ADDRESS);
    let err = check_system_contract(&mut db, &spec_at(ORACLE_CONTRACT_ADDRESS))
        .expect_err("the load fails");
    assert!(matches!(err, SystemContractCheckError::Database(_)), "{err}");
    assert!(core::error::Error::source(&err).is_some(), "the database error is the source");
}
