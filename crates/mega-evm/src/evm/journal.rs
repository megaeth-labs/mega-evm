//! The journal of the Satin engine: revm's journal behind a wrapper of the engine's own.
//!
//! [`MegaJournal`] owns revm's [`Journal`] and implements [`JournalTr`] by delegating every method
//! to it, the configuration switches included: a switch kept at the trait's no-op default would
//! silently turn a journal feature off. The one default kept is `clear`'s, which finalizes through
//! `finalize`. The impl denies `clippy::missing_trait_methods`, so a method `JournalTr` gains is a
//! lint error here until it is given its rule.
//!
//! [`JournalExt::journal`] hands out revm's own entries, and the journal dereferences to revm's
//! [`Journal`] read-only, so code that reads revm's journal state keeps reading it. There is no
//! mutable dereference: a write goes through [`JournalTr`], where the wrapper sees it.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::ops::Deref;
use std::vec::Vec;

use delegate::delegate;
use revm::{
    context::{
        journaled_state::{
            account::JournaledAccount, AccountInfoLoad, AccountLoad, JournalCheckpoint,
            JournalLoadError, TransferError,
        },
        JournalTr,
    },
    context_interface::context::{SStoreResult, SelfDestructResult, StateLoad},
    inspector::JournalExt,
    primitives::{
        hardfork::SpecId, Address, AddressMap, AddressSet, Bytes, HashSet, Log, StorageKey,
        StorageValue, B256, U256,
    },
    state::{Account, Bytecode, EvmState},
    Database, Journal, JournalEntry,
};

/// The journal of the Satin engine: revm's journal behind a wrapper of the engine's own. See the
/// module documentation.
///
/// It is the journal of every [`MegaContext`](crate::MegaContext). A node that names the
/// context's journal type names this one; one that reads revm's journal state reads it through the
/// dereference, and one that writes to it does so through [`JournalTr`].
#[derive(Clone, Debug)]
pub struct MegaJournal<DB: Database> {
    /// revm's journal, which holds the state.
    revm: Journal<DB>,
}

impl<DB: Database> MegaJournal<DB> {
    /// Consumes the journal and returns revm's.
    pub fn into_revm(self) -> Journal<DB> {
        self.revm
    }
}

impl<DB: Database> Deref for MegaJournal<DB> {
    type Target = Journal<DB>;

    fn deref(&self) -> &Journal<DB> {
        &self.revm
    }
}

#[deny(clippy::missing_trait_methods)]
impl<DB: Database> JournalTr for MegaJournal<DB> {
    type Database = DB;
    type State = EvmState;
    type JournaledAccount<'a>
        = JournaledAccount<'a, DB, JournalEntry>
    where
        DB: 'a;

    fn new(database: DB) -> Self {
        Self { revm: Journal::new(database) }
    }

    delegate! {
        to self.revm {
            fn db_mut(&mut self) -> &mut DB;
            fn db(&self) -> &DB;
            fn evm_state_mut(&mut self) -> &mut EvmState;
            fn evm_state(&self) -> &EvmState;
            fn db_and_state(&self) -> (&DB, &EvmState);
            fn db_and_state_mut(&mut self) -> (&mut DB, &mut EvmState);
            fn sload(
                &mut self,
                address: Address,
                key: StorageKey,
            ) -> Result<StateLoad<StorageValue>, DB::Error>;
            fn sload_skip_cold_load(
                &mut self,
                address: Address,
                key: StorageKey,
                skip_cold_load: bool,
            ) -> Result<StateLoad<StorageValue>, JournalLoadError<DB::Error>>;
            fn sstore(
                &mut self,
                address: Address,
                key: StorageKey,
                value: StorageValue,
            ) -> Result<StateLoad<SStoreResult>, DB::Error>;
            fn sstore_skip_cold_load(
                &mut self,
                address: Address,
                key: StorageKey,
                value: StorageValue,
                skip_cold_load: bool,
            ) -> Result<StateLoad<SStoreResult>, JournalLoadError<DB::Error>>;
            fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue;
            fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue);
            fn log(&mut self, log: Log);
            fn take_logs(&mut self) -> Vec<Log>;
            fn logs(&self) -> &[Log];
            fn selfdestruct(
                &mut self,
                address: Address,
                target: Address,
                skip_cold_load: bool,
            ) -> Result<StateLoad<SelfDestructResult>, JournalLoadError<DB::Error>>;
            fn warm_access_list(&mut self, access_list: AddressMap<HashSet<StorageKey>>);
            fn warm_coinbase_account(&mut self, address: Address);
            fn warm_precompiles(&mut self, addresses: &AddressSet);
            fn precompile_addresses(&self) -> &AddressSet;
            fn set_spec_id(&mut self, spec_id: SpecId);
            fn set_eip7708_config(&mut self, disabled: bool, eip8246_delayed_clear_disabled: bool);
            fn set_amsterdam_eip7708_enabled(&mut self, enabled: bool);
            fn set_legacy_journal_semantics(&mut self, enabled: bool);
            fn touch_account(&mut self, address: Address);
            fn transfer(
                &mut self,
                from: Address,
                to: Address,
                balance: U256,
            ) -> Result<Option<TransferError>, DB::Error>;
            #[allow(deprecated)]
            fn caller_accounting_journal_entry(
                &mut self,
                address: Address,
                old_balance: U256,
                bump_nonce: bool,
            );
            fn balance_incr(&mut self, address: Address, balance: U256) -> Result<(), DB::Error>;
            #[allow(deprecated)]
            fn nonce_bump_journal_entry(&mut self, address: Address);
            fn load_account(&mut self, address: Address) -> Result<StateLoad<&Account>, DB::Error>;
            #[allow(deprecated)]
            fn load_account_code(
                &mut self,
                address: Address,
            ) -> Result<StateLoad<&Account>, DB::Error>;
            fn load_account_with_code(
                &mut self,
                address: Address,
            ) -> Result<StateLoad<&Account>, DB::Error>;
            fn load_account_delegated(
                &mut self,
                address: Address,
            ) -> Result<StateLoad<AccountLoad>, DB::Error>;
            fn load_account_mut(
                &mut self,
                address: Address,
            ) -> Result<StateLoad<Self::JournaledAccount<'_>>, DB::Error>;
            fn load_account_mut_skip_cold_load(
                &mut self,
                address: Address,
                skip_cold_load: bool,
            ) -> Result<StateLoad<Self::JournaledAccount<'_>>, JournalLoadError<DB::Error>>;
            fn load_account_with_code_mut(
                &mut self,
                address: Address,
            ) -> Result<StateLoad<Self::JournaledAccount<'_>>, DB::Error>;
            fn load_account_mut_optional_code(
                &mut self,
                address: Address,
                load_code: bool,
            ) -> Result<StateLoad<Self::JournaledAccount<'_>>, DB::Error>;
            fn set_code_with_hash(&mut self, address: Address, code: Bytecode, hash: B256);
            fn set_code(&mut self, address: Address, code: Bytecode);
            fn code(&mut self, address: Address) -> Result<StateLoad<Bytes>, DB::Error>;
            fn code_hash(&mut self, address: Address) -> Result<StateLoad<B256>, DB::Error>;
            fn depth(&self) -> usize;
            fn transfer_loaded(
                &mut self,
                from: Address,
                to: Address,
                balance: U256,
            ) -> Option<TransferError>;
            fn create_account_checkpoint(
                &mut self,
                caller: Address,
                address: Address,
                balance: U256,
                spec_id: SpecId,
            ) -> Result<JournalCheckpoint, TransferError>;
            fn checkpoint(&mut self) -> JournalCheckpoint;
            fn checkpoint_commit(&mut self);
            fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint);
            fn commit_tx(&mut self);
            fn discard_tx(&mut self);
            fn finalize(&mut self) -> EvmState;
            fn load_account_info_skip_cold_load(
                &mut self,
                address: Address,
                load_code: bool,
                skip_cold_load: bool,
            ) -> Result<AccountInfoLoad<'_>, JournalLoadError<DB::Error>>;
        }
    }

    /// The trait's own default: it finalizes through `finalize`.
    fn clear(&mut self) {
        let _ = self.finalize();
    }
}

impl<DB: Database> JournalExt for MegaJournal<DB> {
    /// revm's own journal entries.
    #[inline]
    fn journal(&self) -> &[JournalEntry] {
        JournalExt::journal(&self.revm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MemoryDatabase;
    use alloy_primitives::address;

    const A: Address = address!("00000000000000000000000000000000000a0002");
    const B: Address = address!("00000000000000000000000000000000000a0003");
    const C: Address = address!("00000000000000000000000000000000000a0004");

    /// Every configuration switch reaches revm's journal: a switch kept at the trait's no-op
    /// default would turn a journal feature off without a word.
    #[test]
    fn test_the_switches_reach_revms_journal() {
        let mut mega = MegaJournal::new(MemoryDatabase::default());
        let mut plain: Journal<MemoryDatabase> = Journal::new(MemoryDatabase::default());
        let precompiles = AddressSet::from_iter([A]);
        let access_list = AddressMap::from_iter([(B, HashSet::from_iter([U256::from(1)]))]);
        for (spec, flag) in [(SpecId::OSAKA, true), (SpecId::CANCUN, false)] {
            mega.set_spec_id(spec);
            plain.set_spec_id(spec);
            mega.set_eip7708_config(flag, !flag);
            plain.set_eip7708_config(flag, !flag);
            mega.set_amsterdam_eip7708_enabled(flag);
            plain.set_amsterdam_eip7708_enabled(flag);
            mega.set_legacy_journal_semantics(flag);
            plain.set_legacy_journal_semantics(flag);
            mega.warm_coinbase_account(C);
            plain.warm_coinbase_account(C);
            mega.warm_precompiles(&precompiles);
            plain.warm_precompiles(&precompiles);
            mega.warm_access_list(access_list.clone());
            plain.warm_access_list(access_list.clone());
            assert_eq!(mega.inner.cfg, plain.inner.cfg, "{spec:?}");
            assert_eq!(mega.inner.warm_addresses, plain.inner.warm_addresses, "{spec:?}");
            assert_eq!(mega.precompile_addresses(), plain.precompile_addresses());
        }
    }

    /// `JournalExt::journal` hands out revm's own entries, the ones an inspector reads after a
    /// write.
    #[test]
    fn test_the_journal_extension_hands_out_revms_entries() {
        let mut journal = MegaJournal::new(MemoryDatabase::default());
        journal.set_spec_id(SpecId::OSAKA);
        journal.load_account(A).unwrap();
        journal.sstore(A, U256::from(1), U256::from(1)).unwrap();
        let entries = JournalExt::journal(&journal);
        assert!(!entries.is_empty());
        assert_eq!(entries, JournalExt::journal(&*journal), "revm's own");
    }
}
