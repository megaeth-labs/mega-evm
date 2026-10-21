//! What a write counts: the write records it makes and takes back, by the net change of what it
//! writes, and its other bytes.
//!
//! An account's write record exists while the account's info — its nonce, balance or code —
//! differs from what the transaction loaded, revm's original info, which revm sets at the
//! account's first load in the transaction; a storage slot's while its value differs from its
//! original value. A write that changes an unchanged account or slot makes the record, a write that
//! returns one to its original takes it back, and a write to one already changed adds nothing.
//!
//! The facts are read off revm's journal as it stands where a hook runs: before a frame's start,
//! which is where its records are predicted, and after a completed write, where a `SELFDESTRUCT`'s
//! accounts are rebuilt as they were before it from the balance it moved.

use revm::{
    context_interface::host::{CompletedWrite, FrameStartKind},
    primitives::{Address, U256},
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

/// The write records a frame's start makes: a call's value transfer's, and a creation's created
/// account and creator nonce; and whether it journals a transfer log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartRecords {
    /// The records a call's value transfer makes and takes back: the accounts it changes from what
    /// the transaction loaded, and those it returns to that.
    pub(crate) transfer: RecordDelta,
    /// Whether a creation records the account it creates: one the transaction had not changed.
    /// A creation sets the account's nonce to one, so it never returns the account.
    pub(crate) created: bool,
    /// Whether a creation records its creator's nonce: the bump changes an account the transaction
    /// had not changed. The record is the creator's, which the creation's failure leaves with it.
    pub(crate) creator: bool,
    /// Whether the start journals an EIP-7708 transfer log: value moves to another account.
    pub(crate) transfer_log: bool,
}

impl StartRecords {
    /// No record and no log.
    pub(crate) const NONE: Self =
        Self { transfer: RecordDelta::NONE, created: false, creator: false, transfer_log: false };

    /// The records the start makes on the started frame, which its failure takes back: a
    /// transfer's, and the created account. The creator's is not among them.
    pub(crate) const fn on_frame(self) -> RecordDelta {
        RecordDelta { added: self.transfer.added + self.created as u64, taken: self.transfer.taken }
    }

    /// What the start counts on the started frame: its records there, and its transfer log.
    pub(crate) const fn counted(self) -> Counted {
        let on_frame = self.on_frame();
        Counted::records(on_frame.added, on_frame.taken, self.transfer_log)
    }
}

/// The accounts the running transaction holds, as revm's journal holds them, read for the records
/// a write makes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Accounts<'a> {
    state: &'a EvmState,
    transaction_id: TransactionId,
    /// Whether a value movement journals an EIP-7708 transfer log.
    transfer_logs: bool,
}

/// Whether `info` differs from `original` in what a write record stands for: the nonce, the
/// balance or the code.
fn differs(info: &AccountInfo, original: &AccountInfo) -> bool {
    info.nonce != original.nonce ||
        info.balance != original.balance ||
        info.code_hash != original.code_hash
}

impl<'a> Accounts<'a> {
    /// The accounts `journal` holds, in a transaction whose value movements journal transfer logs
    /// or not (`transfer_logs`).
    pub(crate) fn of<DB>(journal: &'a Journal<DB>, transfer_logs: bool) -> Self {
        Self { state: &journal.state, transaction_id: journal.transaction_id, transfer_logs }
    }

    /// The account at `address` as the transaction loaded it and holds it now: `None` for one
    /// the transaction has not loaded, which it has not changed either.
    fn loaded(&self, address: Address) -> Option<&'a Account> {
        self.state.get(&address).filter(|account| account.transaction_id == self.transaction_id)
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

    /// The current balance of the account at `address`; nothing for one the transaction has not
    /// loaded.
    fn balance(&self, address: Address) -> U256 {
        self.loaded(address).map_or(U256::ZERO, |account| account.info.balance)
    }

    /// The records a transfer of `value` from `from` to `to` makes and takes back, as the journal's
    /// state stands, and whether it journals a transfer log: nothing for a transfer to the same
    /// account or of nothing, which revm moves nothing for, and nothing for a value `from` cannot
    /// fund, which revm refuses.
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
        StartRecords { transfer, transfer_log: self.transfer_logs, ..StartRecords::NONE }
    }

    /// The records a creation by `creator` at `created` with an endowment of `value` makes, before
    /// revm bumps the creator's nonce: the creator's nonce, when the bump changes an account the
    /// transaction had not changed; the account it creates, when the transaction had not changed
    /// it; and a transfer log for an endowment. Nothing for an endowment `creator` cannot fund,
    /// which revm refuses before it bumps the nonce, nor for a creation that takes no address.
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
            transfer_log: self.transfer_logs && !value.is_zero(),
        }
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

    /// What the completed write `write` counts, and the history bytes it appends and takes back:
    ///
    /// - `SSTORE`: the first change of a slot in the transaction is one write record; writing the
    ///   slot back to its original value takes the record back;
    /// - `LOG`: its bytes, one base record for the address, one per topic and the data, which are
    ///   history beside the records;
    /// - `SELFDESTRUCT`: the records of the accounts the move changed and returned — the
    ///   beneficiary, and the destructed account whose balance the move clears — and, when it moved
    ///   value to another account, the EIP-7708 transfer log revm journaled for the move, which is
    ///   data size and no history.
    pub(crate) fn write_counted(&self, write: &CompletedWrite<'_>) -> (Counted, HistoryBytes) {
        let counted = match *write {
            CompletedWrite::Sstore { result: slot, .. } => {
                if slot.is_original_eq_present() {
                    if slot.is_original_eq_new() {
                        Counted::NONE
                    } else {
                        Counted::records(1, 0, false)
                    }
                } else if slot.is_original_eq_new() {
                    Counted::records(0, 1, false)
                } else {
                    Counted::NONE
                }
            }
            CompletedWrite::Log { topics, data_len, .. } => {
                Counted::bytes(log_history_bytes(u64::from(topics), data_len))
            }
            CompletedWrite::SelfDestruct { address, target, balance, result } => {
                let records = self.destruction_records(address, target, balance);
                let moved = result.had_value && target != address;
                let logged = self.transfer_logs && moved;
                Counted::records(records.added, records.taken, logged)
            }
        };
        let log = if counted.transfer_log { TRANSFER_LOG_SIZE } else { 0 };
        (counted, HistoryBytes { appended: counted.added - log, taken: counted.taken })
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
    /// Bytes the records the write took back no longer append.
    pub(crate) taken: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, LOG_BASE_SIZE, LOG_TOPIC_SIZE, WRITE_RECORD_SIZE};
    use alloy_primitives::address;
    use revm::{
        context::JournalTr,
        context_interface::context::{SStoreResult, SelfDestructResult},
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

    /// A journal holding three accounts as the transaction loaded them.
    fn journal() -> Journal<MemoryDatabase> {
        let db = MemoryDatabase::default()
            .account_balance(SENDER, U256::from(10))
            .account_balance(OTHER, U256::from(4))
            .account_balance(DESTRUCTED, U256::from(5));
        let mut journal = Journal::new(db);
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

    /// A slot counts once, on its first change in the transaction; writing it back to its
    /// original value takes the record back; every other write counts nothing.
    #[test]
    fn test_sstore_record_rules() {
        let journal = journal();
        let accounts = Accounts::of(&journal, false);
        let counted = |original, present, new| {
            let slot = slot(original, present, new);
            let write = CompletedWrite::Sstore { address: OTHER, key: U256::ZERO, result: &slot };
            accounts.write_counted(&write).0
        };
        assert_eq!(counted(0, 0, 1), Counted::records(1, 0, false));
        assert_eq!(counted(5, 5, 0), Counted::records(1, 0, false));
        assert_eq!(counted(0, 1, 0), Counted::records(0, 1, false));
        assert_eq!(counted(0, 1, 2), Counted::NONE);
        assert_eq!(counted(3, 3, 3), Counted::NONE);
        assert_eq!(counted(3, 4, 4), Counted::NONE);
    }

    /// A log is 32 bytes for its address, 32 per topic and its data, all of them history; a
    /// transfer log counts what a `LOG3` carrying one word counts.
    #[test]
    fn test_log_bytes() {
        let journal = journal();
        let accounts = Accounts::of(&journal, false);
        let counted = |topics, data_len| {
            accounts.write_counted(&CompletedWrite::Log { address: OTHER, topics, data_len })
        };
        assert_eq!(counted(0, 0), (Counted::bytes(32), HistoryBytes { appended: 32, taken: 0 }));
        assert_eq!(counted(4, 100).0, Counted::bytes(32 + 4 * 32 + 100));
        assert_eq!(counted(1, u64::MAX).0, Counted::bytes(u64::MAX));
        assert_eq!(counted(3, 32).0.added, TRANSFER_LOG_SIZE);
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
        fresh.selfdestruct(DESTRUCTED, OTHER, false).unwrap();
        let (counted, history) = Accounts::of(&fresh, true).write_counted(&destruction(OTHER, 5));
        assert_eq!(counted, Counted::records(2, 0, true));
        assert_eq!(history, HistoryBytes { appended: records_bytes(2), taken: 0 });
        assert_eq!(
            Accounts::of(&fresh, false).write_counted(&destruction(OTHER, 5)).0,
            Counted::records(2, 0, false),
            "no transfer logs, no log"
        );

        // The destructed account sent one away and the beneficiary all it had: the move of the
        // four left puts the beneficiary back as loaded, and leaves the destructed account changed.
        let mut drained = journal();
        drained.transfer(DESTRUCTED, SENDER, U256::from(1)).unwrap();
        drained.transfer(OTHER, SENDER, U256::from(4)).unwrap();
        drained.selfdestruct(DESTRUCTED, OTHER, false).unwrap();
        let (counted, history) =
            Accounts::of(&drained, false).write_counted(&destruction(OTHER, 4));
        assert_eq!(counted, Counted::records(0, 1, false));
        assert_eq!(history, HistoryBytes { appended: 0, taken: records_bytes(1) });

        let mut funded = journal();
        funded.transfer(SENDER, DESTRUCTED, U256::from(2)).unwrap();
        funded.selfdestruct(DESTRUCTED, OTHER, false).unwrap();
        assert_eq!(
            Accounts::of(&funded, false).write_counted(&destruction(OTHER, 7)).0,
            Counted::records(1, 0, false),
            "the beneficiary's record alone"
        );

        // A pre-existing account destructing to itself keeps its balance: nothing changes.
        let to_itself = journal();
        assert_eq!(
            Accounts::of(&to_itself, true).write_counted(&destruction(DESTRUCTED, 5)).0,
            Counted::NONE
        );
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

    /// A transfer's records follow the net change of both sides, on the balances the transfer
    /// leaves them; a transfer to the same account, of nothing, or of more than the sender holds
    /// records nothing and journals no log. A creation records its created account and its
    /// creator's nonce only where the transaction had not changed them, keeps the creator's off
    /// the created frame, and journals a log for an endowment.
    #[test]
    fn test_a_starts_records_follow_the_net_change() {
        let mut journal = journal();
        let accounts = Accounts::of(&journal, true);
        let both = accounts.transfer_records(OTHER, DESTRUCTED, U256::from(1));
        assert_eq!(both.transfer, RecordDelta { added: 2, taken: 0 });
        assert!(both.transfer_log);
        assert_eq!(accounts.transfer_records(OTHER, OTHER, U256::from(1)), StartRecords::NONE);
        assert_eq!(accounts.transfer_records(OTHER, DESTRUCTED, U256::ZERO), StartRecords::NONE);
        assert_eq!(accounts.transfer_records(OTHER, DESTRUCTED, U256::from(5)), StartRecords::NONE);

        // OTHER sent 1 to DESTRUCTED: sending it back returns both.
        journal.transfer(OTHER, DESTRUCTED, U256::from(1)).unwrap();
        let back = Accounts::of(&journal, true).transfer_records(DESTRUCTED, OTHER, U256::from(1));
        assert_eq!(back.transfer, RecordDelta { added: 0, taken: 2 });
        assert_eq!(back.counted(), Counted::records(0, 2, true));

        let created = address!("00000000000000000000000000000000000c4ea7");
        let accounts = Accounts::of(&journal, true);
        let creation = accounts.creation_records(SENDER, Some(created), U256::from(1));
        assert_eq!(
            creation,
            StartRecords {
                transfer: RecordDelta::NONE,
                created: true,
                creator: true,
                transfer_log: true
            }
        );
        assert_eq!(creation.on_frame(), RecordDelta { added: 1, taken: 0 });
        let onto_changed = accounts.creation_records(OTHER, Some(DESTRUCTED), U256::ZERO);
        assert!(!onto_changed.created && !onto_changed.creator && !onto_changed.transfer_log);
        assert_eq!(accounts.creation_records(SENDER, None, U256::ZERO), StartRecords::NONE);
        assert_eq!(
            accounts.creation_records(SENDER, Some(created), U256::from(11)),
            StartRecords::NONE,
            "an endowment the creator cannot fund"
        );
    }
}
