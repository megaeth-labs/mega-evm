//! Unit tests extracted from `crates/mega-evm/src/system/sequencer_registry.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/system/sequencer_registry.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256, B256};
    use mega_system_contracts::sequencer_registry::storage_slots::PENDING_ADMIN;
    use revm::{
        context::BlockEnv,
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    use crate::{MegaHardforkConfig, MegaSpecId};

    const TEST_SEQUENCER: Address = address!("0xBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");
    const TEST_ADMIN: Address = address!("0xCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC");
    /// A non-system test address, used as a stand-in for a `_currentSystemAddress` that has
    /// already been rotated away from the genesis `MEGA_SYSTEM_ADDRESS`. Distinct from
    /// genesis so tests can tell the two apart.
    const TEST_SYSTEM_ADDRESS: Address = address!("0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d");

    const TEST_MIN_ROTATION_DELAY: u64 = 100;

    fn test_config() -> SequencerRegistryConfig {
        SequencerRegistryConfig {
            rex5_initial_sequencer: TEST_SEQUENCER,
            rex5_initial_admin: TEST_ADMIN,
        }
    }

    fn test_rex6_config() -> SequencerRegistryRex6Config {
        SequencerRegistryRex6Config { rex6_min_rotation_delay: TEST_MIN_ROTATION_DELAY }
    }

    /// A chain running Rex5: the pre-upgrade world where the registry runs v1.0.0.
    fn rex5_hardforks() -> MegaHardforkConfig {
        MegaHardforkConfig::default()
            .with_all_activated_through(MegaSpecId::REX5)
            .with_params(test_config())
    }

    /// All hardforks including Rex6, with both registry param sets attached.
    fn rex6_hardforks() -> MegaHardforkConfig {
        MegaHardforkConfig::default()
            .with_all_activated_through(MegaSpecId::REX6)
            .with_params(test_config())
            .with_params(test_rex6_config())
    }

    /// Verifies that Rust slot constants match the Solidity storage layout.
    /// These values come from `forge inspect SequencerRegistry storage-layout`.
    /// If the Solidity field order changes, this test MUST be updated.
    #[test]
    fn test_slot_constants_match_solidity_layout() {
        assert_eq!(CURRENT_SYSTEM_ADDRESS, U256::from(0), "_currentSystemAddress = slot 0");
        assert_eq!(CURRENT_SEQUENCER, U256::from(1), "_currentSequencer = slot 1");
        assert_eq!(ADMIN, U256::from(2), "_admin = slot 2");
        assert_eq!(PENDING_ADMIN, U256::from(3), "_pendingAdmin = slot 3");
        assert_eq!(INITIAL_SYSTEM_ADDRESS, U256::from(4), "_initialSystemAddress = slot 4");
        assert_eq!(INITIAL_SEQUENCER, U256::from(5), "_initialSequencer = slot 5");
        assert_eq!(INITIAL_FROM_BLOCK, U256::from(6), "_initialFromBlock = slot 6");
        assert_eq!(PENDING_SYSTEM_ADDRESS, U256::from(7), "_pendingSystemAddress = slot 7");
        assert_eq!(
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(8),
            "_systemAddressActivationBlock = slot 8"
        );
        assert_eq!(PENDING_SEQUENCER, U256::from(9), "_pendingSequencer = slot 9");
        assert_eq!(
            SEQUENCER_ACTIVATION_BLOCK,
            U256::from(10),
            "_sequencerActivationBlock = slot 10"
        );
        // Slots 11 (_systemAddressHistory) and 12 (_sequencerHistory) are dynamic arrays.
        assert_eq!(MIN_ROTATION_DELAY, U256::from(13), "_minRotationDelay = slot 13 (v2.0.0+)");
    }

    #[test]
    fn test_validate_rejects_zero_rex5_initial_sequencer() {
        let mut config = test_config();
        config.rex5_initial_sequencer = Address::ZERO;
        let err = config.validate().expect_err("zero rex5_initial_sequencer must be rejected");
        assert!(
            err.message.contains("rex5_initial_sequencer must not be zero"),
            "unexpected message: {}",
            err.message,
        );
    }

    #[test]
    fn test_validate_rejects_zero_rex5_initial_admin() {
        let mut config = test_config();
        config.rex5_initial_admin = Address::ZERO;
        let err = config.validate().expect_err("zero rex5_initial_admin must be rejected");
        assert!(
            err.message.contains("rex5_initial_admin must not be zero"),
            "unexpected message: {}",
            err.message,
        );
    }

    #[test]
    fn test_validate_rejects_zero_min_rotation_delay() {
        let config = SequencerRegistryRex6Config { rex6_min_rotation_delay: 0 };
        let err = config.validate().expect_err("zero rex6_min_rotation_delay must be rejected");
        assert!(
            err.message.contains("rex6_min_rotation_delay must not be zero"),
            "unexpected message: {}",
            err.message,
        );
    }

    #[test]
    fn test_deploy_fresh_at_rex6_seeds_v2_with_min_rotation_delay() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let result = transact_deploy_sequencer_registry(
            &rex6_hardforks(),
            0,
            1000,
            &mut state,
            &test_config(),
        )
        .unwrap()
        .unwrap();

        let account = result.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert!(account.is_created());
        assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH_REX6);
        assert_eq!(
            account.storage.len(),
            7,
            "fresh Rex6 deploy writes the 6 bootstrap slots plus _minRotationDelay"
        );
        assert_eq!(
            account.storage.get(&MIN_ROTATION_DELAY).unwrap().present_value(),
            U256::from(TEST_MIN_ROTATION_DELAY),
        );
        assert_eq!(
            account.storage.get(&ADMIN).unwrap().present_value(),
            U256::from_be_bytes(TEST_ADMIN.into_word().0),
        );
        assert_eq!(
            account.storage.get(&INITIAL_FROM_BLOCK).unwrap().present_value(),
            U256::from(1000),
        );
    }

    #[test]
    fn test_deploy_rex6_is_idempotent_when_v2_present() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH_REX6,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE_REX6)),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let result = transact_deploy_sequencer_registry(
            &rex6_hardforks(),
            0,
            2000,
            &mut state,
            &test_config(),
        )
        .unwrap()
        .unwrap();

        let account = result.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert!(!account.is_created(), "idempotent deploy should not re-create");
        assert!(!account.is_touched(), "idempotent deploy should not touch the account");
        assert!(account.storage.is_empty(), "idempotent deploy should not write storage");
    }

    #[test]
    fn test_deploy_rex6_missing_config_errors() {
        // Rex6 active but only the Rex5 params attached: deploy must fail fast instead of
        // installing a v2.0.0 registry with an unseeded _minRotationDelay.
        let hardforks = MegaHardforkConfig::default()
            .with_all_activated_through(MegaSpecId::REX6)
            .with_params(test_config());
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let err =
            transact_deploy_sequencer_registry(&hardforks, 0, 1000, &mut state, &test_config())
                .expect_err("missing Rex6 params must fail closed");
        assert!(
            err.to_string().contains("SequencerRegistryRex6Config not configured"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn test_deploy_rex6_on_top_of_eoa_with_balance() {
        let mut db = InMemoryDB::default();
        // Simulate an EOA that received ETH before the registry was ever deployed. Even at
        // Rex6 this is a fresh bootstrap, not a storage-preserving upgrade: the full seed
        // (including _minRotationDelay) must be written.
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let result = transact_deploy_sequencer_registry(
            &rex6_hardforks(),
            0,
            1000,
            &mut state,
            &test_config(),
        )
        .unwrap()
        .unwrap();

        let account = result.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert!(account.is_created());
        assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH_REX6);
        assert_eq!(account.info.balance, U256::from(1_000_000));
        assert_eq!(account.storage.len(), 7);
        assert_eq!(
            account.storage.get(&MIN_ROTATION_DELAY).unwrap().present_value(),
            U256::from(TEST_MIN_ROTATION_DELAY),
        );
    }

    #[test]
    fn test_deploy_rex6_wrong_existing_code_hash_errors() {
        let wrong_code = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: wrong_code.hash_slow(),
                code: Some(wrong_code),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let err = transact_deploy_sequencer_registry(
            &rex6_hardforks(),
            0,
            2000,
            &mut state,
            &test_config(),
        )
        .expect_err("wrong code hash must fail closed");

        let msg = err.to_string();
        assert!(msg.contains("unexpected code hash"), "unexpected message: {msg}");
        assert!(msg.contains("refusing to overwrite"), "unexpected message: {msg}");
    }

    #[test]
    fn test_resolve_rex6_expects_v2_code_hash() {
        // A V2 registry resolves normally at REX6.
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH_REX6,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE_REX6)),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SYSTEM_ADDRESS,
            TEST_SYSTEM_ADDRESS.into_word().into(),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let (addr, witness) =
            resolve_system_address(&rex6_hardforks(), MegaSpecId::REX6, &mut state).unwrap();
        assert_eq!(addr, TEST_SYSTEM_ADDRESS);
        assert!(witness.is_some());
    }

    #[test]
    fn test_resolve_rex6_rejects_v1_code_hash() {
        // At REX6 the pre-block deploy has already upgraded the bytecode, so a V1 code hash at
        // resolve time is a broken pipeline and must fail closed.
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SYSTEM_ADDRESS,
            TEST_SYSTEM_ADDRESS.into_word().into(),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let err = resolve_system_address(&rex6_hardforks(), MegaSpecId::REX6, &mut state)
            .expect_err("V1 code hash at REX6 must fail closed");
        assert!(err.to_string().contains("code hash mismatch"));
    }

    #[test]
    fn test_resolve_rex5_returns_stored_system_address_with_witness() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SYSTEM_ADDRESS,
            TEST_SYSTEM_ADDRESS.into_word().into(),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let (addr, witness) =
            resolve_system_address(&rex5_hardforks(), MegaSpecId::REX5, &mut state).unwrap();
        assert_eq!(addr, TEST_SYSTEM_ADDRESS);

        // Witness must capture the registry account and the CURRENT_SYSTEM_ADDRESS slot.
        let witness = witness.expect("post-deploy resolve must produce witness");
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("registry account in witness");
        assert_eq!(acc.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
        assert!(
            acc.storage.contains_key(&CURRENT_SYSTEM_ADDRESS),
            "witness must include CURRENT_SYSTEM_ADDRESS slot"
        );
        assert_eq!(acc.storage.len(), 1, "witness should contain exactly one slot");
        let slot = acc.storage.get(&CURRENT_SYSTEM_ADDRESS).expect("slot must exist");
        assert!(!slot.is_changed(), "read-only witness slot must not be marked changed");
        assert_eq!(slot.original_value(), slot.present_value());
    }

    #[test]
    fn test_resolve_rex5_zero_slot_errors() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let result = resolve_system_address(&rex5_hardforks(), MegaSpecId::REX5, &mut state);
        assert!(result.is_err(), "zero _currentSystemAddress should be an error");
    }

    #[test]
    fn test_resolve_rex5_missing_registry_errors() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = rex5_hardforks();

        let err = resolve_system_address(&hardforks, MegaSpecId::REX5, &mut state)
            .expect_err("missing registry at Rex5 must fail closed");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn test_resolve_rex5_wrong_code_hash_errors() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: B256::ZERO,
                code: Some(Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]))),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let err = resolve_system_address(&rex5_hardforks(), MegaSpecId::REX5, &mut state)
            .expect_err("wrong code hash must fail closed");

        assert!(err.to_string().contains("code hash mismatch"));
    }

    #[test]
    fn test_is_apply_pending_changes_due_no_registry() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let (due, witness) = is_apply_pending_changes_due(&mut state, 1000).unwrap();
        assert!(!due);
        // Witness must contain a not-existing account entry.
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness should exist");
        assert_eq!(acc.status, revm::state::AccountStatus::LoadedAsNotExisting);
        assert!(acc.storage.is_empty(), "no-registry witness should have no slots");
    }

    #[test]
    fn test_is_apply_pending_changes_due_no_pending() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let (due, witness) = is_apply_pending_changes_due(&mut state, 1000).unwrap();
        assert!(!due);
        // Witness must include both PENDING_* slots (both zero = no change pending).
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness should exist");
        assert!(
            acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS),
            "witness must include PENDING_SYSTEM_ADDRESS"
        );
        assert!(
            acc.storage.contains_key(&PENDING_SEQUENCER),
            "witness must include PENDING_SEQUENCER"
        );
        // No activation slots needed because both pending values are zero.
        assert_eq!(acc.storage.len(), 2, "no-pending witness should have exactly 2 slots");
    }

    #[test]
    fn test_is_apply_pending_changes_due_system_address_due() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let new_addr = address!("0x1111111111111111111111111111111111111111");
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SYSTEM_ADDRESS,
            new_addr.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(1000),
        )
        .unwrap();
        // Not yet due at block 999.
        let mut state = State::builder().with_database(&mut db).build();
        let (due, witness) = is_apply_pending_changes_due(&mut state, 999).unwrap();
        assert!(!due, "not yet due");
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness");
        assert!(acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(acc.storage.contains_key(&SYSTEM_ADDRESS_ACTIVATION_BLOCK));

        // Exactly due at block 1000 (fresh State to avoid cache from prior call).
        let mut state = State::builder().with_database(&mut db).build();
        let (due, witness) = is_apply_pending_changes_due(&mut state, 1000).unwrap();
        assert!(due, "exactly due");
        // Both roles' pending slots are always read into the witness.
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness");
        assert!(acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(acc.storage.contains_key(&SYSTEM_ADDRESS_ACTIVATION_BLOCK));
        assert!(acc.storage.contains_key(&PENDING_SEQUENCER));
    }

    #[test]
    fn test_is_apply_pending_changes_due_sequencer_due() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let new_seq = address!("0x2222222222222222222222222222222222222222");
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SEQUENCER,
            new_seq.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SEQUENCER_ACTIVATION_BLOCK,
            U256::from(500),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let (due, witness) = is_apply_pending_changes_due(&mut state, 500).unwrap();
        assert!(due);
        // System address has no pending change (slot is zero) so only PENDING_SYSTEM_ADDRESS
        // is read. Then sequencer path reads PENDING_SEQUENCER + SEQUENCER_ACTIVATION_BLOCK.
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness");
        assert!(acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(acc.storage.contains_key(&PENDING_SEQUENCER));
        assert!(acc.storage.contains_key(&SEQUENCER_ACTIVATION_BLOCK));
        assert_eq!(acc.storage.len(), 3);
    }

    #[test]
    fn test_is_apply_pending_changes_due_checks_sequencer_when_system_not_due() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let new_sys = address!("0x1111111111111111111111111111111111111111");
        let new_seq = address!("0x2222222222222222222222222222222222222222");
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SYSTEM_ADDRESS,
            new_sys.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(1001),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SEQUENCER,
            new_seq.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SEQUENCER_ACTIVATION_BLOCK,
            U256::from(1000),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let (due, witness) = is_apply_pending_changes_due(&mut state, 1000).unwrap();
        assert!(
            due,
            "sequencer change should still trigger the pre-block call when the system address is not due yet"
        );
        // System address is not due (activation at 1001) but its pending slot + activation
        // slot are still read.  Then the sequencer path reads its own pending + activation.
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness");
        assert!(acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(acc.storage.contains_key(&SYSTEM_ADDRESS_ACTIVATION_BLOCK));
        assert!(acc.storage.contains_key(&PENDING_SEQUENCER));
        assert!(acc.storage.contains_key(&SEQUENCER_ACTIVATION_BLOCK));
        assert_eq!(acc.storage.len(), 4, "both roles' slots should be in the witness");
        for slot_key in [
            PENDING_SYSTEM_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            PENDING_SEQUENCER,
            SEQUENCER_ACTIVATION_BLOCK,
        ] {
            let slot = acc.storage.get(&slot_key).expect("slot must exist");
            assert!(!slot.is_changed(), "read-only witness slot must not be marked changed");
            assert_eq!(slot.original_value(), slot.present_value());
        }
    }

    #[test]
    fn test_transact_apply_pending_changes_updates_and_clears_due_roles() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );

        let next_system_address = address!("0x1111111111111111111111111111111111111111");
        let next_sequencer = address!("0x2222222222222222222222222222222222222222");

        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SYSTEM_ADDRESS,
            TEST_SYSTEM_ADDRESS.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SEQUENCER,
            TEST_SEQUENCER.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SYSTEM_ADDRESS,
            next_system_address.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(1000),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SEQUENCER,
            next_sequencer.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SEQUENCER_ACTIVATION_BLOCK,
            U256::from(1000),
        )
        .unwrap();

        let block =
            BlockEnv { number: U256::from(1000), gas_limit: 30_000_000, ..Default::default() };
        let mut context = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(context);

        let result =
            transact_apply_pending_changes(&mut evm).expect("applyPendingChanges() should succeed");
        let state = result.state;
        drop(evm);

        revm::DatabaseCommit::commit(&mut db, state);

        assert_eq!(
            revm::Database::storage(&mut db, SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS)
                .unwrap(),
            address_to_storage_value(next_system_address),
        );
        assert_eq!(
            revm::Database::storage(&mut db, SEQUENCER_REGISTRY_ADDRESS, CURRENT_SEQUENCER)
                .unwrap(),
            address_to_storage_value(next_sequencer),
        );
        assert_eq!(
            revm::Database::storage(&mut db, SEQUENCER_REGISTRY_ADDRESS, PENDING_SYSTEM_ADDRESS)
                .unwrap(),
            U256::ZERO,
        );
        assert_eq!(
            revm::Database::storage(&mut db, SEQUENCER_REGISTRY_ADDRESS, PENDING_SEQUENCER)
                .unwrap(),
            U256::ZERO,
        );
        assert_eq!(
            revm::Database::storage(
                &mut db,
                SEQUENCER_REGISTRY_ADDRESS,
                SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            )
            .unwrap(),
            U256::ZERO,
        );
        assert_eq!(
            revm::Database::storage(
                &mut db,
                SEQUENCER_REGISTRY_ADDRESS,
                SEQUENCER_ACTIVATION_BLOCK,
            )
            .unwrap(),
            U256::ZERO,
        );
    }

    #[test]
    fn test_transact_apply_pending_changes_uses_block_gas_limit() {
        // Block gas_limit > 30M must be passed through to the system call so that
        // applyPendingChanges() can absorb the variable cost from REX dynamic
        // storage gas instead of being capped at the upstream-fixed 30M.
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );

        let block =
            BlockEnv { number: U256::from(1000), gas_limit: 250_000_000, ..Default::default() };
        let mut context = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(context);

        transact_apply_pending_changes(&mut evm).expect("system call should succeed");
        assert_eq!(revm::handler::EvmTr::ctx_ref(&evm).tx.base.gas_limit, 250_000_000);
    }

    #[test]
    fn test_transact_apply_pending_changes_respects_30m_floor() {
        // When the block gas limit is below the 30M floor, the system call must
        // still receive at least the historical default budget.
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );

        let block =
            BlockEnv { number: U256::from(1000), gas_limit: 1_000_000, ..Default::default() };
        let mut context = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(context);

        transact_apply_pending_changes(&mut evm).expect("system call should succeed");
        assert_eq!(
            revm::handler::EvmTr::ctx_ref(&evm).tx.base.gas_limit,
            crate::constants::rex5::SYSTEM_CALL_GAS_LIMIT_FLOOR,
        );
    }

    #[test]
    fn test_transact_apply_pending_changes_errors_when_registry_reverts() {
        let revert_code = Bytecode::new_legacy(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]));

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: revert_code.hash_slow(),
                code: Some(revert_code),
                ..Default::default()
            },
        );

        let block =
            BlockEnv { number: U256::from(1000), gas_limit: 30_000_000, ..Default::default() };
        let mut context = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(context);

        let err = transact_apply_pending_changes(&mut evm)
            .expect_err("reverting registry bytecode must fail closed");

        assert!(err.to_string().contains("reverted or halted"));
    }
}
