//! Unit tests extracted from `crates/mega-evm/src/evm/host.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/host.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};
    use core::cell::Cell;
    use revm::{
        primitives::HashMap,
        state::{AccountInfo, Bytecode},
        Database,
    };

    /// Minimal `revm::Database` implementation that mimics the production
    /// `reth`-style `StateProviderDatabase` contract: `basic()` returns
    /// `AccountInfo { code: None, code_hash: <real hash> }` for accounts with
    /// on-chain bytecode, and the bytecode itself is lazy-loaded on demand via
    /// `code_by_hash()`. The workspace's `MemoryDatabase` cannot model this —
    /// it eagerly populates `AccountInfo.code` inside `basic()`, so any cache
    /// miss against it would always see the code already hydrated.
    #[derive(Default)]
    struct LazyCodeDatabase {
        accounts: HashMap<Address, AccountInfo>,
        codes: HashMap<B256, Bytecode>,
        storage_calls: Cell<usize>,
    }

    impl LazyCodeDatabase {
        fn with_account_code(mut self, address: Address, bytecode: Bytes) -> Self {
            let code = Bytecode::new_raw(bytecode);
            let code_hash = code.hash_slow();
            self.accounts.insert(
                address,
                AccountInfo { balance: U256::ZERO, nonce: 0, code_hash, code: None },
            );
            self.codes.insert(code_hash, code);
            self
        }

        fn with_eip7702_delegation(mut self, address: Address, delegate: Address) -> Self {
            let code = Bytecode::new_eip7702(delegate);
            let code_hash = code.hash_slow();
            self.accounts.insert(
                address,
                AccountInfo { balance: U256::ZERO, nonce: 0, code_hash, code: None },
            );
            self.codes.insert(code_hash, code);
            self
        }

        fn storage_calls(&self) -> usize {
            self.storage_calls.get()
        }
    }

    impl revm::Database for LazyCodeDatabase {
        type Error = core::convert::Infallible;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            // Mirror reth's `StateProviderDatabase::basic`: return AccountInfo without
            // populating `code`, even when the account has on-chain bytecode.
            Ok(self.accounts.get(&address).cloned())
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(self.codes.get(&code_hash).cloned().unwrap_or_default())
        }

        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            self.storage_calls.set(self.storage_calls.get() + 1);
            Ok(U256::ZERO)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    /// `inspect_account(addr, false)` must not hydrate `info.code` on the vacant
    /// branch — callers that need it pass `load_code = true`.
    #[test]
    fn test_inspect_account_vacant_path_does_not_hydrate_code() {
        const ADDR: Address = address!("00000000000000000000000000000000000000aa");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]); // PUSH1 1 PUSH1 1 ADD
        let expected_hash = keccak256(&bytecode);

        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let account =
            inspect_account(&mut journal, ADDR, false).expect("inspect_account must succeed");

        assert_eq!(
            account.info.code_hash, expected_hash,
            "code_hash must propagate from the database's `basic()` result",
        );
        assert!(
            account.info.code.is_none(),
            "`load_code = false` must leave `info.code` as-is on the vacant branch",
        );
    }

    /// `inspect_account(addr, true)` hydrates `info.code` from `code_by_hash` on
    /// first cold inspection against a lazy-code database.
    #[test]
    fn test_inspect_account_with_load_code_hydrates_lazy_bytecode_on_first_touch() {
        const ADDR: Address = address!("00000000000000000000000000000000000000aa");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);

        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode.clone());
        let mut journal = Journal::new(db);

        let account = inspect_account(&mut journal, ADDR, true)
            .expect("inspect_account must succeed on first cold-touch");
        let hydrated = account
            .info
            .code
            .as_ref()
            .expect("`load_code = true` must populate `info.code` from code_by_hash");
        assert_eq!(
            hydrated.original_bytes().as_ref(),
            bytecode.as_ref(),
            "hydrated bytecode must match what `code_by_hash` would return",
        );
    }

    /// The occupied-branch hydration must keep firing — a second `inspect_account`
    /// against the same lazy-DB address must observe hydrated `info.code` even with
    /// `load_code = false`.
    #[test]
    fn test_inspect_account_occupied_branch_hydrates_on_second_inspection() {
        const ADDR: Address = address!("00000000000000000000000000000000000000bb");
        let bytecode = Bytes::from_static(&[0x5b]); // JUMPDEST
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let first_code_hash = inspect_account(&mut journal, ADDR, false)
            .expect("first inspection must succeed")
            .info
            .code_hash;
        let second =
            inspect_account(&mut journal, ADDR, false).expect("second inspection must succeed");

        assert_eq!(
            second.info.code_hash, first_code_hash,
            "code_hash must be identical across cache miss and cache hit",
        );
        assert!(
            second.info.code.is_some(),
            "second inspection must observe the hydrated code via the occupied-branch \
             `code_by_hash` load",
        );
    }

    /// `inspect_account(addr, true)` must short-circuit on EOAs — the `code_hash !=
    /// KECCAK_EMPTY` guard keeps `code_by_hash` off the hot path.
    #[test]
    fn test_inspect_account_with_load_code_leaves_eoa_code_empty() {
        const EOA: Address = address!("00000000000000000000000000000000000000cc");

        let mut db = LazyCodeDatabase::default();
        db.accounts.insert(
            EOA,
            AccountInfo {
                balance: U256::from(1_000_000u64),
                nonce: 5,
                code_hash: KECCAK_EMPTY,
                code: None,
            },
        );
        let mut journal = Journal::new(db);

        let account = inspect_account(&mut journal, EOA, true)
            .expect("inspect_account must succeed and be a no-op on EOAs");
        assert_eq!(account.info.code_hash, KECCAK_EMPTY, "EOA code_hash must remain KECCAK_EMPTY");
        assert!(
            account.info.code.is_none(),
            "EOA code must stay `None`; the `code_hash != KECCAK_EMPTY` guard keeps \
             `code_by_hash` off the hot path for accounts without on-chain code",
        );
    }

    /// `inspect_account_code_hash` returns the account's `code_hash` but must NEVER hydrate
    /// `info.code` — on either branch. In particular the occupied branch must not fall through to
    /// `code_by_hash` the way `inspect_account(.., false)` does (see
    /// `test_inspect_account_occupied_branch_hydrates_on_second_inspection`). This is what lets the
    /// REX6 keyless-deploy occupancy check decide on the hash alone, without demanding an
    /// already-warmed occupied address's bytecode in a stateless witness carrying only its proof.
    #[test]
    fn test_inspect_account_code_hash_never_hydrates_code() {
        const ADDR: Address = address!("00000000000000000000000000000000000000dd");
        let bytecode = Bytes::from_static(&[0x5b]); // JUMPDEST
        let expected_hash = keccak256(&bytecode);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        // Vacant cache-miss: returns the hash from `basic()` without hydrating code.
        let vacant_hash =
            inspect_account_code_hash(&mut journal, ADDR).expect("vacant read must succeed");
        assert_eq!(vacant_hash, expected_hash, "vacant branch must return the code_hash");
        assert!(
            journal.inner.state.get(&ADDR).is_some_and(|a| a.info.code.is_none()),
            "vacant branch must not hydrate info.code",
        );

        // The address is now resident with `code == None`. `inspect_account` would hydrate on this
        // occupied branch; `inspect_account_code_hash` must not.
        let occupied_hash =
            inspect_account_code_hash(&mut journal, ADDR).expect("occupied read must succeed");
        assert_eq!(
            occupied_hash, expected_hash,
            "occupied branch must return the cached code_hash"
        );
        assert!(
            journal.inner.state.get(&ADDR).is_some_and(|a| a.info.code.is_none()),
            "inspect_account_code_hash must NOT hydrate info.code on the occupied branch",
        );
    }

    /// On REX5+, `inspect_account_delegated` must follow the EIP-7702 hop on the
    /// very first cold inspection against a lazy-code database. Regression guard:
    /// any refactor that re-introduces a code-None branch silently degrades the
    /// walk to "treat the delegator as a regular EOA".
    #[test]
    fn test_inspect_account_delegated_follows_eip7702_on_cold_first_touch() {
        use revm::context::JournalTr;

        const DELEGATOR: Address = address!("00000000000000000000000000000000000000d1");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000d2");
        let delegate_bytecode = Bytes::from_static(&[0x60, 0x42, 0x60, 0x00, 0x55]); // PUSH1 0x42 PUSH1 0 SSTORE

        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(DELEGATOR, DELEGATE)
            .with_account_code(DELEGATE, delegate_bytecode.clone());

        let mut journal = Journal::new(db);

        let resolved = journal
            .inspect_account_delegated(MegaSpecId::REX5, DELEGATOR)
            .expect("inspect_account_delegated must succeed on a cold-cache first touch");

        // The resolved account must be the delegate, not the delegator. The only way
        // to distinguish them is the code: the delegator's code is the EIP-7702
        // designation pointing at DELEGATE; the delegate's code is the raw bytecode.
        let hydrated = resolved.info.code.as_ref().expect(
            "delegate's bytecode must be hydrated by the inner inspect_account call — \
             without the vacant-path hydration, the cold-touch on DELEGATE would leave \
             code as None and any subsequent EIP-7702 walk would see a wrongly-empty target",
        );
        assert!(
            !matches!(hydrated, Bytecode::Eip7702(_)),
            "resolved account must NOT be the delegator (whose code is the EIP-7702 \
             designation); got: {hydrated:?}",
        );
        assert_eq!(
            hydrated.original_bytes().as_ref(),
            delegate_bytecode.as_ref(),
            "resolved account's code must match the delegate's raw bytecode — confirms \
             the delegation was followed exactly one hop",
        );
    }

    /// Pins the `LazyCodeDatabase` fixture's contract against the production
    /// `revm::Database` shape it is modeling: `basic()` returns
    /// `code: None` for known accounts, `None` for unknown addresses;
    /// `code_by_hash()` falls back to empty bytecode for an unknown hash;
    /// `storage()` and `block_hash()` are inert stubs (no tests exercise them via
    /// `inspect_account`, but they must remain wired so the fixture is a complete
    /// `revm::Database`). If the fixture ever drifts (e.g. someone "helpfully"
    /// makes `basic()` eagerly populate `code` like `MemoryDatabase` does), the
    /// inspect-account tests above silently lose their load-bearing property —
    /// this test fails fast in that case.
    #[test]
    fn test_lazy_code_database_fixture_pins_reth_style_contract() {
        const KNOWN: Address = address!("00000000000000000000000000000000000000ee");
        let bytecode = Bytes::from_static(&[0x00]);
        let mut db = LazyCodeDatabase::default().with_account_code(KNOWN, bytecode);

        let known = db.basic(KNOWN).unwrap().expect("known account must resolve");
        assert!(
            known.code.is_none(),
            "LazyCodeDatabase::basic must NOT pre-populate code — that is the \
             behavior `inspect_account` is being tested against",
        );
        assert!(
            db.basic(Address::ZERO).unwrap().is_none(),
            "unknown address must return None from basic()",
        );

        let unknown_hash = keccak256([0xffu8]);
        assert_eq!(
            db.code_by_hash(unknown_hash).unwrap().original_bytes().len(),
            0,
            "unknown code_hash must fall back to empty bytecode",
        );

        assert_eq!(db.storage(KNOWN, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(db.block_hash(0).unwrap(), B256::ZERO);
    }

    // === inspect_storage coverage tests (PR #334) ===

    #[test]
    fn test_inspect_storage_rex4_slot_hit_returns_existing_value() {
        const ADDR: Address = address!("00000000000000000000000000000000000000bb");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let key = U256::from(3);
        let expected_value = U256::from(42);
        {
            let tid = journal.transaction_id;
            let account = inspect_account(&mut journal, ADDR, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected_value, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let spec = MegaSpecId::REX4;
        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on existing slot");

        assert_eq!(
            slot.present_value, expected_value,
            "REX4 slot hit must return the pre-seeded value"
        );
        assert!(slot.is_cold, "inspected slot must remain cold");
    }

    #[test]
    fn test_inspect_storage_rex4_slot_miss_inserts_and_returns_db_value() {
        const ADDR: Address = address!("00000000000000000000000000000000000000cc");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let key = U256::from(7);
        let spec = MegaSpecId::REX4;

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on absent slot");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "absent slot on non-created account must return ZERO from database"
        );
        assert!(slot.is_cold, "newly inserted slot must be marked cold");

        let calls_after_first = journal.database.storage_calls();
        let slot2 =
            journal.inspect_storage(spec, ADDR, key).expect("second inspect_storage must succeed");

        assert_eq!(slot2.present_value, U256::ZERO, "second call must return the same value");
        assert_eq!(
            journal.database.storage_calls(),
            calls_after_first,
            "second inspect_storage on the same slot must hit the cache, not the DB",
        );
    }

    #[test]
    fn test_inspect_storage_rex4_newly_created_short_circuits_db() {
        const ADDR: Address = address!("00000000000000000000000000000000000000dd");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        {
            let account = inspect_account(&mut journal, ADDR, false).unwrap();
            account.mark_created();
        }

        let key = U256::from(1);
        let spec = MegaSpecId::REX4;

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on newly-created account");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "newly-created account must return ZERO without querying database"
        );
        assert!(slot.is_cold, "slot must be marked cold");
    }

    #[test]
    fn test_inspect_storage_rex4_ignores_eip7702_delegation() {
        const DELEGATOR: Address = address!("0000000000000000000000000000000000000d01");
        const DELEGATE: Address = address!("0000000000000000000000000000000000000d02");
        let delegate_code = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(DELEGATOR, DELEGATE)
            .with_account_code(DELEGATE, delegate_code);
        let mut journal = Journal::new(db);

        let key = U256::from(2);
        let expected = U256::from(99);
        {
            let tid = journal.transaction_id;
            let account = inspect_account(&mut journal, DELEGATOR, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let spec = MegaSpecId::REX4;
        let slot = journal
            .inspect_storage(spec, DELEGATOR, key)
            .expect("inspect_storage must succeed for REX4 delegator");

        assert_eq!(
            slot.present_value, expected,
            "REX4 must read storage from delegator (original address), not delegate"
        );
    }
}
