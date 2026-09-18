//! The Host of the Satin engine, and the journal reads that observe without warming.
//!
//! # The Host only observes
//!
//! revm hands the Host the facts of every state-writing opcode: an `SSTORE`'s original, present and
//! new values, a log's topics and data, a `SELFDESTRUCT`'s balance and beneficiary. The Host
//! stages them ([`AdditionalLimit::stage_record`](crate::AdditionalLimit)) and records nothing.
//! The opcode's wrapper commits the staged record after the opcode completed, and discards it when
//! the opcode failed. Recording in the Host would count a write the opcode's own failure then takes
//! back: `SSTORE` charges its dynamic gas after the Host call, and an out-of-gas there halts the
//! frame with the record already counted.
//!
//! Every other Host method delegates to op-revm's context.

use alloy_primitives::map::Entry;
use delegate::delegate;
use revm::{
    context_interface::{
        cfg::{GasId, GasParams, StateGasCharge, StateGasSite},
        context::{SStoreResult, SelfDestructResult, StateLoad},
        host::LoadError,
        journaled_state::{AccountInfoLoad, AccountLoad},
    },
    primitives::{Address, Bytes, Log, StorageKey, StorageValue, B256, KECCAK_EMPTY, U256},
    state::{Account, EvmStorageSlot},
    Database, Journal,
};

use crate::{ExternalEnvTypes, MegaContext, StagedRecord};

impl<DB: Database, ExtEnvs: ExternalEnvTypes> revm::context_interface::Host
    for MegaContext<DB, ExtEnvs>
{
    delegate! {
        to self.inner {
            fn basefee(&self) -> U256;
            fn blob_gasprice(&self) -> U256;
            fn gas_limit(&self) -> U256;
            fn difficulty(&self) -> U256;
            fn prevrandao(&self) -> Option<U256>;
            fn block_number(&self) -> U256;
            fn timestamp(&self) -> U256;
            fn beneficiary(&self) -> Address;
            fn slot_num(&self) -> U256;
            fn chain_id(&self) -> U256;
            fn effective_gas_price(&self) -> U256;
            fn caller(&self) -> Address;
            fn blob_hash(&self, number: usize) -> Option<U256>;
            fn max_initcode_size(&self) -> usize;
            fn gas_params(&self) -> &GasParams;
            fn is_amsterdam_eip8037_enabled(&self) -> bool;
            fn state_gas_price(&mut self, id: GasId, site: StateGasSite) -> Option<u64>;
            fn state_gas_charge(&mut self, charge: StateGasCharge) -> Option<u64>;
            fn block_hash(&mut self, number: u64) -> Option<B256>;
            fn sload_skip_cold_load(
                &mut self,
                address: Address,
                key: StorageKey,
                skip_cold_load: bool,
            ) -> Result<StateLoad<StorageValue>, LoadError>;
            fn sload(&mut self, address: Address, key: StorageKey) -> Option<StateLoad<StorageValue>>;
            fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue);
            fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue;
            fn load_account_info_skip_cold_load(
                &mut self,
                address: Address,
                load_code: bool,
                skip_cold_load: bool,
            ) -> Result<AccountInfoLoad<'_>, LoadError>;
            fn balance(&mut self, address: Address) -> Option<StateLoad<U256>>;
            fn load_account_delegated(&mut self, address: Address) -> Option<StateLoad<AccountLoad>>;
            fn load_account_code(&mut self, address: Address) -> Option<StateLoad<Bytes>>;
            fn load_account_code_hash(&mut self, address: Address) -> Option<StateLoad<B256>>;
        }
    }

    /// Writes the slot and stages the write's values.
    #[inline]
    fn sstore_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SStoreResult>, LoadError> {
        let load = self.inner.sstore_skip_cold_load(address, key, value, skip_cold_load)?;
        self.additional_limit.stage_record(StagedRecord::Sstore(load.data.clone()));
        Ok(load)
    }

    /// Writes the slot and stages the write's values.
    #[inline]
    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Option<StateLoad<SStoreResult>> {
        let load = self.inner.sstore(address, key, value)?;
        self.additional_limit.stage_record(StagedRecord::Sstore(load.data.clone()));
        Some(load)
    }

    /// Stages the log's topic count and data length, then appends it.
    #[inline]
    fn log(&mut self, log: Log) {
        self.additional_limit.stage_record(StagedRecord::Log {
            topics: log.topics().len() as u8,
            data_len: log.data.data.len() as u64,
        });
        self.inner.log(log);
    }

    /// Destructs the account and stages whether value moved and where to.
    #[inline]
    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SelfDestructResult>, LoadError> {
        let load = self.inner.selfdestruct(address, target, skip_cold_load)?;
        self.additional_limit.stage_record(StagedRecord::SelfDestruct {
            had_value: load.data.had_value,
            target_exists: load.data.target_exists,
            to_other_account: target != address,
            beneficiary: target,
        });
        Ok(load)
    }
}

/// Journal reads that observe an account or a slot without warming it.
///
/// A read loads what the journal does not hold yet into it, so the returned state and a
/// stateless witness carry it, and marks it cold, so it neither enters the EIP-2929 access list
/// nor leaves a journal entry. Execution that later touches the same account or slot pays for it
/// as if the read never happened.
pub trait JournalInspectTr {
    /// The database error.
    type DBError;

    /// The account at `address`, without following an EIP-7702 delegation.
    ///
    /// With `load_code`, the code of an account the database served without it is loaded. An
    /// account the journal already holds always has its code loaded.
    fn inspect_account(
        &mut self,
        address: Address,
        load_code: bool,
    ) -> Result<&mut Account, Self::DBError>;

    /// The code hash of the account at `address`. Never loads the code, so an occupancy check
    /// needs only the account in a witness, not its bytecode.
    fn inspect_account_code_hash(&mut self, address: Address) -> Result<B256, Self::DBError>;

    /// The slot `key` of the account at `address`, which is always the account's own storage (an
    /// EIP-7702 delegation does not move it). A slot of an account created in this transaction
    /// is zero without a database read.
    fn inspect_storage(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError>;
}

impl<DB: Database> JournalInspectTr for Journal<DB> {
    type DBError = DB::Error;

    fn inspect_account(
        &mut self,
        address: Address,
        load_code: bool,
    ) -> Result<&mut Account, DB::Error> {
        let transaction_id = self.transaction_id;
        match self.inner.state.entry(address) {
            Entry::Occupied(entry) => {
                let account = entry.into_mut();
                if account.info.code_hash != KECCAK_EMPTY && account.info.code.is_none() {
                    account.info.code = Some(self.database.code_by_hash(account.info.code_hash)?);
                }
                Ok(account)
            }
            Entry::Vacant(entry) => {
                let mut account = self
                    .database
                    .basic(address)?
                    .map(Account::from)
                    .unwrap_or_else(|| Account::new_not_existing(transaction_id));
                if load_code &&
                    account.info.code_hash != KECCAK_EMPTY &&
                    account.info.code.is_none()
                {
                    account.info.code = Some(self.database.code_by_hash(account.info.code_hash)?);
                }
                account.mark_cold();
                Ok(entry.insert(account))
            }
        }
    }

    fn inspect_account_code_hash(&mut self, address: Address) -> Result<B256, DB::Error> {
        let transaction_id = self.transaction_id;
        match self.inner.state.entry(address) {
            Entry::Occupied(entry) => Ok(entry.get().info.code_hash),
            Entry::Vacant(entry) => {
                let mut account = self
                    .database
                    .basic(address)?
                    .map(Account::from)
                    .unwrap_or_else(|| Account::new_not_existing(transaction_id));
                account.mark_cold();
                Ok(entry.insert(account).info.code_hash)
            }
        }
    }

    fn inspect_storage(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, DB::Error> {
        let transaction_id = self.transaction_id;
        let is_newly_created = self.inspect_account(address, true)?.is_created();
        let account = self.inner.state.get_mut(&address).expect("inspected above");
        match account.storage.entry(key) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let value = if is_newly_created {
                    U256::ZERO
                } else {
                    self.database.storage(address, key)?
                };
                let mut slot = EvmStorageSlot::new(value, transaction_id);
                slot.mark_cold();
                Ok(entry.insert(slot))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};
    use core::cell::Cell;
    use revm::{
        context::JournalTr,
        primitives::HashMap,
        state::{AccountInfo, Bytecode},
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
            self.accounts
                .insert(address, AccountInfo { code_hash, code: None, ..Default::default() });
            self.codes.insert(code_hash, code);
            self
        }

        fn with_eip7702_delegation(mut self, address: Address, delegate: Address) -> Self {
            let code = Bytecode::new_eip7702(delegate);
            let code_hash = code.hash_slow();
            self.accounts
                .insert(address, AccountInfo { code_hash, code: None, ..Default::default() });
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

        let account = journal.inspect_account(ADDR, false).expect("inspect_account must succeed");

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

        let account = journal
            .inspect_account(ADDR, true)
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

        let first_code_hash = journal
            .inspect_account(ADDR, false)
            .expect("first inspection must succeed")
            .info
            .code_hash;
        let second = journal.inspect_account(ADDR, false).expect("second inspection must succeed");

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
                ..Default::default()
            },
        );
        let mut journal = Journal::new(db);

        let account = journal
            .inspect_account(EOA, true)
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
    /// `test_inspect_account_occupied_branch_hydrates_on_second_inspection`). This is what lets an
    /// occupancy check decide on the hash alone, without demanding an already-warmed occupied
    /// address's bytecode in a stateless witness carrying only its proof.
    #[test]
    fn test_inspect_account_code_hash_never_hydrates_code() {
        const ADDR: Address = address!("00000000000000000000000000000000000000dd");
        let bytecode = Bytes::from_static(&[0x5b]); // JUMPDEST
        let expected_hash = keccak256(&bytecode);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        // Vacant cache-miss: returns the hash from `basic()` without hydrating code.
        let vacant_hash =
            journal.inspect_account_code_hash(ADDR).expect("vacant read must succeed");
        assert_eq!(vacant_hash, expected_hash, "vacant branch must return the code_hash");
        assert!(
            journal.inner.state.get(&ADDR).is_some_and(|a| a.info.code.is_none()),
            "vacant branch must not hydrate info.code",
        );

        // The address is now resident with `code == None`. `inspect_account` would hydrate on this
        // occupied branch; `inspect_account_code_hash` must not.
        let occupied_hash =
            journal.inspect_account_code_hash(ADDR).expect("occupied read must succeed");
        assert_eq!(
            occupied_hash, expected_hash,
            "occupied branch must return the cached code_hash"
        );
        assert!(
            journal.inner.state.get(&ADDR).is_some_and(|a| a.info.code.is_none()),
            "inspect_account_code_hash must NOT hydrate info.code on the occupied branch",
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

    #[test]
    fn test_inspect_storage_slot_hit_returns_existing_value() {
        const ADDR: Address = address!("00000000000000000000000000000000000000bb");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let key = U256::from(3);
        let expected_value = U256::from(42);
        {
            let tid = journal.transaction_id;
            let account = journal.inspect_account(ADDR, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected_value, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let slot = journal
            .inspect_storage(ADDR, key)
            .expect("inspect_storage must succeed on existing slot");

        assert_eq!(
            slot.present_value, expected_value,
            "a slot hit must return the pre-seeded value"
        );
        assert!(slot.is_cold, "inspected slot must remain cold");
    }

    #[test]
    fn test_inspect_storage_slot_miss_inserts_and_returns_db_value() {
        const ADDR: Address = address!("00000000000000000000000000000000000000cc");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let key = U256::from(7);

        let slot = journal
            .inspect_storage(ADDR, key)
            .expect("inspect_storage must succeed on absent slot");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "absent slot on non-created account must return ZERO from database"
        );
        assert!(slot.is_cold, "newly inserted slot must be marked cold");

        let calls_after_first = journal.database.storage_calls();
        let slot2 =
            journal.inspect_storage(ADDR, key).expect("second inspect_storage must succeed");

        assert_eq!(slot2.present_value, U256::ZERO, "second call must return the same value");
        assert_eq!(
            journal.database.storage_calls(),
            calls_after_first,
            "second inspect_storage on the same slot must hit the cache, not the DB",
        );
    }

    #[test]
    fn test_inspect_storage_newly_created_short_circuits_db() {
        const ADDR: Address = address!("00000000000000000000000000000000000000dd");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        {
            let account = journal.inspect_account(ADDR, false).unwrap();
            account.mark_created();
        }

        let key = U256::from(1);

        let slot = journal
            .inspect_storage(ADDR, key)
            .expect("inspect_storage must succeed on newly-created account");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "newly-created account must return ZERO without querying database"
        );
        assert!(slot.is_cold, "slot must be marked cold");
    }

    #[test]
    fn test_inspect_storage_ignores_eip7702_delegation() {
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
            let account = journal.inspect_account(DELEGATOR, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let slot = journal
            .inspect_storage(DELEGATOR, key)
            .expect("inspect_storage must succeed for a delegator");

        assert_eq!(
            slot.present_value, expected,
            "storage is read from the delegator (original address), not the delegate"
        );
    }
}
