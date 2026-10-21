//! What a write an opcode completed counts.
//!
//! revm hands the Host the facts of every write `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT`
//! complete, once the write is in the journal and every charge the opcode makes has been made
//! ([`Host::write_completed`](revm::context_interface::Host::write_completed)). A failed opcode
//! offers nothing, so nothing here is counted for a write its failure takes back.

use alloy_primitives::Address;
use revm::context_interface::host::CompletedWrite;

use super::{log_history_bytes, LimitUsage, WRITE_RECORD};

/// How a committed record changes the running frame's usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordEffect {
    /// Nothing to count.
    None,
    /// Count this usage.
    Record(LimitUsage),
    /// Take this usage back.
    Refund(LimitUsage),
}

/// The history bytes a committed record appends, or takes back.
///
/// They are the record's data size and nothing else: what a log or a write record weighs in a
/// block is what the data-size limit meters it at, so the two cannot drift apart. That is the
/// pairing, and it holds per record; a transaction's two totals part where the byte table says
/// they do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryBytes {
    /// Nothing to charge.
    None,
    /// Bytes the record appends.
    Appended(u64),
    /// Bytes a record that was taken back no longer appends.
    Taken(u64),
}

/// What a completed write keeps, as the journal's ledger counts it: what it adds, what it takes
/// back, and the log bytes among what it adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriteCount {
    /// What the write adds: a record, a log's bytes, a transfer log.
    pub(crate) added: LimitUsage,
    /// What the write takes back: the record of a slot written back to its original value.
    pub(crate) taken: LimitUsage,
    /// The bytes of [`added`](Self::added) that are a log's.
    pub(crate) log_bytes: u64,
}

impl WriteCount {
    /// Nothing kept.
    pub(crate) const NONE: Self =
        Self { added: LimitUsage::ZERO, taken: LimitUsage::ZERO, log_bytes: 0 };
}

impl RecordEffect {
    /// The history bytes this effect appends or takes back.
    pub(crate) const fn history_bytes(self) -> HistoryBytes {
        match self {
            Self::None => HistoryBytes::None,
            Self::Record(usage) => HistoryBytes::Appended(usage.data_size),
            Self::Refund(usage) => HistoryBytes::Taken(usage.data_size),
        }
    }
}

/// What committing the write `write` counts, per site:
///
/// - `SSTORE`: the first change of a slot in the transaction is one write record; writing the slot
///   back to its original value takes the record back.
/// - `LOG`: its bytes, one base record for the address, one per topic and the data.
/// - `SELFDESTRUCT`: one write record for the beneficiary, when value moves to another account than
///   the destructed one and the transaction's `sender`, whose account the transaction body counts.
pub(crate) fn write_effect(write: &CompletedWrite<'_>, sender: Address) -> RecordEffect {
    match *write {
        CompletedWrite::Sstore { result: slot, .. } => {
            if slot.is_original_eq_present() {
                if slot.is_original_eq_new() {
                    RecordEffect::None
                } else {
                    RecordEffect::Record(WRITE_RECORD)
                }
            } else if slot.is_original_eq_new() {
                RecordEffect::Refund(WRITE_RECORD)
            } else {
                RecordEffect::None
            }
        }
        CompletedWrite::Log { topics, data_len, .. } => RecordEffect::Record(LimitUsage {
            data_size: log_history_bytes(u64::from(topics), data_len),
            write_records: 0,
        }),
        CompletedWrite::SelfDestruct { address, target, result } => {
            if result.had_value && target != address && target != sender {
                RecordEffect::Record(WRITE_RECORD)
            } else {
                RecordEffect::None
            }
        }
    }
}

/// Whether the write moved value to another account: a `SELFDESTRUCT` of an account that had a
/// balance, to a beneficiary other than itself. revm journals an EIP-7708 transfer log for exactly
/// that move, whoever the beneficiary is — the sender included — and none for a destruction to
/// itself, which burns the balance on Satin's base spec.
pub(crate) fn write_moves_value(write: &CompletedWrite<'_>) -> bool {
    match *write {
        CompletedWrite::SelfDestruct { address, target, result } => {
            result.had_value && target != address
        }
        CompletedWrite::Sstore { .. } | CompletedWrite::Log { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LOG_BASE_SIZE, LOG_TOPIC_SIZE, WRITE_RECORD_SIZE};
    use alloy_primitives::{address, U256};
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
        CompletedWrite::SelfDestruct { address: DESTRUCTED, target, result }
    }

    /// A slot counts once, on its first change in the transaction; writing it back to its
    /// original value takes the record back; every other write counts nothing.
    #[test]
    fn test_sstore_record_rules() {
        let effect =
            |original, present, new| write_effect(&sstore(&slot(original, present, new)), SENDER);
        assert_eq!(effect(0, 0, 1), RecordEffect::Record(WRITE_RECORD));
        assert_eq!(effect(5, 5, 0), RecordEffect::Record(WRITE_RECORD));
        assert_eq!(effect(0, 1, 0), RecordEffect::Refund(WRITE_RECORD));
        assert_eq!(effect(0, 1, 2), RecordEffect::None);
        assert_eq!(effect(3, 3, 3), RecordEffect::None);
        assert_eq!(effect(3, 4, 4), RecordEffect::None);
    }

    /// A log is 32 bytes for its address, 32 per topic and its data.
    #[test]
    fn test_log_bytes() {
        let effect = |topics, data_len| write_effect(&log(topics, data_len), SENDER);
        assert_eq!(
            effect(0, 0),
            RecordEffect::Record(LimitUsage { data_size: 32, write_records: 0 })
        );
        assert_eq!(
            effect(4, 100),
            RecordEffect::Record(LimitUsage { data_size: 32 + 4 * 32 + 100, write_records: 0 })
        );
        assert_eq!(
            effect(1, u64::MAX),
            RecordEffect::Record(LimitUsage { data_size: u64::MAX, write_records: 0 })
        );
    }

    /// A record's history bytes are its data size, at every site: a log's own bytes, a write
    /// record's forty, and nothing for a write that leaves no record.
    #[test]
    fn test_history_bytes_are_the_records_data_size() {
        let history = |write: &CompletedWrite<'_>| write_effect(write, SENDER).history_bytes();
        assert_eq!(history(&sstore(&slot(0, 0, 1))), HistoryBytes::Appended(WRITE_RECORD_SIZE));
        assert_eq!(history(&sstore(&slot(0, 1, 0))), HistoryBytes::Taken(WRITE_RECORD_SIZE));
        assert_eq!(history(&sstore(&slot(3, 3, 3))), HistoryBytes::None);
        assert_eq!(
            history(&log(3, 32)),
            HistoryBytes::Appended(LOG_BASE_SIZE + 3 * LOG_TOPIC_SIZE + 32)
        );
        assert_eq!(
            history(&selfdestruct(&destruction(true), OTHER)),
            HistoryBytes::Appended(WRITE_RECORD_SIZE)
        );
    }

    /// Only value that moves to another account is a beneficiary write.
    #[test]
    fn test_selfdestruct_record_rules() {
        let effect = |had_value, target| {
            write_effect(&selfdestruct(&destruction(had_value), target), SENDER)
        };
        assert_eq!(effect(true, OTHER), RecordEffect::Record(WRITE_RECORD));
        assert_eq!(effect(true, DESTRUCTED), RecordEffect::None);
        assert_eq!(effect(false, OTHER), RecordEffect::None);
        assert_eq!(effect(false, DESTRUCTED), RecordEffect::None);
        assert_eq!(effect(true, SENDER), RecordEffect::None, "the body counts the sender");
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
        assert_eq!(
            write_effect(&log(3, 32), SENDER),
            RecordEffect::Record(crate::limit::TRANSFER_LOG)
        );
    }
}
