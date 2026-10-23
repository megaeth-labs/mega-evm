//! What a write counts: the write records it makes and takes back, by the net change of what it
//! writes, and its other bytes.
//!
//! An account's write record exists while the account's info — its nonce, balance or code —
//! differs from what the transaction loaded, revm's original info, which revm sets at the
//! account's first load in the transaction; a storage slot's while its value differs from its
//! original value. A write that changes an unchanged account or slot makes the record, a write that
//! returns one to its original takes it back, and a write to one already changed adds nothing.
//!
//! The facts are read off revm's journal. What a frame's start made is read off what revm did:
//! the accounts it writes on its frame as they stood before revm decided the start, and as they
//! stand once it did ([`StartBefore`]); a creator's nonce record likewise, the creator as it stood
//! before revm decided the creation, and as it stands once the creation returned. The opcode that
//! starts the frame prices the start's records before revm decides it, as the journal's state
//! stands then ([`StartRecords`]), and a completed write is read after it, where a
//! `SELFDESTRUCT`'s accounts are rebuilt as they were before it from the balance it moved.
//!
//! State growth is counted at the same sites, one unit per new account or new slot, where EIP-8037
//! charges the state gas of one: a slot an `SSTORE` makes non-zero from an original of zero, and
//! takes back by writing it to zero again; an account that is empty under EIP-161 and that a value
//! transfer, a creation, a `SELFDESTRUCT`'s balance or a creator's nonce bump makes.
//!
//! The first effective `SELFDESTRUCT` of an account created in the transaction takes back every
//! part of it the transaction made ([`DestroyedParts`]): its leaf when it was absent at the
//! transaction's start, with the account's record and its unit of state growth; the code deposited
//! at it, with its bytes; and each slot it made non-zero from an original of zero, with its record
//! and its unit. From that mark until the transaction ends the account counts nothing more: its
//! nonce is no longer what the transaction loaded, so no write to the account makes or takes back
//! its record, a value moved to it adds no account, and an `SSTORE` of one of its slots counts
//! nothing ([`Accounts::is_destroyed`]). Every fact is the journal's: revm's flags of an account
//! created and of one destroyed in the transaction, which a failed frame's revert takes back with
//! the rest of it, the account's original info and each slot's original value.
//!
//! Whether a value movement journals an EIP-7708 transfer log, whose bytes a frame start and a
//! `SELFDESTRUCT` count, is the journal's too: its own switch, read by the rule revm emits the log
//! by ([`emits_transfer_logs`]). A caller or an inspector that turns the journal's switch through
//! the context's mutable journal, past the configuration the spec fixes, changes what revm emits,
//! and the count follows the logs revm emits.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use revm::{
    context_interface::{
        host::{CompletedWrite, FrameStartKind},
        journaled_state::entry::{JournalEntry, SelfdestructionRevertStatus},
    },
    primitives::{
        eip7708::{ETH_TRANSFER_LOG_ADDRESS, ETH_TRANSFER_LOG_TOPIC},
        hardfork::SpecId,
        Address, Log, StorageKey, KECCAK_EMPTY, U256,
    },
    state::{Account, AccountInfo, EvmState, TransactionId},
    Journal,
};

use super::{log_history_bytes, Counted, TRANSFER_LOG_SIZE};

/// How a write moves the record count: the records it creates, for accounts it changes from what
/// the transaction loaded, and the records it takes back, for accounts it returns to that.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecordDelta {
    /// Records created.
    pub(crate) added: u64,
    /// Records taken back.
    pub(crate) taken: u64,
}

impl RecordDelta {
    /// No record moved.
    pub(crate) const NONE: Self = Self { added: 0, taken: 0 };

    /// The delta of one account whose info was changed from what the transaction loaded `before`
    /// a write and is changed `after` it: a record when the write changed it, a record taken back
    /// when the write returned it, and nothing otherwise.
    pub(crate) const fn of_account(before: bool, after: bool) -> Self {
        match (before, after) {
            (false, true) => Self { added: 1, taken: 0 },
            (true, false) => Self { added: 0, taken: 1 },
            _ => Self::NONE,
        }
    }

    /// This delta and `other` together.
    pub(crate) const fn with(self, other: Self) -> Self {
        Self { added: self.added + other.added, taken: self.taken + other.taken }
    }
}

/// The write records a frame's start makes, as the journal's state stands before revm decides it,
/// which is what its opcode prices their history at: a call's value transfer's, and a creation's
/// created account and creator nonce.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartRecords {
    /// The records a call's value transfer makes and takes back: the accounts it changes from what
    /// the transaction loaded, and those it returns to that.
    pub(crate) transfer: RecordDelta,
    /// Whether a creation records the account it creates: one the transaction had not changed.
    /// A creation sets the account's nonce to one, so it never returns the account.
    pub(crate) created: bool,
    /// Whether a creation records its creator's nonce: the bump changes an account the transaction
    /// had not changed. The record is the creator's, which the creation's failure leaves with it,
    /// and is counted on the creator once the creation returned ([`CreatorBefore`]).
    pub(crate) creator: bool,
}

impl StartRecords {
    /// No record.
    pub(crate) const NONE: Self =
        Self { transfer: RecordDelta::NONE, created: false, creator: false };

    /// The records the start makes on the started frame, which its failure takes back: a
    /// transfer's, and the created account. The creator's is not among them.
    pub(crate) const fn on_frame(self) -> RecordDelta {
        RecordDelta { added: self.transfer.added + self.created as u64, taken: self.transfer.taken }
    }
}

/// The parts of an account created in the transaction that its first effective `SELFDESTRUCT`
/// gives back: each part the transaction made, which does not remain for a node to store. A part
/// that existed at the transaction's start is an ordinary deletion and is not among them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DestroyedParts {
    /// Whether the account's leaf was absent at the transaction's start: the leaf's state gas, its
    /// unit of state growth and the account's write record come back.
    pub(crate) leaf: bool,
    /// The bytes of the code deposited at the account, which come back with their state gas, their
    /// data size and their history gas. Code is no write record and no unit of state growth.
    pub(crate) code: u64,
    /// The slots the transaction made non-zero from an original of zero, in key order: each slot's
    /// state gas, its unit of state growth and its write record come back.
    pub(crate) slots: Vec<StorageKey>,
    /// The slots that held a non-zero value at the transaction's start, which the transaction
    /// loaded and which still hold it: an ordinary deletion, which refills nothing and subtracts
    /// nothing, and whose deletion is what makes each slot's write record.
    pub(crate) deleted: u64,
}

impl DestroyedParts {
    /// The write records the parts take back: the account's, for an absent leaf, and each slot's.
    fn records(&self) -> u64 {
        u64::from(self.leaf) + self.slots.len() as u64
    }

    /// What the destruction counts of the parts: the records it takes back with their bytes, the
    /// code's bytes and one unit of state growth per record taken back, and the records it makes
    /// of the deleted slots that held a value at the start.
    pub(crate) fn counted(&self) -> Counted {
        let records = self.records();
        Counted::records(self.deleted, records, false)
            .with_growth(0, records)
            .plus_bytes_taken(self.code)
    }
}

/// The accounts the running transaction holds, as revm's journal holds them, read for the records
/// a write makes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Accounts<'a> {
    state: &'a EvmState,
    /// The journal's entries, which revm clears between transactions: this transaction's.
    entries: &'a [JournalEntry],
    /// The logs the journal holds, which a failed frame's revert takes back with its entries.
    logs: &'a [Log],
    transaction_id: TransactionId,
    /// Whether a value movement journals an EIP-7708 transfer log, as the journal's own
    /// configuration says ([`emits_transfer_logs`]).
    transfer_logs: bool,
}

/// Whether `info` differs from `original` in what a write record stands for: the nonce, the
/// balance or the code.
fn differs(info: &AccountInfo, original: &AccountInfo) -> bool {
    info.nonce != original.nonce ||
        info.balance != original.balance ||
        info.code_hash != original.code_hash
}

/// A creation's creator as the transaction held it before revm decided the creation's start: what
/// tells, once the creation returned, whether revm bumped the creator's nonce and what the bump
/// made of the creator ([`Accounts::creator_counted`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CreatorBefore {
    /// The creator's account.
    account: AccountBefore,
    /// Its nonce.
    nonce: u64,
}

/// An account a frame's start may change, as the transaction held it before revm decided the
/// start: what tells, once revm did, the write record and the state growth it made of the account
/// ([`Accounts::account_made`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AccountBefore {
    /// The account.
    address: Address,
    /// Whether the transaction had changed it from what it loaded.
    changed: bool,
    /// Whether it was empty under EIP-161.
    empty: bool,
}

/// The accounts a frame's start writes on its frame — a call's caller and target, a creation's
/// created account — as the transaction held them before revm decided the start, and how many logs
/// the journal held then: what tells, once revm decided the start, what it made
/// ([`Accounts::start_made`]). A creation's creator is not among them: its nonce record is the
/// creator's, read off the bump once the creation returned ([`CreatorBefore`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StartBefore {
    /// The accounts, each once.
    accounts: [Option<AccountBefore>; 2],
    /// The logs the journal held.
    logs: usize,
}

/// Whether a frame start of `kind` that transfers `value` makes no write record whatever the
/// journal holds, so the opcode starting it prices its records at nothing without reading the
/// journal: a call of any scheme that transfers no value, whose transfer records nothing
/// ([`Accounts::transfer_records`]). A creation may record the account it creates and its
/// creator's nonce.
pub(crate) fn start_makes_no_records(kind: FrameStartKind, value: U256) -> bool {
    match kind {
        FrameStartKind::Create | FrameStartKind::Create2 => false,
        FrameStartKind::Call |
        FrameStartKind::CallCode |
        FrameStartKind::DelegateCall |
        FrameStartKind::StaticCall => value.is_zero(),
    }
}

/// Whether `log` is the EIP-7708 transfer log revm journals for a value movement: a log from the
/// system address `0xff…fe` carrying ERC-20's `Transfer` event.
pub(crate) fn is_transfer_log(log: &Log) -> bool {
    log.address == ETH_TRANSFER_LOG_ADDRESS && log.topics().first() == Some(&ETH_TRANSFER_LOG_TOPIC)
}

/// Whether a value movement journals an EIP-7708 transfer log on `journal`: from Amsterdam on, or
/// with the switch on, unless EIP-7708 is disabled. It is the rule revm's journal emits one by,
/// read off the journal's own copy of the configuration, so the data size counts a transfer log
/// exactly where revm emits one, a switch a caller turned on the journal itself included.
pub(crate) fn emits_transfer_logs<DB>(journal: &Journal<DB>) -> bool {
    (journal.cfg.spec.is_enabled_in(SpecId::AMSTERDAM) || journal.amsterdam_eip7708_enabled()) &&
        !journal.cfg.eip7708_disabled
}

impl<'a> Accounts<'a> {
    /// The accounts `journal` holds.
    pub(crate) fn of<DB>(journal: &'a Journal<DB>) -> Self {
        Self {
            state: &journal.state,
            entries: &journal.journal,
            logs: &journal.logs,
            transaction_id: journal.transaction_id,
            transfer_logs: emits_transfer_logs(journal),
        }
    }

    /// The account at `address` as the transaction loaded it and holds it now: `None` for one
    /// the transaction has not loaded, which it has not changed either.
    fn loaded(&self, address: Address) -> Option<&'a Account> {
        self.state.get(&address).filter(|account| account.transaction_id == self.transaction_id)
    }

    /// Whether the transaction has loaded the account at `address`. An account an earlier
    /// transaction on the same journal left in it is not loaded until this one loads it again.
    pub(crate) fn is_loaded(&self, address: Address) -> bool {
        self.loaded(address).is_some()
    }

    /// Whether the transaction changed the account at `address` from what it loaded: its nonce,
    /// balance or code differs from the account's original info. An account the transaction has
    /// not loaded is unchanged.
    pub(crate) fn is_changed(&self, address: Address) -> bool {
        self.loaded(address).is_some_and(|account| differs(&account.info, &account.original_info()))
    }

    /// Whether the account at `address` would be changed from what the transaction loaded were
    /// its balance `balance`, the rest of its info as it is.
    fn changed_with_balance(&self, address: Address, balance: U256) -> bool {
        match self.loaded(address) {
            Some(account) => {
                differs(&AccountInfo { balance, ..account.info.clone() }, &account.original_info())
            }
            // What the transaction loads for an account it has not loaded is what the chain holds,
            // so a balance it does not hold changes it.
            None => balance != U256::ZERO,
        }
    }

    /// Whether the account at `address` was created in the transaction and has been destroyed
    /// since: the mark the first effective `SELFDESTRUCT` of such an account leaves, which
    /// stands until the transaction ends unless the destroying frame, or a frame that contains
    /// it, fails.
    pub(crate) fn is_destroyed(&self, address: Address) -> bool {
        self.loaded(address).is_some_and(|account| {
            account.is_created_locally() && account.is_selfdestructed_locally()
        })
    }

    /// The parts of the account at `address` its `SELFDESTRUCT` that just completed gives back,
    /// read off the journal after the write: `None` unless the account was created in the
    /// transaction and the destruction is its first effective one (`previously_destroyed` names
    /// one the mark already stood for).
    ///
    /// The leaf was absent at the transaction's start when the transaction found no account
    /// there ([`Self::was_absent_at_start`]), which a value transfer, a `SELFDESTRUCT`'s balance,
    /// the transaction's value or the creation itself then charged as a new account; a leaf the
    /// transaction found, a balance or an account empty under EIP-161 with storage, stands. The
    /// code is what the account holds: non-empty code at the transaction's start collides with the
    /// creation, so it is code the transaction deposited. A slot the transaction did not load has
    /// no record. A slot of an account created in the transaction that the journal does not hold
    /// yet loads as zero, so the slots given back are those it made non-zero; one loaded before the
    /// creation, which only a tool's load reaches, keeps the value it held at the start, and the
    /// deletion makes its record while it still holds that value.
    pub(crate) fn destroyed_parts(
        &self,
        address: Address,
        previously_destroyed: bool,
    ) -> Option<DestroyedParts> {
        let account = self.loaded(address)?;
        if previously_destroyed || !account.is_created_locally() {
            return None;
        }
        let original = account.original_info();
        debug_assert_eq!(original.code_hash, KECCAK_EMPTY, "code at the start collides");
        debug_assert!(self.is_changed(address), "a creation changes its account's nonce");
        let leaf = self.was_absent_at_start(address, account);
        let code = if account.info.code_hash == KECCAK_EMPTY {
            0
        } else {
            account.info.code.as_ref().map_or(0, |code| code.len() as u64)
        };
        let loaded =
            account.storage.iter().filter(|(_, slot)| slot.transaction_id == self.transaction_id);
        let mut slots = Vec::new();
        let mut deleted = 0;
        for (key, slot) in loaded {
            if slot.original_value.is_zero() {
                if !slot.present_value.is_zero() {
                    slots.push(*key);
                }
            } else if !slot.is_changed() {
                deleted += 1;
            }
        }
        slots.sort_unstable();
        Some(DestroyedParts { leaf, code, slots, deleted })
    }

    /// Whether the transaction found no account at `address`, which it loaded as `account`, when
    /// it started: its info then was empty, and no account stood there — the database answered
    /// none, or an earlier transaction on the same journal destroyed the account, which its first
    /// load in this transaction cleared. An account the database answered with, empty under EIP-161
    /// with storage, stood there.
    ///
    /// Read at the completed write of the account's `SELFDESTRUCT`. revm's flag of an account the
    /// database answered none for was set at its first load on the journal and outlives the
    /// transactions after it, so an earlier destruction is read off revm's entry for the one that
    /// just completed: [`SelfdestructionRevertStatus::LocallySelfdestroyed`] names a first
    /// destruction in this transaction of an account an earlier one destroyed. revm empties its
    /// entries when a transaction ends, so they are this transaction's.
    fn was_absent_at_start(&self, address: Address, account: &Account) -> bool {
        if !account.original_info().is_empty() {
            return false;
        }
        account.is_loaded_as_not_existing() ||
            self.entries.iter().rev().find_map(|entry| match entry {
                JournalEntry::AccountDestroyed { address: destroyed, destroyed_status, .. }
                    if *destroyed == address =>
                {
                    Some(*destroyed_status == SelfdestructionRevertStatus::LocallySelfdestroyed)
                }
                _ => None,
            }) == Some(true)
    }

    /// Whether the account at `address` is empty under EIP-161 — no nonce, no balance, no code —
    /// as the transaction holds it: a value moved to it, or a creation at it, makes a new account.
    /// An account the transaction has not loaded is not known to be empty.
    pub(crate) fn is_empty(&self, address: Address) -> bool {
        self.loaded(address).is_some_and(|account| account.info.is_empty())
    }

    /// The current balance of the account at `address`; nothing for one the transaction has not
    /// loaded.
    fn balance(&self, address: Address) -> U256 {
        self.loaded(address).map_or(U256::ZERO, |account| account.info.balance)
    }

    /// The records a transfer of `value` from `from` to `to` makes and takes back, as the journal's
    /// state stands: nothing for a transfer to the same account or of nothing, which revm moves
    /// nothing for, and nothing for a value `from` cannot fund, which revm refuses.
    pub(crate) fn transfer_records(&self, from: Address, to: Address, value: U256) -> StartRecords {
        if from == to || value.is_zero() {
            return StartRecords::NONE;
        }
        let Some(from_after) = self.balance(from).checked_sub(value) else {
            return StartRecords::NONE;
        };
        let to_after = self.balance(to).saturating_add(value);
        let transfer = RecordDelta::of_account(
            self.is_changed(from),
            self.changed_with_balance(from, from_after),
        )
        .with(RecordDelta::of_account(
            self.is_changed(to),
            self.changed_with_balance(to, to_after),
        ));
        StartRecords { transfer, ..StartRecords::NONE }
    }

    /// The records a creation by `creator` at `created` with an endowment of `value` makes, before
    /// revm bumps the creator's nonce: the creator's nonce, when the bump changes an account the
    /// transaction had not changed; and the account it creates, when the transaction had not
    /// changed it. Nothing for an endowment `creator` cannot fund, which revm refuses before it
    /// bumps the nonce, nor for a creation that takes no address.
    pub(crate) fn creation_records(
        &self,
        creator: Address,
        created: Option<Address>,
        value: U256,
    ) -> StartRecords {
        let Some(created) = created else { return StartRecords::NONE };
        if self.balance(creator) < value {
            return StartRecords::NONE;
        }
        StartRecords {
            transfer: RecordDelta::NONE,
            created: !self.is_changed(created),
            creator: !self.is_changed(creator),
        }
    }

    /// The creator at `address` of a creation revm is about to decide, as the transaction holds it.
    /// The creator is read loaded: revm loads it first thing when it decides the creation.
    pub(crate) fn creator_before(&self, address: Address) -> CreatorBefore {
        CreatorBefore {
            account: self.account_before(address),
            nonce: self.loaded(address).map_or(0, |account| account.info.nonce),
        }
    }

    /// Whether revm bumped the nonce of the creator `before` was read of: it does for every
    /// creation it decides but one it refuses on the creator's account — an endowment the creator
    /// cannot fund, a nonce at its maximum — and the bump stands whatever the creation then does.
    pub(crate) fn bumped(&self, before: &CreatorBefore) -> bool {
        let address = before.account.address;
        self.loaded(address).is_some_and(|account| account.info.nonce != before.nonce)
    }

    /// What the nonce bump of the creator `before` was read of made, read off the journal once its
    /// creation returned: the creator's write record when the bump changed an account the
    /// transaction had not changed, and one account of state growth when the bump made an empty
    /// account. The record follows the account's net change, so a keyless deployment's settlement,
    /// which may set the signer's nonce back, is read once it did.
    pub(crate) fn creator_counted(&self, before: &CreatorBefore) -> Counted {
        let (record, grew) = self.account_made(&before.account);
        Counted::records(record.added, record.taken, false).with_growth(u64::from(grew), 0)
    }

    /// The account at `address` as the transaction holds it.
    fn account_before(&self, address: Address) -> AccountBefore {
        AccountBefore { address, changed: self.is_changed(address), empty: self.is_empty(address) }
    }

    /// What changed of the account `before` was read of since, as the journal holds it now: its
    /// write record, by its change ([`RecordDelta::of_account`]), and whether it became a new
    /// account, empty under EIP-161 then and not now.
    fn account_made(&self, before: &AccountBefore) -> (RecordDelta, bool) {
        let record = RecordDelta::of_account(before.changed, self.is_changed(before.address));
        let loaded = self.loaded(before.address);
        (record, before.empty && loaded.is_some_and(|account| !account.info.is_empty()))
    }

    /// The accounts a start writes on its frame — `account` and `other`, a call's caller and
    /// target, or a creation's created account alone — as the transaction holds them before revm
    /// decides the start, the journal holding `logs` logs. An account named twice is read once.
    pub(crate) fn start_before(
        &self,
        account: Address,
        other: Option<Address>,
        logs: usize,
    ) -> StartBefore {
        let other = other.filter(|other| *other != account).map(|other| self.account_before(other));
        StartBefore { accounts: [Some(self.account_before(account)), other], logs }
    }

    /// What the start `before` was read of made on its frame, read off the journal once revm
    /// decided it and before anything ran on it: each account's write record, by its change since
    /// ([`RecordDelta::of_account`]); one account of state growth for each that was empty and is
    /// not; and the EIP-7708 transfer log revm journaled since. The records are answered apart as
    /// well, for the price of their history.
    pub(crate) fn start_made(&self, before: &StartBefore) -> (RecordDelta, Counted) {
        let mut records = RecordDelta::NONE;
        let mut growth = 0;
        for account in before.accounts.iter().flatten() {
            let (record, grew) = self.account_made(account);
            records = records.with(record);
            growth += u64::from(grew);
        }
        let logs = self.logs.get(before.logs..).unwrap_or_default();
        let logged = logs.iter().any(is_transfer_log);
        (records, Counted::records(records.added, records.taken, logged).with_growth(growth, 0))
    }

    /// The records a frame start of `kind` by `caller` makes: a call's value transfer to `target`
    /// ([`transfer_records`](Self::transfer_records)); a creation's at `target`, the address it
    /// takes ([`creation_records`](Self::creation_records)). The transfer of a `CALLCODE` is from
    /// the calling account to itself, and a `DELEGATECALL` or `STATICCALL` carries none: neither
    /// makes a record.
    pub(crate) fn start_records(
        &self,
        kind: FrameStartKind,
        caller: Address,
        target: Option<Address>,
        value: U256,
    ) -> StartRecords {
        match kind {
            FrameStartKind::Create | FrameStartKind::Create2 => {
                self.creation_records(caller, target, value)
            }
            FrameStartKind::Call |
            FrameStartKind::CallCode |
            FrameStartKind::DelegateCall |
            FrameStartKind::StaticCall => target
                .map_or(StartRecords::NONE, |target| self.transfer_records(caller, target, value)),
        }
    }

    /// The records a `SELFDESTRUCT` of `address` to `target` that moved `balance` made and took
    /// back, read off the journal after the write: a destruction changes balances alone, so the
    /// destructed account held `balance` before it, and a beneficiary that is another account its
    /// balance less `balance`, all of which it got.
    fn destruction_records(&self, address: Address, target: Address, balance: U256) -> RecordDelta {
        let source = RecordDelta::of_account(
            self.changed_with_balance(address, balance),
            self.is_changed(address),
        );
        if target == address {
            return source;
        }
        let before = self.balance(target).saturating_sub(balance);
        source.with(RecordDelta::of_account(
            self.changed_with_balance(target, before),
            self.is_changed(target),
        ))
    }

    /// What the completed write `write` counts, the history bytes it appends and takes back, and,
    /// for the first effective `SELFDESTRUCT` of an account created in the transaction, the parts
    /// of the account that come back:
    ///
    /// - `SSTORE`: the first change of a slot in the transaction is one write record; writing the
    ///   slot back to its original value takes the record back; a slot whose original value is zero
    ///   is one of state growth while it holds another, made by the write from zero and taken back
    ///   by the write to zero. A slot of an account created in the transaction and destroyed since
    ///   counts nothing ([`is_destroyed`](Self::is_destroyed)): the destruction took back what its
    ///   slots counted;
    /// - `LOG`: its bytes, one base record for the address, one per topic and the data, which are
    ///   history beside the records;
    /// - `SELFDESTRUCT`: the records of the accounts the move changed and returned — the
    ///   beneficiary, and the destructed account whose balance the move clears — and, when it moved
    ///   value to another account, the EIP-7708 transfer log revm journaled for the move, which is
    ///   data size and no history, and one of state growth when that account did not exist; and
    ///   what the parts it destroys take back ([`destroyed_parts`](Self::destroyed_parts)), whose
    ///   records are history taken back beside the move's. The code's history is the schedule's
    ///   charge for the deposit, which the caller gives back from the parts.
    pub(crate) fn write_counted(
        &self,
        write: &CompletedWrite<'_>,
    ) -> (Counted, HistoryBytes, Option<DestroyedParts>) {
        let mut destroyed = None;
        let counted = match *write {
            CompletedWrite::Sstore { address, .. } if self.is_destroyed(address) => Counted::NONE,
            CompletedWrite::Sstore { result: slot, .. } => {
                let records = if slot.is_original_eq_present() {
                    if slot.is_original_eq_new() {
                        Counted::NONE
                    } else {
                        Counted::records(1, 0, false)
                    }
                } else if slot.is_original_eq_new() {
                    Counted::records(0, 1, false)
                } else {
                    Counted::NONE
                };
                let fresh = slot.original_value.is_zero();
                let rise = fresh && slot.present_value.is_zero() && !slot.new_value.is_zero();
                let fall = fresh && !slot.present_value.is_zero() && slot.new_value.is_zero();
                records.with_growth(u64::from(rise), u64::from(fall))
            }
            CompletedWrite::Log { topics, data_len, .. } => {
                Counted::bytes(log_history_bytes(u64::from(topics), data_len))
            }
            CompletedWrite::SelfDestruct { address, target, balance, result } => {
                let records = self.destruction_records(address, target, balance);
                let moved = result.had_value && target != address;
                let logged = self.transfer_logs && moved;
                let creates = moved && !result.target_exists;
                let counted = Counted::records(records.added, records.taken, logged)
                    .with_growth(u64::from(creates), 0);
                destroyed = self.destroyed_parts(address, result.previously_destroyed);
                destroyed.as_ref().map_or(counted, |parts| counted.plus(parts.counted()))
            }
        };
        let log = if counted.transfer_log { TRANSFER_LOG_SIZE } else { 0 };
        let code = destroyed.as_ref().map_or(0, |parts| parts.code);
        let history = HistoryBytes {
            appended: counted.bytes_added() - log,
            taken: counted.bytes_taken() - code,
        };
        (counted, history, destroyed)
    }
}

/// The history bytes a completed write appends and takes back.
///
/// They are the record's data size and nothing else: what a log or a write record weighs in a
/// block is what the data-size limit meters it at, so the two cannot drift apart. That is the
/// pairing, and it holds per record; a transaction's two totals part where the byte table says
/// they do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HistoryBytes {
    /// Bytes the write appends: the records it creates and, for a log, its own bytes.
    pub(crate) appended: u64,
    /// Bytes the records the write took back no longer append. Deployed code a destruction takes
    /// back is not among them: its history is the schedule's charge for the deposit.
    pub(crate) taken: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        limit::{KV_UPDATE_POOL, STATE_GROWTH_POOL},
        test_utils::MemoryDatabase,
        LOG_BASE_SIZE, LOG_TOPIC_SIZE, WRITE_RECORD_SIZE,
    };
    use alloy_primitives::{address, Bytes};
    use revm::{
        bytecode::Bytecode,
        context::JournalTr,
        context_interface::{
            context::{SStoreResult, SelfDestructResult},
            journaled_state::account::JournaledAccountTr,
        },
    };

    const SENDER: Address = address!("0000000000000000000000000000000000005e4d");
    const OTHER: Address = address!("0000000000000000000000000000000000000077");
    const DESTRUCTED: Address = address!("0000000000000000000000000000000000000d00");
    const MOVED: SelfDestructResult =
        SelfDestructResult { had_value: true, target_exists: true, previously_destroyed: false };

    fn slot(original: u64, present: u64, new: u64) -> SStoreResult {
        SStoreResult {
            original_value: U256::from(original),
            present_value: U256::from(present),
            new_value: U256::from(new),
        }
    }

    /// A journal holding three accounts as the transaction loaded them, below Amsterdam, where the
    /// EIP-7708 switch alone decides whether a value movement logs.
    fn journal() -> Journal<MemoryDatabase> {
        let db = MemoryDatabase::default()
            .account_balance(SENDER, U256::from(10))
            .account_balance(OTHER, U256::from(4))
            .account_balance(DESTRUCTED, U256::from(5));
        let mut journal = Journal::new(db);
        journal.set_spec_id(SpecId::OSAKA);
        for address in [SENDER, OTHER, DESTRUCTED] {
            journal.load_account(address).unwrap();
        }
        journal
    }

    /// The `SELFDESTRUCT` of `DESTRUCTED` to `target` that moved `balance`.
    const fn destruction(target: Address, balance: u64) -> CompletedWrite<'static> {
        CompletedWrite::SelfDestruct {
            address: DESTRUCTED,
            target,
            balance: U256::from_limbs([balance, 0, 0, 0]),
            result: &MOVED,
        }
    }

    /// What an `SSTORE` of `new` over a slot holding `present`, `original` at the transaction's
    /// start, counts.
    fn sstore_counted(original: u64, present: u64, new: u64) -> Counted {
        let journal = journal();
        let slot = slot(original, present, new);
        let write = CompletedWrite::Sstore { address: OTHER, key: U256::ZERO, result: &slot };
        Accounts::of(&journal).write_counted(&write).0
    }

    /// The opcode prices no record without reading the journal exactly for a call that transfers
    /// no value, of any scheme, and never for a creation; for every such call the journal-read
    /// price is nothing too, whichever account it calls and whatever the caller holds.
    #[test]
    fn test_a_start_makes_no_records_only_when_a_call_transfers_no_value() {
        let calls = [
            FrameStartKind::Call,
            FrameStartKind::CallCode,
            FrameStartKind::DelegateCall,
            FrameStartKind::StaticCall,
        ];
        let creations = [FrameStartKind::Create, FrameStartKind::Create2];
        let journal = journal();
        let accounts = Accounts::of(&journal);
        for (value, expected) in [(U256::ZERO, true), (U256::from(1), false)] {
            for kind in calls {
                assert_eq!(start_makes_no_records(kind, value), expected, "{kind:?} of {value}");
            }
            for kind in creations {
                assert!(!start_makes_no_records(kind, value), "{kind:?} of {value}");
            }
        }
        for kind in calls {
            for target in [OTHER, SENDER, address!("00000000000000000000000000000000000000ee")] {
                let records = accounts.start_records(kind, SENDER, Some(target), U256::ZERO);
                assert_eq!(records, StartRecords::NONE, "{kind:?} to {target}");
            }
        }
        // A transfer that does move value records: the skip is not a price of nothing in general.
        let moved = accounts.start_records(FrameStartKind::Call, SENDER, Some(OTHER), U256::ONE);
        assert_ne!(moved, StartRecords::NONE);
    }

    /// A slot counts once, on its first change in the transaction; writing it back to its
    /// original value takes the record back; every other write counts nothing.
    #[test]
    fn test_sstore_record_rules() {
        let records = |original, present, new| {
            let counted = sstore_counted(original, present, new);
            (counted.added.get(KV_UPDATE_POOL), counted.taken.get(KV_UPDATE_POOL))
        };
        assert_eq!(records(0, 0, 1), (1, 0));
        assert_eq!(records(5, 5, 0), (1, 0));
        assert_eq!(records(0, 1, 0), (0, 1));
        assert_eq!(records(0, 1, 2), (0, 0));
        assert_eq!(records(3, 3, 3), (0, 0));
        assert_eq!(records(3, 4, 4), (0, 0));
        assert_eq!(sstore_counted(0, 0, 1), Counted::records(1, 0, false).with_growth(1, 0));
    }

    /// An `SSTORE` changes state growth by the slot's original, present and new values, on the
    /// spec's eight rows: a rise for original zero, present zero, new non-zero; a fall for original
    /// zero, present non-zero, new zero; nothing on the six others, a slot whose original value is
    /// not zero never changing it.
    #[test]
    fn test_an_sstore_grows_state_only_on_a_slot_whose_original_is_zero() {
        let growth = |original, present, new| {
            let counted = sstore_counted(original, present, new);
            (counted.added.get(STATE_GROWTH_POOL), counted.taken.get(STATE_GROWTH_POOL))
        };
        let rows = [
            ((0, 0, 0), (0, 0)),
            ((0, 0, 1), (1, 0)),
            ((0, 1, 0), (0, 1)),
            ((0, 1, 2), (0, 0)),
            ((5, 0, 0), (0, 0)),
            ((5, 0, 6), (0, 0)),
            ((5, 5, 0), (0, 0)),
            ((5, 5, 6), (0, 0)),
        ];
        for ((original, present, new), expected) in rows {
            assert_eq!(growth(original, present, new), expected, "{original}, {present}, {new}");
        }
    }

    /// A log is 32 bytes for its address, 32 per topic and its data, all of them history; a
    /// transfer log counts what a `LOG3` carrying one word counts.
    #[test]
    fn test_log_bytes() {
        let journal = journal();
        let accounts = Accounts::of(&journal);
        let counted = |topics, data_len| {
            let (counted, history, _) =
                accounts.write_counted(&CompletedWrite::Log { address: OTHER, topics, data_len });
            (counted, history)
        };
        assert_eq!(counted(0, 0), (Counted::bytes(32), HistoryBytes { appended: 32, taken: 0 }));
        assert_eq!(counted(4, 100).0, Counted::bytes(32 + 4 * 32 + 100));
        assert_eq!(counted(1, u64::MAX).0, Counted::bytes(u64::MAX));
        assert_eq!(counted(3, 32).0.bytes_added(), TRANSFER_LOG_SIZE);
        assert_eq!(LOG_BASE_SIZE + 3 * LOG_TOPIC_SIZE + 32, TRANSFER_LOG_SIZE);
    }

    /// A `SELFDESTRUCT`'s records are rebuilt from the journal after the write and the balance it
    /// moved, which tells the cases the after state alone cannot: a destructed account and a
    /// beneficiary the transaction had not changed are two records; a beneficiary drained earlier,
    /// which the move puts back as loaded, takes its record back; a destructed account funded or
    /// drained earlier, changed before the move and after it, adds none of its own; and a
    /// destruction to itself moves nothing. The move's transfer log is data size and no history.
    #[test]
    fn test_a_destruction_counts_what_its_move_changed() {
        let records_bytes = |records| WRITE_RECORD_SIZE * records;

        let mut fresh = journal();
        fresh.set_amsterdam_eip7708_enabled(true);
        fresh.selfdestruct(DESTRUCTED, OTHER, false).unwrap();
        let (counted, history, _) = Accounts::of(&fresh).write_counted(&destruction(OTHER, 5));
        assert_eq!(counted, Counted::records(2, 0, true));
        assert_eq!(history, HistoryBytes { appended: records_bytes(2), taken: 0 });
        fresh.set_amsterdam_eip7708_enabled(false);
        assert_eq!(
            Accounts::of(&fresh).write_counted(&destruction(OTHER, 5)).0,
            Counted::records(2, 0, false),
            "no transfer logs, no log"
        );

        // The destructed account sent one away and the beneficiary all it had: the move of the
        // four left puts the beneficiary back as loaded, and leaves the destructed account changed.
        let mut drained = journal();
        drained.transfer(DESTRUCTED, SENDER, U256::from(1)).unwrap();
        drained.transfer(OTHER, SENDER, U256::from(4)).unwrap();
        drained.selfdestruct(DESTRUCTED, OTHER, false).unwrap();
        let (counted, history, _) = Accounts::of(&drained).write_counted(&destruction(OTHER, 4));
        assert_eq!(counted, Counted::records(0, 1, false));
        assert_eq!(history, HistoryBytes { appended: 0, taken: records_bytes(1) });

        let mut funded = journal();
        funded.transfer(SENDER, DESTRUCTED, U256::from(2)).unwrap();
        funded.selfdestruct(DESTRUCTED, OTHER, false).unwrap();
        assert_eq!(
            Accounts::of(&funded).write_counted(&destruction(OTHER, 7)).0,
            Counted::records(1, 0, false),
            "the beneficiary's record alone"
        );

        // A pre-existing account destructing to itself keeps its balance: nothing changes.
        let to_itself = journal();
        assert_eq!(
            Accounts::of(&to_itself).write_counted(&destruction(DESTRUCTED, 5)).0,
            Counted::NONE
        );
    }

    /// A `SELFDESTRUCT` that moves a non-zero balance to another account that does not exist
    /// creates it: one of state growth with its records. A beneficiary that exists, a destruction
    /// that moves nothing and one to itself create nothing.
    #[test]
    fn test_a_destruction_grows_state_by_the_beneficiary_it_creates() {
        let growth = |target, balance: u64, target_exists| {
            let result = SelfDestructResult {
                had_value: balance != 0,
                target_exists,
                previously_destroyed: false,
            };
            let write = CompletedWrite::SelfDestruct {
                address: DESTRUCTED,
                target,
                balance: U256::from(balance),
                result: &result,
            };
            let journal = journal();
            let counted = Accounts::of(&journal).write_counted(&write).0;
            (counted.added.get(STATE_GROWTH_POOL), counted.taken.get(STATE_GROWTH_POOL))
        };
        let fresh = address!("00000000000000000000000000000000000000f5");
        assert_eq!(growth(fresh, 5, false), (1, 0), "a new beneficiary");
        assert_eq!(growth(OTHER, 5, true), (0, 0), "a beneficiary that exists");
        assert_eq!(growth(fresh, 0, false), (0, 0), "no balance moved");
        assert_eq!(growth(DESTRUCTED, 5, false), (0, 0), "to itself");
    }

    /// What `journal` looks like once `SENDER` created an account at `created` with an endowment
    /// of `endowment`, the account wrote `slots` (key, value) in order and deposited `code`.
    fn created(
        db: MemoryDatabase,
        created: Address,
        endowment: u64,
        slots: &[(u64, u64)],
        code: &[u8],
    ) -> Journal<MemoryDatabase> {
        let mut journal = Journal::new(db.account_balance(SENDER, U256::from(10)));
        journal.load_account(SENDER).unwrap();
        journal.load_account(created).unwrap();
        let spec = journal.cfg.spec;
        journal.create_account_checkpoint(SENDER, created, U256::from(endowment), spec).unwrap();
        for &(key, value) in slots {
            journal.sstore(created, U256::from(key), U256::from(value)).unwrap();
        }
        if !code.is_empty() {
            journal.set_code(created, Bytecode::new_legacy(Bytes::copy_from_slice(code)));
        }
        journal
    }

    /// The first effective destruction of an account created in the transaction gives back the
    /// parts the transaction made: the leaf when the database held no account there, the code it
    /// deposited and each slot it made non-zero, in key order, a slot written back to zero not
    /// among them. A leaf the database held — with a balance, or empty under EIP-161 with storage
    /// — stands. A repeated destruction and one of an account the transaction did not create give
    /// back nothing; from the mark an `SSTORE` of the account's slots counts nothing.
    #[test]
    fn test_a_destruction_gives_back_the_parts_the_transaction_made() {
        let fresh = address!("00000000000000000000000000000000000c4ea7");
        let slots = [(3, 1), (1, 7), (2, 1), (2, 0)];
        let mut made = created(MemoryDatabase::default(), fresh, 0, &slots, &[0x60, 0x00, 0xff]);
        assert!(!Accounts::of(&made).is_destroyed(fresh));
        made.selfdestruct(fresh, OTHER, false).unwrap();
        let accounts = Accounts::of(&made);
        let parts = DestroyedParts {
            leaf: true,
            code: 3,
            slots: vec![U256::from(1), U256::from(3)],
            deleted: 0,
        };
        assert_eq!(accounts.destroyed_parts(fresh, false), Some(parts.clone()));
        assert_eq!(parts.records(), 3);
        assert_eq!(
            parts.counted(),
            Counted::records(0, 3, false).with_growth(0, 3).plus_bytes_taken(3),
            "three records with their bytes, three units of growth, the code's three bytes"
        );
        assert!(accounts.is_destroyed(fresh));
        assert_eq!(accounts.destroyed_parts(fresh, true), None, "a repeated destruction");

        // The write's own count carries the parts; the code's bytes are no record's history.
        let result = SelfDestructResult {
            had_value: false,
            target_exists: true,
            previously_destroyed: false,
        };
        let write = CompletedWrite::SelfDestruct {
            address: fresh,
            target: OTHER,
            balance: U256::ZERO,
            result: &result,
        };
        let (counted, history, destroyed) = accounts.write_counted(&write);
        assert_eq!(destroyed, Some(parts.clone()));
        assert_eq!(counted, parts.counted());
        assert_eq!(history, HistoryBytes { appended: 0, taken: 3 * WRITE_RECORD_SIZE });

        // After the mark a slot's write counts nothing, whichever transition it makes.
        for (original, present, new) in [(0, 0, 1), (0, 1, 0), (0, 7, 8)] {
            let slot = slot(original, present, new);
            let write =
                CompletedWrite::Sstore { address: fresh, key: U256::from(1), result: &slot };
            assert_eq!(
                accounts.write_counted(&write).0,
                Counted::NONE,
                "{original}, {present}, {new}"
            );
        }

        // A leaf the database held stands; the code and the slots still come back.
        let funded = MemoryDatabase::default().account_balance(fresh, U256::from(2));
        let storage =
            MemoryDatabase::default().account_storage(fresh, U256::from(9), U256::from(5));
        for (db, what) in [(funded, "a balance"), (storage, "an empty account with storage")] {
            let mut made = created(db, fresh, 0, &[(1, 1)], &[]);
            made.selfdestruct(fresh, OTHER, false).unwrap();
            assert_eq!(
                Accounts::of(&made).destroyed_parts(fresh, false),
                Some(DestroyedParts {
                    leaf: false,
                    code: 0,
                    slots: vec![U256::from(1)],
                    deleted: 0
                }),
                "{what}"
            );
        }

        // Slots that held a value at the start, loaded before the creation as only a tool loads
        // them: the deletion makes the record of the one that still holds it, and the one the
        // transaction wrote since keeps the record its write made. Neither refills or falls.
        let held = MemoryDatabase::default()
            .account_storage(fresh, U256::from(8), U256::from(4))
            .account_storage(fresh, U256::from(9), U256::from(5));
        let mut loaded = Journal::new(held.account_balance(SENDER, U256::from(10)));
        loaded.load_account(SENDER).unwrap();
        loaded.load_account(fresh).unwrap();
        for key in [8, 9] {
            loaded.sload(fresh, U256::from(key)).unwrap();
        }
        let spec = loaded.cfg.spec;
        loaded.create_account_checkpoint(SENDER, fresh, U256::ZERO, spec).unwrap();
        loaded.sstore(fresh, U256::from(8), U256::from(6)).unwrap();
        loaded.selfdestruct(fresh, OTHER, false).unwrap();
        let parts = Accounts::of(&loaded).destroyed_parts(fresh, false).unwrap();
        assert_eq!(parts, DestroyedParts { leaf: false, code: 0, slots: vec![], deleted: 1 });
        assert_eq!(parts.counted(), Counted::records(1, 0, false));

        // An account the transaction did not create is an ordinary deletion.
        let mut older = journal();
        older.selfdestruct(DESTRUCTED, OTHER, false).unwrap();
        let accounts = Accounts::of(&older);
        assert_eq!(accounts.destroyed_parts(DESTRUCTED, false), None);
        assert!(!accounts.is_destroyed(DESTRUCTED));
    }

    /// An account's delta: a record when a write changes it, a record back when a write returns
    /// it, and nothing when it stays as it was, changed or not.
    #[test]
    fn test_an_accounts_delta_follows_its_change() {
        assert_eq!(RecordDelta::of_account(false, true), RecordDelta { added: 1, taken: 0 });
        assert_eq!(RecordDelta::of_account(true, false), RecordDelta { added: 0, taken: 1 });
        assert_eq!(RecordDelta::of_account(true, true), RecordDelta::NONE);
        assert_eq!(RecordDelta::of_account(false, false), RecordDelta::NONE);
        assert_eq!(
            RecordDelta::of_account(false, true).with(RecordDelta::of_account(true, false)),
            RecordDelta { added: 1, taken: 1 }
        );
    }

    /// The records a start's opcode prices follow the net change of both sides of a transfer, on
    /// the balances the transfer leaves them; a transfer to the same account, of nothing, or of
    /// more than the sender holds records nothing. A creation records its created account and its
    /// creator's nonce only where the transaction had not changed them, and keeps the creator's off
    /// the created frame.
    #[test]
    fn test_a_starts_priced_records_follow_the_net_change() {
        let mut journal = journal();
        let accounts = Accounts::of(&journal);
        let both = accounts.transfer_records(OTHER, DESTRUCTED, U256::from(1));
        assert_eq!(both.transfer, RecordDelta { added: 2, taken: 0 });
        assert_eq!(accounts.transfer_records(OTHER, OTHER, U256::from(1)), StartRecords::NONE);
        assert_eq!(accounts.transfer_records(OTHER, DESTRUCTED, U256::ZERO), StartRecords::NONE);
        assert_eq!(accounts.transfer_records(OTHER, DESTRUCTED, U256::from(5)), StartRecords::NONE);

        // OTHER sent 1 to DESTRUCTED: sending it back returns both.
        journal.transfer(OTHER, DESTRUCTED, U256::from(1)).unwrap();
        let back = Accounts::of(&journal).transfer_records(DESTRUCTED, OTHER, U256::from(1));
        assert_eq!(back.transfer, RecordDelta { added: 0, taken: 2 });

        let created = address!("00000000000000000000000000000000000c4ea7");
        let accounts = Accounts::of(&journal);
        let creation = accounts.creation_records(SENDER, Some(created), U256::from(1));
        assert_eq!(
            creation,
            StartRecords { transfer: RecordDelta::NONE, created: true, creator: true }
        );
        assert_eq!(creation.on_frame(), RecordDelta { added: 1, taken: 0 });
        let onto_changed = accounts.creation_records(OTHER, Some(DESTRUCTED), U256::ZERO);
        assert!(!onto_changed.created && !onto_changed.creator);
        assert_eq!(accounts.creation_records(SENDER, None, U256::ZERO), StartRecords::NONE);
        assert_eq!(
            accounts.creation_records(SENDER, Some(created), U256::from(11)),
            StartRecords::NONE,
            "an endowment the creator cannot fund"
        );
    }

    /// What a start made is read off what revm did, against its accounts as they stood before it:
    /// a transfer to an empty account makes both sides' records, one account of state growth and
    /// the transfer log revm journaled since; sending the value back takes both records back and
    /// grows nothing, the recipient no longer being empty; a start that moved nothing, or whose
    /// move revm took back, made nothing; a log another account emitted is no transfer log; and an
    /// account named twice is counted once.
    #[test]
    fn test_a_starts_records_are_read_off_what_revm_did() {
        let empty = address!("00000000000000000000000000000000000e3e70");
        let mut journal = journal();
        journal.set_amsterdam_eip7708_enabled(true);
        journal.load_account(empty).unwrap();
        let before = |journal: &Journal<MemoryDatabase>, first, second| {
            Accounts::of(journal).start_before(first, Some(second), journal.logs.len())
        };
        let made =
            |journal: &Journal<MemoryDatabase>, before| Accounts::of(journal).start_made(&before);

        let funding = before(&journal, OTHER, empty);
        assert_eq!(made(&journal, funding), (RecordDelta::NONE, Counted::NONE), "nothing moved");
        journal.transfer(OTHER, empty, U256::from(1)).unwrap();
        assert_eq!(
            made(&journal, funding),
            (RecordDelta { added: 2, taken: 0 }, Counted::records(2, 0, true).with_growth(1, 0))
        );

        let back = before(&journal, empty, OTHER);
        journal.transfer(empty, OTHER, U256::from(1)).unwrap();
        assert_eq!(
            made(&journal, back),
            (RecordDelta { added: 0, taken: 2 }, Counted::records(0, 2, true))
        );

        let taken_back = before(&journal, OTHER, DESTRUCTED);
        let checkpoint = journal.checkpoint();
        journal.transfer(OTHER, DESTRUCTED, U256::from(1)).unwrap();
        journal.checkpoint_revert(checkpoint);
        assert_eq!(made(&journal, taken_back), (RecordDelta::NONE, Counted::NONE), "taken back");

        let logged = before(&journal, OTHER, DESTRUCTED);
        journal.log(Log::new_unchecked(OTHER, vec![], Bytes::new()));
        assert_eq!(made(&journal, logged), (RecordDelta::NONE, Counted::NONE), "a contract's log");

        let itself = before(&journal, SENDER, SENDER);
        journal.load_account_mut(SENDER).unwrap().data.bump_nonce();
        assert_eq!(
            made(&journal, itself),
            (RecordDelta { added: 1, taken: 0 }, Counted::records(1, 0, false))
        );
    }

    /// A creator's record is read off what revm did: nothing while its nonce is where it was
    /// before the creation; once revm bumped it, a record when the bump changed an account the
    /// transaction had not changed, and one account of state growth when it made an empty one. A
    /// nonce set back leaves no record, unless the account keeps another change.
    #[test]
    fn test_a_creators_record_is_read_off_the_bump() {
        let empty = address!("00000000000000000000000000000000000e3e70");
        let mut journal = journal();
        journal.load_account(empty).unwrap();
        journal.transfer(OTHER, DESTRUCTED, U256::from(1)).unwrap();
        let accounts = Accounts::of(&journal);
        let [sender, changed, empty_before] =
            [SENDER, DESTRUCTED, empty].map(|address| accounts.creator_before(address));
        for before in [sender, changed, empty_before] {
            assert!(!accounts.bumped(&before), "{before:?}: nothing moved yet");
        }

        for address in [SENDER, DESTRUCTED, empty] {
            assert!(journal.load_account_mut(address).unwrap().data.bump_nonce());
        }
        let accounts = Accounts::of(&journal);
        let record = Counted::records(1, 0, false);
        for (before, counted) in
            [(sender, record), (changed, Counted::NONE), (empty_before, record.with_growth(1, 0))]
        {
            assert!(accounts.bumped(&before), "{before:?}");
            assert_eq!(accounts.creator_counted(&before), counted, "{before:?}");
        }

        // A nonce set back: the unchanged account leaves no record, the one that keeps a balance
        // change stands as it was.
        for address in [SENDER, DESTRUCTED] {
            journal.load_account_mut(address).unwrap().data.set_nonce(0);
        }
        let accounts = Accounts::of(&journal);
        assert_eq!(accounts.creator_counted(&sender), Counted::NONE);
        assert_eq!(accounts.creator_counted(&changed), Counted::NONE);
    }
}
