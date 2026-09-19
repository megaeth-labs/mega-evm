//! Unit tests extracted from `crates/mega-evm/src/system/deploy.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/system/deploy.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MegaHardfork, MegaHardforkConfig, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH, ORACLE_CONTRACT_CODE_HASH_REX2,
        ORACLE_CONTRACT_CODE_HASH_REX5,
    };
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::address;
    use revm::{database::InMemoryDB, state::AccountInfo, Database as _, DatabaseCommit};

    fn addrs(specs: &[SystemContractSpec]) -> Vec<Address> {
        specs.iter().map(|s| s.address).collect()
    }

    const SEEDED_ADDR: Address = address!("0x6342000000000000000000000000000000000099");
    const SEED_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);
    const UNRELATED_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);

    /// Builds a spec whose code hash differs from any pre-existing account, so
    /// `transact_deploy` takes the bytecode-upgrade path rather than the
    /// idempotent early return.
    fn seeded_spec(force_create_on_upgrade: bool) -> SystemContractSpec {
        let code = Bytes::from_static(&[0x60, 0x00]);
        let code_hash = keccak256(&code);
        SystemContractSpec::new(SEEDED_ADDR, code, code_hash)
            .with_seed(Vec::from([(SEED_SLOT, U256::from(42))]))
            .with_force_create_on_upgrade(force_create_on_upgrade)
    }

    /// An in-place upgrade that does NOT recreate the account must not write seed
    /// slots: doing so would mix genesis storage into preserved live storage and
    /// record a wrong `original_value`. Guards the `force_create_on_upgrade =
    /// false` + non-empty `seed` configuration that no current contract uses.
    #[test]
    fn test_seed_skipped_on_storage_preserving_upgrade() {
        let mut db = InMemoryDB::default();
        // Pre-existing account with different bytecode (so the upgrade path runs)
        // and a live value at the slot the spec would otherwise seed.
        let existing_code = Bytes::from_static(&[0xfe]);
        db.insert_account_info(
            SEEDED_ADDR,
            AccountInfo {
                balance: U256::from(1),
                nonce: 1,
                code_hash: keccak256(&existing_code),
                code: Some(Bytecode::new_raw(existing_code)),
            },
        );
        db.insert_account_storage(SEEDED_ADDR, SEED_SLOT, U256::from(99)).unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let result = transact_deploy(&mut state, &seeded_spec(false)).unwrap();
        let account = result.get(&SEEDED_ADDR).unwrap();

        assert!(account.is_touched());
        assert!(!account.is_created(), "storage-preserving upgrade must not recreate");
        assert!(
            !account.storage.contains_key(&SEED_SLOT),
            "seed must not be written when storage is preserved"
        );

        // Committing the upgrade must leave the pre-existing slot at its old value.
        state.commit(result);
        assert_eq!(
            state.storage(SEEDED_ADDR, SEED_SLOT).unwrap(),
            U256::from(99),
            "preserved storage must keep its pre-upgrade value after commit"
        );
    }

    /// The same spec with `force_create_on_upgrade = true` recreates the account
    /// and therefore does seed — the path the `SequencerRegistry` relies on.
    #[test]
    fn test_seed_written_on_force_created_upgrade() {
        let mut db = InMemoryDB::default();
        let existing_code = Bytes::from_static(&[0xfe]);
        db.insert_account_info(
            SEEDED_ADDR,
            AccountInfo {
                balance: U256::from(1),
                nonce: 1,
                code_hash: keccak256(&existing_code),
                code: Some(Bytecode::new_raw(existing_code)),
            },
        );
        // An unrelated live slot that recreation must clear.
        db.insert_account_storage(SEEDED_ADDR, UNRELATED_SLOT, U256::from(99)).unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let result = transact_deploy(&mut state, &seeded_spec(true)).unwrap();
        let account = result.get(&SEEDED_ADDR).unwrap();

        assert!(account.is_created(), "force_create_on_upgrade must recreate");
        let slot = account.storage.get(&SEED_SLOT).expect("seed slot must be written");
        assert_eq!(slot.present_value, U256::from(42));
        assert_eq!(slot.original_value, U256::ZERO);

        // Recreation clears prior storage: after commit the seed slot holds its
        // value and the unrelated pre-existing slot reads back as zero.
        state.commit(result);
        assert_eq!(state.storage(SEEDED_ADDR, SEED_SLOT).unwrap(), U256::from(42));
        assert_eq!(
            state.storage(SEEDED_ADDR, UNRELATED_SLOT).unwrap(),
            U256::ZERO,
            "recreation must clear unrelated pre-existing storage"
        );
    }

    /// A fresh deploy seeds regardless of `force_create_on_upgrade`.
    #[test]
    fn test_seed_written_on_fresh_deploy() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let result = transact_deploy(&mut state, &seeded_spec(false)).unwrap();
        let account = result.get(&SEEDED_ADDR).unwrap();

        assert!(account.is_created(), "fresh deploy creates the account");
        assert_eq!(
            account.storage.get(&SEED_SLOT).expect("seed slot must be written").present_value,
            U256::from(42)
        );
    }

    #[test]
    fn test_registry_oracle_version_by_spec() {
        // Only MiniRex active: Oracle v1.0.0, force-created on upgrade (pre-Rex5),
        // and just Oracle + high-precision Oracle.
        let mini =
            MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
        let mini = flat_system_contract_specs(&mini, 0);
        assert_eq!(mini.len(), 2);
        assert_eq!(mini[0].code_hash, ORACLE_CONTRACT_CODE_HASH);
        assert!(mini[0].force_create_on_upgrade);

        // Rex2 active but not Rex5: Oracle v1.1.0, still force-created on upgrade.
        let rex2 = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(0));
        let rex2 = flat_system_contract_specs(&rex2, 0);
        assert_eq!(rex2[0].code_hash, ORACLE_CONTRACT_CODE_HASH_REX2);
        assert!(rex2[0].force_create_on_upgrade);
    }
}
