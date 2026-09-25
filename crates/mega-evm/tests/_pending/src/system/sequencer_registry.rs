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
}
