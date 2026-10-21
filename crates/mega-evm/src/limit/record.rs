//! What a write an opcode completed counts.
//!
//! revm hands the Host the facts of every write `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT`
//! complete, once the write is in the journal and every charge the opcode makes has been made
//! ([`Host::write_completed`](revm::context_interface::Host::write_completed)). A failed opcode
//! offers nothing, so nothing here is counted for a write its failure takes back.

use revm::context_interface::host::CompletedWrite;

use super::{log_history_bytes, LimitUsage, WRITE_RECORD};

/// How a write moves the record count: the records it creates, for accounts it changes from what
/// the transaction loaded, and the records it takes back, for accounts it returns to that.
///
/// A value transfer moves at most one record per side, and a `SELFDESTRUCT` one per account it
/// writes. A creation's records are kept apart from this delta
/// ([`FrameStartRecords`](super::FrameStartRecords)): its created account, which the creation
/// leaves changed and never returns, and its creator's nonce bump, which lands on the creating
/// frame.
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

/// What a completed write keeps, as the journal's ledger counts it: what it adds, what it takes
/// back, and the log bytes among what it adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriteCount {
    /// What the write adds: records, a log's bytes, a transfer log.
    pub(crate) added: LimitUsage,
    /// What the write takes back: the records of accounts and slots returned to what the
    /// transaction loaded.
    pub(crate) taken: LimitUsage,
    /// The bytes of [`added`](Self::added) that are a log's.
    pub(crate) log_bytes: u64,
}

impl WriteCount {
    /// Nothing kept.
    #[cfg(test)]
    pub(crate) const NONE: Self =
        Self { added: LimitUsage::ZERO, taken: LimitUsage::ZERO, log_bytes: 0 };
}

/// What committing the write `write` counts, per site, as records added and taken back and as
/// bytes appended and taken back:
///
/// - `SSTORE`: the first change of a slot in the transaction is one write record; writing the slot
///   back to its original value takes the record back.
/// - `LOG`: its bytes, one base record for the address, one per topic and the data.
/// - `SELFDESTRUCT`: `destruction`, the records of the accounts the move changed and returned,
///   which the journal observed as it made the write: the beneficiary, and the destructed account
///   whose balance the move clears.
pub(crate) fn write_effect(
    write: &CompletedWrite<'_>,
    destruction: RecordDelta,
) -> (LimitUsage, LimitUsage) {
    match *write {
        CompletedWrite::Sstore { result: slot, .. } => {
            if slot.is_original_eq_present() {
                if slot.is_original_eq_new() {
                    (LimitUsage::ZERO, LimitUsage::ZERO)
                } else {
                    (WRITE_RECORD, LimitUsage::ZERO)
                }
            } else if slot.is_original_eq_new() {
                (LimitUsage::ZERO, WRITE_RECORD)
            } else {
                (LimitUsage::ZERO, LimitUsage::ZERO)
            }
        }
        CompletedWrite::Log { topics, data_len, .. } => (
            LimitUsage {
                data_size: log_history_bytes(u64::from(topics), data_len),
                write_records: 0,
            },
            LimitUsage::ZERO,
        ),
        CompletedWrite::SelfDestruct { .. } => {
            (WRITE_RECORD.times(destruction.added), WRITE_RECORD.times(destruction.taken))
        }
    }
}

/// Whether the write moved value to another account: a `SELFDESTRUCT` of an account that had a
/// balance, to a beneficiary other than itself. revm journals an EIP-7708 transfer log for exactly
/// that move, whoever the beneficiary is — the sender included — and none for a destruction to
/// itself, which burns the balance on Satin's base spec.
pub(crate) fn write_moves_value(write: &CompletedWrite<'_>) -> bool {
    match *write {
        CompletedWrite::SelfDestruct { address, target, result, .. } => {
            result.had_value && target != address
        }
        CompletedWrite::Sstore { .. } | CompletedWrite::Log { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LOG_BASE_SIZE, LOG_TOPIC_SIZE};
    use alloy_primitives::{address, Address, U256};
    use revm::context_interface::context::{SStoreResult, SelfDestructResult};

    const SENDER: Address = address!("0000000000000000000000000000000000005e4d");
    const OTHER: Address = address!("0000000000000000000000000000000000000077");
    const DESTRUCTED: Address = address!("0000000000000000000000000000000000000d00");

    fn slot(original: u64, present: u64, new: u64) -> SStoreResult {
        SStoreResult {
            original_value: U256::from(original),
            present_value: U256::from(present),
            new_value: U256::from(new),
        }
    }

    fn sstore(slot: &SStoreResult) -> CompletedWrite<'_> {
        CompletedWrite::Sstore { address: OTHER, key: U256::ZERO, result: slot }
    }

    const fn log(topics: u8, data_len: u64) -> CompletedWrite<'static> {
        CompletedWrite::Log { address: OTHER, topics, data_len }
    }

    fn destruction(had_value: bool) -> SelfDestructResult {
        SelfDestructResult { had_value, target_exists: true, previously_destroyed: false }
    }

    fn selfdestruct(result: &SelfDestructResult, target: Address) -> CompletedWrite<'_> {
        CompletedWrite::SelfDestruct { address: DESTRUCTED, target, balance: U256::ZERO, result }
    }

    const RECORD: (LimitUsage, LimitUsage) = (WRITE_RECORD, LimitUsage::ZERO);
    const TAKEN: (LimitUsage, LimitUsage) = (LimitUsage::ZERO, WRITE_RECORD);
    const NOTHING: (LimitUsage, LimitUsage) = (LimitUsage::ZERO, LimitUsage::ZERO);

    /// A slot counts once, on its first change in the transaction; writing it back to its
    /// original value takes the record back; every other write counts nothing.
    #[test]
    fn test_sstore_record_rules() {
        let effect = |original, present, new| {
            write_effect(&sstore(&slot(original, present, new)), RecordDelta::NONE)
        };
        assert_eq!(effect(0, 0, 1), RECORD);
        assert_eq!(effect(5, 5, 0), RECORD);
        assert_eq!(effect(0, 1, 0), TAKEN);
        assert_eq!(effect(0, 1, 2), NOTHING);
        assert_eq!(effect(3, 3, 3), NOTHING);
        assert_eq!(effect(3, 4, 4), NOTHING);
    }

    /// A log is 32 bytes for its address, 32 per topic and its data.
    #[test]
    fn test_log_bytes() {
        let effect = |topics, data_len| write_effect(&log(topics, data_len), RecordDelta::NONE);
        assert_eq!(
            effect(0, 0),
            (LimitUsage { data_size: 32, write_records: 0 }, LimitUsage::ZERO)
        );
        assert_eq!(
            effect(4, 100),
            (LimitUsage { data_size: 32 + 4 * 32 + 100, write_records: 0 }, LimitUsage::ZERO)
        );
        assert_eq!(
            effect(1, u64::MAX),
            (LimitUsage { data_size: u64::MAX, write_records: 0 }, LimitUsage::ZERO)
        );
    }

    /// A `SELFDESTRUCT` counts the records the journal saw its move make and take back, whatever
    /// the beneficiary: the sender is told apart by its account, which the transaction changed,
    /// not by its address.
    #[test]
    fn test_selfdestruct_counts_the_records_the_journal_observed() {
        let moved = destruction(true);
        let effect = |target, delta| write_effect(&selfdestruct(&moved, target), delta);
        assert_eq!(
            effect(OTHER, RecordDelta { added: 2, taken: 0 }),
            (WRITE_RECORD.times(2), LimitUsage::ZERO)
        );
        assert_eq!(effect(OTHER, RecordDelta { added: 1, taken: 1 }), (WRITE_RECORD, WRITE_RECORD));
        assert_eq!(effect(SENDER, RecordDelta { added: 1, taken: 0 }), RECORD);
        assert_eq!(effect(DESTRUCTED, RecordDelta::NONE), NOTHING);
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

    /// Value moves to another account only when a destructed account had a balance and a
    /// beneficiary other than itself, the sender included; a write or a log moves none.
    #[test]
    fn test_only_a_destruction_to_another_account_moves_value() {
        let moves =
            |had_value, target| write_moves_value(&selfdestruct(&destruction(had_value), target));
        assert!(moves(true, OTHER));
        assert!(moves(true, SENDER), "the body's record is not the log");
        assert!(!moves(true, DESTRUCTED), "a destruction to itself burns");
        assert!(!moves(false, OTHER), "nothing to move");
        assert!(!write_moves_value(&sstore(&slot(0, 0, 1))));
        assert!(!write_moves_value(&log(3, 32)));
    }

    /// A transfer log counts what a `LOG3` carrying one word counts: the same rule, not a number
    /// of its own.
    #[test]
    fn test_a_transfer_log_counts_what_a_log3_of_one_word_counts() {
        assert_eq!(write_effect(&log(3, 32), RecordDelta::NONE).0, crate::limit::TRANSFER_LOG,);
        assert_eq!(LOG_BASE_SIZE + 3 * LOG_TOPIC_SIZE + 32, crate::limit::TRANSFER_LOG.data_size);
    }
}
