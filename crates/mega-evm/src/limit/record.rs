//! The facts the Host observes for the opcode that is running.

use revm::interpreter::SStoreResult;

use super::{LimitUsage, LOG_BASE_SIZE, LOG_TOPIC_SIZE, WRITE_RECORD};

/// What the Host saw an opcode do, staged until the opcode completes.
///
/// The Host only observes: it stages the facts revm hands it and records nothing. The opcode's
/// wrapper commits the staged record once the opcode completed, or discards it when the opcode
/// failed after the Host call (an out-of-gas on the gas charged after the write, say), because the
/// failure takes the write back with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StagedRecord {
    /// An `SSTORE`: the slot's value at the start of the transaction, before the write, and
    /// after it.
    Sstore(SStoreResult),
    /// A `LOG`: its topic count and data length.
    Log {
        /// Number of topics.
        topics: u8,
        /// Length of the data, in bytes.
        data_len: u64,
    },
    /// A `SELFDESTRUCT`.
    SelfDestruct {
        /// Whether the destructed account had a balance to move.
        had_value: bool,
        /// Whether the beneficiary existed before.
        target_exists: bool,
        /// Whether the beneficiary is another account than the destructed one.
        to_other_account: bool,
    },
}

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

impl StagedRecord {
    /// What committing the record counts, per site:
    ///
    /// - `SSTORE`: the first change of a slot in the transaction is one write record; writing the
    ///   slot back to its original value takes the record back.
    /// - `LOG`: its bytes, one base record for the address, one per topic and the data.
    /// - `SELFDESTRUCT`: one write record for the beneficiary, when value moves to another account.
    pub(crate) fn effect(&self) -> RecordEffect {
        match self {
            Self::Sstore(slot) => {
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
            Self::Log { topics, data_len } => RecordEffect::Record(LimitUsage {
                data_size: LOG_BASE_SIZE
                    .saturating_add(LOG_TOPIC_SIZE.saturating_mul(*topics as u64))
                    .saturating_add(*data_len),
                write_records: 0,
            }),
            Self::SelfDestruct { had_value, to_other_account, .. } => {
                if *had_value && *to_other_account {
                    RecordEffect::Record(WRITE_RECORD)
                } else {
                    RecordEffect::None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    fn sstore(original: u64, present: u64, new: u64) -> StagedRecord {
        StagedRecord::Sstore(SStoreResult {
            original_value: U256::from(original),
            present_value: U256::from(present),
            new_value: U256::from(new),
        })
    }

    /// A slot counts once, on its first change in the transaction; writing it back to its
    /// original value takes the record back; every other write counts nothing.
    #[test]
    fn test_sstore_record_rules() {
        assert_eq!(sstore(0, 0, 1).effect(), RecordEffect::Record(WRITE_RECORD));
        assert_eq!(sstore(5, 5, 0).effect(), RecordEffect::Record(WRITE_RECORD));
        assert_eq!(sstore(0, 1, 0).effect(), RecordEffect::Refund(WRITE_RECORD));
        assert_eq!(sstore(0, 1, 2).effect(), RecordEffect::None);
        assert_eq!(sstore(3, 3, 3).effect(), RecordEffect::None);
        assert_eq!(sstore(3, 4, 4).effect(), RecordEffect::None);
    }

    /// A log is 32 bytes for its address, 32 per topic and its data.
    #[test]
    fn test_log_bytes() {
        let log = |topics, data_len| StagedRecord::Log { topics, data_len }.effect();
        assert_eq!(log(0, 0), RecordEffect::Record(LimitUsage { data_size: 32, write_records: 0 }));
        assert_eq!(
            log(4, 100),
            RecordEffect::Record(LimitUsage { data_size: 32 + 4 * 32 + 100, write_records: 0 })
        );
        assert_eq!(
            log(1, u64::MAX),
            RecordEffect::Record(LimitUsage { data_size: u64::MAX, write_records: 0 })
        );
    }

    /// Only value that moves to another account is a beneficiary write.
    #[test]
    fn test_selfdestruct_record_rules() {
        let sd = |had_value, to_other_account| {
            StagedRecord::SelfDestruct { had_value, target_exists: true, to_other_account }.effect()
        };
        assert_eq!(sd(true, true), RecordEffect::Record(WRITE_RECORD));
        assert_eq!(sd(true, false), RecordEffect::None);
        assert_eq!(sd(false, true), RecordEffect::None);
        assert_eq!(sd(false, false), RecordEffect::None);
    }
}
