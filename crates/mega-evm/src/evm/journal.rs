//! The journal of the Satin engine: revm's journal, with the ledger of what the transaction's own
//! execution keeps beside it.
//!
//! [`MegaJournal`] owns revm's [`Journal`] and implements [`JournalTr`] by delegating every method
//! to it. A handful of methods also tell the ledger what they did, under one rule:
//!
//! - **Counted writes.** A call's value transfer (`transfer_loaded`) and a creation's account
//!   checkpoint (`create_account_checkpoint`) are counted when revm makes them for a frame start,
//!   inside the start window the engine opens around revm's frame start, and only there. A
//!   `SELFDESTRUCT`'s move (`selfdestruct`) is observed here and counted by the Host's
//!   completed-write hook once the opcode completed. Every other write — `sstore`, `log`, a
//!   `transfer`, anything through an account handle — is forwarded and counts nothing: a write an
//!   opcode completed reaches the ledger from the Host's completed-write hook, and a write a tool
//!   or a precompile makes is not the transaction's own execution.
//! - **What a write counts.** An account's write record follows the net change of its info within
//!   the transaction: the journal compares the account's nonce, balance and code hash with what the
//!   transaction loaded ([`is_changed`](MegaJournal::is_changed)) before and after each counted
//!   write, and tells the ledger which accounts the write changed and which it returned
//!   ([`RecordDelta`]). The same prediction, on the journal's state as it stands, is what the
//!   frame-start hook charges a start's records from
//!   ([`start_records`](MegaJournal::start_records)).
//! - **Levels and the transaction's end.** `checkpoint`, `checkpoint_commit` and
//!   `checkpoint_revert` open and close the ledger's levels with revm's checkpoints; `commit_tx`,
//!   `discard_tx` and `finalize` close them all.
//! - **Everything else is forwarded**, the configuration switches included: a switch kept at the
//!   trait's no-op default would silently turn a journal feature off. The one default kept is
//!   `clear`'s, which finalizes through the hooked `finalize`.
//!
//! The impl denies `clippy::missing_trait_methods`, so a method `JournalTr` gains is a lint error
//! here until it is given its rule.
//!
//! [`JournalExt::journal`] hands out revm's own entries, and the journal dereferences to revm's
//! [`Journal`] read-only, so code that reads revm's journal state keeps reading it. There is no
//! mutable dereference: a write that bypassed the methods above could not be seen by the ledger.

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
    context_interface::{
        context::{SStoreResult, SelfDestructResult, StateLoad},
        host::{FrameStart, FrameStartKind},
    },
    inspector::JournalExt,
    primitives::{
        hardfork::SpecId, Address, AddressMap, AddressSet, Bytes, HashSet, Log, StorageKey,
        StorageValue, B256, U256,
    },
    state::{Account, AccountInfo, Bytecode, EvmState},
    Database, Journal, JournalEntry,
};

use crate::limit::{FrameStartRecords, Ledger, Mark, RecordDelta};

/// The journal of the Satin engine: revm's journal, and the ledger of what the transaction's own
/// execution keeps. See the module documentation.
///
/// It is the journal of every [`MegaContext`](crate::MegaContext). A node that names the
/// context's journal type names this one; one that reads revm's journal state reads it through the
/// dereference, and one that writes to it does so through [`JournalTr`].
#[derive(Clone, Debug)]
pub struct MegaJournal<DB: Database> {
    /// revm's journal, which holds the state.
    revm: Journal<DB>,
    /// The ledger, kept in step with `revm`'s checkpoints.
    ledger: Ledger,
}

/// Whether `info` differs from `original` in what a write record stands for: the nonce, the
/// balance or the code.
fn differs(info: &AccountInfo, original: &AccountInfo) -> bool {
    info.nonce != original.nonce ||
        info.balance != original.balance ||
        info.code_hash != original.code_hash
}

impl<DB: Database> MegaJournal<DB> {
    /// The ledger.
    pub(crate) const fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The ledger, mutably.
    pub(crate) const fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    /// Consumes the journal and returns revm's.
    pub(crate) fn into_revm(self) -> Journal<DB> {
        self.revm
    }

    /// Marks where the journal and the ledger stand, without changing the journal's depth.
    pub(crate) fn mark(&mut self) -> Mark {
        let position = self.revm.checkpoint();
        self.revm.checkpoint_commit();
        self.ledger.mark(position)
    }

    /// Takes back every journal entry, log and self-destruct made since `mark`, as revm takes back
    /// a frame that fails, and the ledger with them; the journal's depth is left as it is.
    pub(crate) fn revert_to(&mut self, mark: &Mark) {
        let _ = self.revm.checkpoint();
        self.revm.checkpoint_revert(mark.position);
        self.ledger.revert_to(mark);
    }

    /// Asserts the ledger holds a level for every checkpoint revm's journal holds open.
    #[inline]
    fn assert_levels(&self) {
        debug_assert!(
            self.ledger.depth() == self.revm.depth,
            "the ledger holds one level per open checkpoint"
        );
    }

    /* What a write changes */

    /// The account at `address` as the transaction loaded it and holds it now: `None` for one
    /// the transaction has not loaded, which it has not changed either.
    fn loaded(&self, address: Address) -> Option<&Account> {
        self.revm
            .state
            .get(&address)
            .filter(|account| account.transaction_id == self.revm.transaction_id)
    }

    /// Whether the transaction changed the account at `address` from what it loaded: its nonce,
    /// balance or code differs from the account's original info, which revm sets at the account's
    /// first load in the transaction. An account the transaction has not loaded is unchanged.
    pub(crate) fn is_changed(&self, address: Address) -> bool {
        self.loaded(address).is_some_and(|account| differs(&account.info, &account.original_info()))
    }

    /// Whether the transaction changed the account at `address` from what it loaded, before the
    /// nonce bump revm just made on it: the creator of a creation, whose nonce revm bumps before
    /// it asks for the creation's account checkpoint.
    fn is_changed_before_bump(&self, address: Address) -> bool {
        self.loaded(address).is_some_and(|account| {
            let before =
                AccountInfo { nonce: account.info.nonce.wrapping_sub(1), ..account.info.clone() };
            differs(&before, &account.original_info())
        })
    }

    /// How the record of the account at `address` moves when its balance becomes `balance`: a
    /// record when the write changes an account the transaction had not changed, a record back when
    /// it returns a changed one to what the transaction loaded.
    fn balance_change(&self, address: Address, balance: U256) -> RecordDelta {
        let Some(account) = self.loaded(address) else {
            // An account the transaction has not loaded is unchanged, and a balance it did not
            // hold changes it: what the transaction loads for it will be what the chain holds.
            return RecordDelta::of_account(false, balance != U256::ZERO);
        };
        let original = account.original_info();
        let after = AccountInfo { balance, ..account.info.clone() };
        RecordDelta::of_account(differs(&account.info, &original), differs(&after, &original))
    }

    /// The current balance of the account at `address`, as the transaction holds it; nothing for
    /// one it has not loaded.
    fn balance(&self, address: Address) -> U256 {
        self.loaded(address).map_or(U256::ZERO, |account| account.info.balance)
    }

    /// The records a transfer of `value` from `from` to `to` makes and takes back, as the journal's
    /// state stands: nothing for a transfer to the same account or of nothing, which revm moves
    /// nothing for, and nothing for a value `from` cannot fund, which revm refuses.
    pub(crate) fn transfer_records(&self, from: Address, to: Address, value: U256) -> RecordDelta {
        if from == to || value.is_zero() {
            return RecordDelta::NONE;
        }
        let Some(from_after) = self.balance(from).checked_sub(value) else {
            return RecordDelta::NONE;
        };
        let to_after = self.balance(to).saturating_add(value);
        self.balance_change(from, from_after).with(self.balance_change(to, to_after))
    }

    /// The records a creation by `creator` with an endowment of `value` makes: the account it
    /// creates, and the creator's nonce, when the bump changes an account the transaction had not
    /// changed; nothing for an endowment `creator` cannot fund, which revm refuses before it bumps
    /// the nonce.
    pub(crate) fn creation_records(&self, creator: Address, value: U256) -> FrameStartRecords {
        if self.balance(creator) < value {
            return FrameStartRecords::NONE;
        }
        FrameStartRecords {
            on_level: RecordDelta { added: 1, taken: 0 },
            creator: !self.is_changed(creator),
        }
    }

    /// The records the frame `start` makes, for the frame-start hook to charge its caller for: a
    /// call's value transfer ([`transfer_records`](Self::transfer_records)), a creation's created
    /// account and creator nonce ([`creation_records`](Self::creation_records)). The transfer of a
    /// `CALLCODE` is from the calling account to itself, and a `DELEGATECALL` or `STATICCALL`
    /// carries none: neither makes a record. Nor does a start whose value the caller cannot fund,
    /// which revm refuses on the caller's account.
    pub(crate) fn start_records(&self, start: &FrameStart) -> FrameStartRecords {
        match start.kind {
            FrameStartKind::Create | FrameStartKind::Create2 => {
                self.creation_records(start.caller, start.value)
            }
            FrameStartKind::Call |
            FrameStartKind::CallCode |
            FrameStartKind::DelegateCall |
            FrameStartKind::StaticCall => FrameStartRecords {
                on_level: start.target.map_or(RecordDelta::NONE, |target| {
                    self.transfer_records(start.caller, target, start.value)
                }),
                creator: false,
            },
        }
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
        Self { revm: Journal::new(database), ledger: Ledger::default() }
    }

    // Forwarded: reads, transient storage, the switches, account handles, and the writes the
    // ledger does not count here. A write an opcode completed is counted by the Host's
    // completed-write hook; any other is not the transaction's own execution.
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
            fn warm_access_list(&mut self, access_list: AddressMap<HashSet<StorageKey>>);
            fn warm_coinbase_account(&mut self, address: Address);
            fn warm_precompiles(&mut self, addresses: &AddressSet);
            fn precompile_addresses(&self) -> &AddressSet;
            fn set_spec_id(&mut self, spec_id: SpecId);
            fn set_eip7708_config(&mut self, disabled: bool, eip8246_delayed_clear_disabled: bool);
            fn set_amsterdam_eip7708_enabled(&mut self, enabled: bool);
            fn set_legacy_journal_semantics(&mut self, enabled: bool);
            fn touch_account(&mut self, address: Address);
            // A transfer that is not a frame start's: revm starts frames with `transfer_loaded`.
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
            fn load_account_info_skip_cold_load(
                &mut self,
                address: Address,
                load_code: bool,
                skip_cold_load: bool,
            ) -> Result<AccountInfoLoad<'_>, JournalLoadError<DB::Error>>;
        }
    }

    /// Counted inside a call's start window: the records the transfer makes and takes back, as
    /// the accounts it writes stand before and after it, and its transfer log.
    fn transfer_loaded(
        &mut self,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Option<TransferError> {
        let moves = from != to && !balance.is_zero();
        let before = moves.then(|| (self.is_changed(from), self.is_changed(to)));
        let logs = self.revm.logs().len();
        let refused = self.revm.transfer_loaded(from, to, balance);
        let logged = self.revm.logs().len() > logs;
        let records = match before.filter(|_| refused.is_none()) {
            Some((from_before, to_before)) => {
                RecordDelta::of_account(from_before, self.is_changed(from))
                    .with(RecordDelta::of_account(to_before, self.is_changed(to)))
            }
            None => RecordDelta::NONE,
        };
        self.ledger.on_transfer(records, refused.is_some(), logged);
        refused
    }

    /// Counted inside a creation's start window: the creator's nonce record before the creation's
    /// level opens, when the bump revm made changed an account the transaction had not changed,
    /// then the created account and the transfer log on it.
    fn create_account_checkpoint(
        &mut self,
        caller: Address,
        address: Address,
        balance: U256,
        spec_id: SpecId,
    ) -> Result<JournalCheckpoint, TransferError> {
        let start = self.ledger.before_creation(!self.is_changed_before_bump(caller));
        let logs = self.revm.logs().len();
        let opened = self.revm.create_account_checkpoint(caller, address, balance, spec_id);
        let logged = self.revm.logs().len() > logs;
        self.ledger.after_creation(start, opened.ok(), logged);
        self.assert_levels();
        opened
    }

    /// Observed for the completed-write hook: the records the destruction makes and takes back, as
    /// the destructed account and its beneficiary stand before and after the move. The hook counts
    /// them once the opcode completed; the write itself counts nothing here.
    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SelfDestructResult>, JournalLoadError<DB::Error>> {
        let before = (self.is_changed(address), self.is_changed(target));
        let result = self.revm.selfdestruct(address, target, skip_cold_load);
        let records = if result.is_ok() {
            RecordDelta::of_account(before.0, self.is_changed(address))
                .with(RecordDelta::of_account(before.1, self.is_changed(target)))
        } else {
            RecordDelta::NONE
        };
        self.ledger.observe_destruction(records);
        result
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        let checkpoint = self.revm.checkpoint();
        self.ledger.on_checkpoint(checkpoint);
        self.assert_levels();
        checkpoint
    }

    fn checkpoint_commit(&mut self) {
        self.revm.checkpoint_commit();
        self.ledger.on_commit();
        self.assert_levels();
    }

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.revm.checkpoint_revert(checkpoint);
        self.ledger.on_revert(checkpoint);
        self.assert_levels();
    }

    fn commit_tx(&mut self) {
        self.revm.commit_tx();
        self.ledger.on_transaction_end();
    }

    fn discard_tx(&mut self) {
        self.revm.discard_tx();
        self.ledger.on_transaction_end();
    }

    fn finalize(&mut self) -> EvmState {
        let state = self.revm.finalize();
        self.ledger.on_transaction_end();
        state
    }

    /// The trait's own default: it finalizes through the hooked `finalize`.
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
    use crate::{
        limit::{HistoryBytes, Mark, StartFacts, StartReport, StartSide},
        test_utils::MemoryDatabase,
        EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, TRANSFER_LOG_SIZE, TX_BODY_SIZE,
        WRITE_RECORD_SIZE,
    };
    use alloy_primitives::address;
    use revm::{
        context_interface::journaled_state::account::JournaledAccountTr,
        primitives::Log as PrimitiveLog,
    };

    const SENDER: Address = address!("00000000000000000000000000000000000a0001");
    /// The transaction's own frame.
    const A: Address = address!("00000000000000000000000000000000000a0002");
    const B: Address = address!("00000000000000000000000000000000000a0003");
    const C: Address = address!("00000000000000000000000000000000000a0004");
    /// The account a creation makes.
    const CREATED: Address = address!("00000000000000000000000000000000000a0005");

    /// What a start's records and transfer log weigh: `records` write records, and the log when it
    /// `moves` value.
    const fn start_bytes(records: u64, moves: bool) -> u64 {
        records * WRITE_RECORD_SIZE + if moves { TRANSFER_LOG_SIZE } else { 0 }
    }

    /// The transaction's bytes before any frame: the body.
    const fn body() -> LimitUsage {
        LimitUsage { data_size: TX_BODY_SIZE, write_records: 0 }
    }

    /// `records` write records and `logs` transfer logs on top of the body.
    const fn kept(records: u64, logs: u64) -> LimitUsage {
        LimitUsage {
            data_size: TX_BODY_SIZE + records * WRITE_RECORD_SIZE + logs * TRANSFER_LOG_SIZE,
            write_records: records,
        }
    }

    /// A journal on Satin's base spec, with transfer logs, every account funded, holding a
    /// transaction from [`SENDER`] under `limits`. The sender is changed as validation leaves it:
    /// its nonce bumped.
    fn journal(limits: EvmTxRuntimeLimits) -> MegaJournal<MemoryDatabase> {
        let db = [SENDER, A, B, C].into_iter().fold(MemoryDatabase::default(), |db, account| {
            db.account_balance(account, U256::from(10))
        });
        let mut journal = MegaJournal::new(db);
        journal.set_spec_id(SpecId::OSAKA);
        journal.set_amsterdam_eip7708_enabled(true);
        for account in [SENDER, A, B, C, CREATED] {
            journal.load_account(account).unwrap();
        }
        let _ = journal.load_account_mut(SENDER).unwrap().data.bump_nonce();
        journal.ledger_mut().set_limits(limits);
        journal.ledger_mut().begin_transaction(false, true, TX_BODY_SIZE);
        journal
    }

    /// Starts a call from `caller` to `target` at `depth` as revm starts one: its checkpoint, then
    /// its value transfer. Returns the frame's checkpoint and what the start counted.
    fn start_call(
        journal: &mut MegaJournal<MemoryDatabase>,
        depth: usize,
        caller: Address,
        target: Address,
        value: u64,
    ) -> (JournalCheckpoint, StartReport) {
        let value = U256::from(value);
        let facts = StartFacts { depth, creation: false, transfers_value: !value.is_zero() };
        journal.ledger_mut().open_start(facts);
        let checkpoint = journal.checkpoint();
        assert!(journal.transfer_loaded(caller, target, value).is_none());
        (checkpoint, journal.ledger_mut().close_start())
    }

    /// Starts a creation of `created` from `creator` at `depth` as revm starts one: the nonce
    /// bump, then the account checkpoint.
    fn start_creation(
        journal: &mut MegaJournal<MemoryDatabase>,
        depth: usize,
        creator: Address,
        created: Address,
        value: u64,
    ) -> (JournalCheckpoint, StartReport) {
        let value = U256::from(value);
        let facts = StartFacts { depth, creation: true, transfers_value: !value.is_zero() };
        journal.ledger_mut().open_start(facts);
        let _ = journal.load_account_mut(creator).unwrap().data.bump_nonce();
        let checkpoint =
            journal.create_account_checkpoint(creator, created, value, SpecId::OSAKA).unwrap();
        (checkpoint, journal.ledger_mut().close_start())
    }

    /// Returns from the running frame as revm does: its level closes, committed or reverted to
    /// `failed`'s checkpoint.
    fn end_frame(journal: &mut MegaJournal<MemoryDatabase>, failed: Option<JournalCheckpoint>) {
        journal.ledger_mut().frame_returning();
        match failed {
            Some(checkpoint) => journal.checkpoint_revert(checkpoint),
            None => journal.checkpoint_commit(),
        }
    }

    /// The journal's own write methods count nothing, inside a frame start's window and outside
    /// it: a write an opcode completed is counted by the Host's completed-write hook, and any other
    /// is not the transaction's own execution. A `SELFDESTRUCT` is observed for that hook, and
    /// counts nothing until the hook counts it.
    #[test]
    fn test_the_journals_own_writes_count_nothing() {
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        let writes = |journal: &mut MegaJournal<MemoryDatabase>| {
            journal.sstore(A, U256::from(1), U256::from(1)).unwrap();
            journal.sstore_skip_cold_load(A, U256::from(2), U256::from(1), false).unwrap();
            journal.log(PrimitiveLog::new_unchecked(A, Vec::new(), Bytes::new()));
            assert_eq!(journal.transfer(A, B, U256::from(1)).unwrap(), None);
            journal.selfdestruct(C, B, false).unwrap();
        };
        let _ = start_call(&mut journal, 0, SENDER, A, 0);
        writes(&mut journal);
        assert_eq!(journal.ledger().usage(), body(), "outside a start's window");

        journal.ledger_mut().open_start(StartFacts {
            depth: 1,
            creation: false,
            transfers_value: false,
        });
        let _ = journal.checkpoint();
        writes(&mut journal);
        let _ = journal.ledger_mut().close_start();
        assert_eq!(journal.ledger().usage(), body(), "inside a start's window");
        assert!(!journal.logs().is_empty(), "the writes reached revm's journal");
    }

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
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        journal.sstore(A, U256::from(1), U256::from(1)).unwrap();
        let entries = JournalExt::journal(&journal);
        assert!(!entries.is_empty());
        assert_eq!(entries, JournalExt::journal(&*journal), "revm's own");
    }

    /// An ordinary method call on the journal is the journal's own trait method, not one of revm's
    /// journal reached through the dereference: a checkpoint opened and reverted with method syntax
    /// opens and closes a level of the ledger, and the extension's entries are revm's.
    #[test]
    fn test_an_ordinary_method_call_is_the_journals_own() {
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        let checkpoint = journal.checkpoint();
        assert_eq!(journal.ledger().depth(), 1, "the ledger saw the checkpoint");
        journal.sstore(A, U256::from(1), U256::from(1)).unwrap();
        assert_eq!(journal.journal().len(), journal.revm.journal.len());
        journal.checkpoint_revert(checkpoint);
        assert_eq!((journal.ledger().depth(), journal.depth()), (0, 0));
    }

    /// The ledger holds a level for every checkpoint revm's journal holds open: a commit closes
    /// one and keeps what was counted inside it, a revert closes one and takes it back, and
    /// `clear`, the trait's default, closes them all through `finalize`.
    #[test]
    fn test_the_levels_follow_revms_checkpoints() {
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        let _ = start_call(&mut journal, 0, SENDER, A, 0);
        let (checkpoint, _) = start_call(&mut journal, 1, A, B, 1);
        assert_eq!((journal.ledger().depth(), journal.depth()), (2, 2));
        assert_eq!(journal.ledger().usage(), kept(2, 1), "B's record, A's, and the transfer log");
        end_frame(&mut journal, Some(checkpoint));
        assert_eq!((journal.ledger().depth(), journal.depth()), (1, 1));
        assert_eq!(journal.ledger().usage(), body(), "the revert takes the start back");

        let _ = start_call(&mut journal, 1, A, B, 1);
        end_frame(&mut journal, None);
        assert_eq!(journal.ledger().usage(), kept(2, 1), "the commit keeps it");
        let _ = journal.checkpoint();
        journal.clear();
        assert_eq!((journal.ledger().depth(), journal.depth()), (0, 0));
        assert_eq!(journal.ledger().usage(), kept(2, 1), "readable until the next transaction");
    }

    /// An account is changed once its nonce, balance or code differs from what the transaction
    /// loaded, and unchanged again once a write returns it; one the transaction has not loaded is
    /// unchanged. The sender is changed from the start, by the nonce validation bumped.
    #[test]
    fn test_an_account_is_changed_while_its_info_differs_from_what_was_loaded() {
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        assert!(journal.is_changed(SENDER), "validation bumped its nonce");
        assert!(!journal.is_changed(A));
        assert!(
            !journal.is_changed(address!("00000000000000000000000000000000000a00ff")),
            "not loaded"
        );
        assert_eq!(journal.transfer(A, B, U256::from(3)).unwrap(), None);
        assert!(journal.is_changed(A) && journal.is_changed(B));
        assert_eq!(journal.transfer(B, A, U256::from(3)).unwrap(), None);
        assert!(!journal.is_changed(A) && !journal.is_changed(B), "both returned");
        let _ = journal.load_account_mut(A).unwrap().data.bump_nonce();
        assert!(journal.is_changed(A), "a nonce bump changes it");
        assert!(!journal.is_changed_before_bump(A), "but not before the bump");
        journal.set_code(B, Bytecode::new_raw(Bytes::from_static(&[0x00])));
        assert!(journal.is_changed(B), "code changes it");
    }

    /// The records a transfer makes are predicted from the journal's state as the hook sees it and
    /// counted as revm makes the transfer, and the two agree: a transfer that changes both
    /// accounts, one whose sender the transaction changed, one that returns an account, and the
    /// moves that make none — to the same account, of nothing, of more than the sender holds.
    #[test]
    fn test_the_records_predicted_for_a_transfer_are_the_records_counted() {
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        let _ = start_call(&mut journal, 0, SENDER, A, 0);
        let none = RecordDelta::NONE;
        let both = RecordDelta { added: 2, taken: 0 };
        let recipient = RecordDelta { added: 1, taken: 0 };
        assert_eq!(journal.transfer_records(A, A, U256::from(1)), none, "to itself");
        assert_eq!(journal.transfer_records(A, B, U256::ZERO), none, "of nothing");
        assert_eq!(journal.transfer_records(A, B, U256::from(11)), none, "unfunded");
        assert_eq!(journal.transfer_records(SENDER, B, U256::from(1)), recipient, "the sender");
        assert_eq!(journal.transfer_records(A, B, U256::from(1)), both);

        let (_, report) = start_call(&mut journal, 1, A, B, 1);
        assert_eq!(report.records.on_level, both, "counted as predicted");
        end_frame(&mut journal, None);
        assert_eq!(
            journal.transfer_records(B, A, U256::from(1)),
            RecordDelta { added: 0, taken: 2 },
            "the return takes both back"
        );
        assert_eq!(
            journal.transfer_records(B, C, U256::from(1)),
            RecordDelta { added: 1, taken: 1 },
            "B returns, C changes"
        );
        let (_, report) = start_call(&mut journal, 1, B, A, 1);
        assert_eq!(report.records.on_level, RecordDelta { added: 0, taken: 2 });
        assert_eq!(journal.ledger().usage(), kept(0, 2), "the records went, the logs stay");
    }

    /// A tool's checkpoint around a call's start — opened before the start, closed after the frame
    /// returned — is a rollback level the frame's level sits above. Committed, it changes nothing:
    /// the frame's records stand, its refund is none, and its caller's account stays changed, so a
    /// later value call from it records only its recipient. Reverted, it takes back what the frame
    /// kept: the records and the frame's refund all go back to before the start, and the later
    /// call finds the caller unchanged and records it again.
    #[test]
    fn test_a_tool_checkpoint_around_a_call_closes_in_order() {
        for reverted in [false, true] {
            let case = if reverted { "reverted" } else { "committed" };
            let mut journal = journal(EvmTxRuntimeLimits::no_limits());
            let _ = start_call(&mut journal, 0, SENDER, A, 0);
            let tool = journal.checkpoint();
            let (_, report) = start_call(&mut journal, 1, A, B, 1);
            assert_eq!(
                (report.records.on_level, report.transfer_log),
                (RecordDelta { added: 2, taken: 0 }, true),
                "{case}: B's record, A's, and the log"
            );
            end_frame(&mut journal, None);
            let mut side = StartSide::new(1, 7, 0, true, None);
            side.attach(report, None);
            if reverted {
                journal.checkpoint_revert(tool);
            } else {
                journal.checkpoint_commit();
            }
            let refund = if reverted { 7 } else { 0 };
            assert_eq!(side.refund(journal.ledger()), refund, "{case}: B's refund");
            assert_eq!(journal.ledger().usage(), if reverted { body() } else { kept(2, 1) });

            let (_, later) = start_call(&mut journal, 1, A, C, 1);
            let records = if reverted { 2 } else { 1 };
            assert_eq!(later.records.on_level.added, records, "{case}: A's account");
            end_frame(&mut journal, None);
            let records = if reverted { 2 } else { 3 };
            assert_eq!(journal.ledger().usage(), kept(records, if reverted { 1 } else { 2 }));
        }
    }

    /// A tool's checkpoint around a creation's start closes in order as one around a call does, and
    /// a revert of it takes back the creator's nonce record with the rest, which the creation's own
    /// failure leaves: the creator's caller gets back the charge for that record too, and the
    /// creator's account is unchanged again, so its next creation records it.
    #[test]
    fn test_a_tool_checkpoint_around_a_creation_takes_its_creators_record_back() {
        for (case, failed, reverted, refund, creator_changed) in [
            ("a creation that succeeds, the tool's checkpoint committed", false, false, 0, true),
            ("a creation that fails, the tool's checkpoint committed", true, false, 7, true),
            (
                "a creation that succeeds, the tool's checkpoint reverted",
                false,
                true,
                7 + 11,
                false,
            ),
            ("a creation that fails, the tool's checkpoint reverted", true, true, 7 + 11, false),
        ] {
            let mut journal = journal(EvmTxRuntimeLimits::no_limits());
            let _ = start_call(&mut journal, 0, SENDER, A, 0);
            let _ = start_call(&mut journal, 1, A, B, 0);
            let tool = journal.checkpoint();
            let (checkpoint, report) = start_creation(&mut journal, 2, B, CREATED, 1);
            assert!(report.records.creator, "{case}: B's nonce record is the start's");
            end_frame(&mut journal, failed.then_some(checkpoint));
            let mut side = StartSide::new(2, 7, 11, true, None);
            side.attach(report, None);
            if reverted {
                journal.checkpoint_revert(tool);
            } else {
                journal.checkpoint_commit();
            }
            assert_eq!(side.refund(journal.ledger()), refund, "{case}: the refund");
            let (_, later) = start_creation(&mut journal, 2, B, C, 0);
            assert_eq!(!later.records.creator, creator_changed, "{case}: B's account");
        }
    }

    /// A revert that would close a level from below — a checkpoint taken before a frame started,
    /// reverted while the frame still runs — is not supported, and trips.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a revert closes the latest level")]
    fn test_a_revert_below_a_running_frame_trips() {
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        let _ = start_call(&mut journal, 0, SENDER, A, 0);
        let tool = journal.checkpoint();
        let _ = start_call(&mut journal, 1, A, B, 1);
        journal.checkpoint_revert(tool);
    }

    /// A creation's budget and usage are measured from before its creator's nonce record, and its
    /// level opens after it: with `R` bytes left to an unchanged creator, the creation gets
    /// `R − ⌈R / 50⌉` and starts with the created account's record and the creator's, 80 bytes.
    /// At 81 bytes that is a budget of 79, which the start crosses; at 100, a budget of 98, which
    /// leaves the creation 18 bytes. An endowment adds its transfer log to the start.
    #[test]
    fn test_a_creation_is_budgeted_from_before_its_creators_record() {
        let log = TRANSFER_LOG_SIZE;
        for (left, value, budget) in
            [(81, 0, 79), (100, 0, 98), (81 + log, 1, 236), (100 + log, 1, 254)]
        {
            let start = start_bytes(2, value != 0);
            let limits =
                EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE + left);
            let mut journal = journal(limits);
            let _ = start_call(&mut journal, 0, SENDER, A, 0);
            let (_, report) = start_creation(&mut journal, 1, A, CREATED, value);
            let case = format!("{left} bytes left, value {value}");
            assert!(report.records.creator, "{case}: the creator's record is the start's");
            let used = LimitUsage { data_size: start, write_records: 2 };
            assert_eq!(journal.ledger().running_budget(), Some((budget, used)), "{case}");
            let verdict = if start > budget {
                LimitCheck::ExceedsLimit {
                    kind: LimitKind::DataSize,
                    limit: budget,
                    used: start,
                    frame_local: true,
                }
            } else {
                LimitCheck::WithinLimit
            };
            assert_eq!(report.verdict, Some(verdict), "{case}");
        }
    }

    /// A creation's start mark is its level's opening — after the creator's record — never where
    /// its usage is measured from: a creation rewritten into a failure is taken back to it, and
    /// keeps its creator's record, as a creation that fails does, the creator's nonce standing.
    #[test]
    fn test_a_creations_start_mark_keeps_its_creators_record() {
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        let _ = start_call(&mut journal, 0, SENDER, A, 0);
        let (checkpoint, _) = start_creation(&mut journal, 1, A, CREATED, 1);
        let mark: Mark = journal.ledger().frame_start_mark().expect("the creation's level is open");
        assert_eq!(mark.position, checkpoint, "the creation's own checkpoint");
        end_frame(&mut journal, None);
        assert_eq!(journal.ledger().usage(), kept(2, 1));

        journal.revert_to(&mark);
        assert_eq!(journal.ledger().usage(), kept(1, 0), "the creator's record stays");
        let (_, later) = start_creation(&mut journal, 1, A, C, 0);
        assert!(!later.records.creator, "A stays changed");
    }

    /// The hypothetical start a call gas detention answers without running is held to the limits
    /// as the start revm would have made: the same records, the transfer log of the value it moves,
    /// the same budget.
    #[test]
    fn test_a_hypothetical_start_is_held_as_the_start_revm_makes() {
        // A value call one frame down records its recipient and its caller; the transaction's own
        // frame records its recipient alone.
        let (below, own) = (start_bytes(2, true), start_bytes(1, true));
        for (depth, value, room) in
            [(1, 1, below - 1), (1, 1, below), (1, 0, 0), (0, 1, own - 1), (0, 1, own)]
        {
            let limits =
                EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE + room);
            let mut journal = journal(limits);
            let caller = if depth == 0 {
                SENDER
            } else {
                let _ = start_call(&mut journal, 0, SENDER, A, 0);
                A
            };
            let records = journal.transfer_records(caller, B, U256::from(value));
            let logged = journal.ledger().counts_transfer_logs() && value != 0;
            let hypothetical = journal.ledger().hypothetical_start(records, logged);
            let (_, report) = start_call(&mut journal, depth, caller, B, value);
            assert_eq!(Some(hypothetical), report.verdict, "depth {depth}, value {value}, {room}");
        }
    }

    /// A `SELFDESTRUCT` is observed for the completed-write hook as its move leaves the two
    /// accounts: the destructed account, whose balance it clears, and the beneficiary. A
    /// beneficiary the transaction gave its balance is returned by the move; a destruction to
    /// itself, of an account the transaction did not create, moves nothing.
    #[test]
    fn test_a_destruction_is_observed_for_the_hook() {
        let mut journal = journal(EvmTxRuntimeLimits::no_limits());
        let _ = start_call(&mut journal, 0, SENDER, A, 0);
        let records = |journal: &mut MegaJournal<MemoryDatabase>, from, to| {
            journal.selfdestruct(from, to, false).unwrap();
            journal.ledger().write_count(
                &revm::context_interface::host::CompletedWrite::SelfDestruct {
                    address: from,
                    target: to,
                    result: &SelfDestructResult { had_value: true, ..Default::default() },
                },
            )
        };
        let (count, history) = records(&mut journal, A, B);
        assert_eq!(
            count.added,
            LimitUsage { data_size: 2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE, write_records: 2 }
        );
        assert_eq!(count.taken, LimitUsage::ZERO);
        assert_eq!(history, HistoryBytes { appended: 2 * WRITE_RECORD_SIZE, taken: 0 });

        // C gives three of its ten to an account that holds nothing, which destructs back to C:
        // both are as the transaction loaded them again.
        assert_eq!(journal.transfer(C, CREATED, U256::from(3)).unwrap(), None);
        let (count, history) = records(&mut journal, CREATED, C);
        assert_eq!(count.added, LimitUsage { data_size: TRANSFER_LOG_SIZE, write_records: 0 });
        assert_eq!(count.taken, LimitUsage { data_size: 2 * WRITE_RECORD_SIZE, write_records: 2 });
        assert_eq!(history, HistoryBytes { appended: 0, taken: 2 * WRITE_RECORD_SIZE });

        let (count, _) = records(&mut journal, C, C);
        assert_eq!(count.added, LimitUsage::ZERO, "a destruction to itself moves nothing");
        assert_eq!(count.taken, LimitUsage::ZERO);
    }
}
