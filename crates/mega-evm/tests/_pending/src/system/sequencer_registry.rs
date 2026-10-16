//! Unit tests extracted from `crates/mega-evm/src/system/sequencer_registry.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/system/sequencer_registry.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

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
