//! Resource limits of the Satin engine.
//!
//! The common execution layer defines what a limit reports when it is crossed: the dimension
//! ([`LimitKind`]), the verdict of a check ([`LimitCheck`]) and the revert data a stopped frame
//! returns ([`MegaLimitExceeded`]). The mechanisms that meter a dimension (the data-size limit,
//! detention, the state-growth and KV limits) fill these in.
//!
//! It also counts what those limits meter at the sites the data-size limit counts: data-size
//! bytes and write records, on a lane per frame ([`AdditionalLimit`]). The Host stages what it
//! observes ([`StagedRecord`]) and the opcode commits it once it completed.

mod frame_limit;
#[allow(clippy::module_inception)]
mod limit;
mod record;

pub use limit::AdditionalLimit;
pub use record::StagedRecord;

use alloy_primitives::Bytes;
use alloy_sol_types::SolError;

/// Bytes of one write record: the key and value delta one account or storage write leaves in
/// the state diff.
pub const WRITE_RECORD_SIZE: u64 = 40;

/// Bytes every log counts for the address it carries.
pub const LOG_BASE_SIZE: u64 = 32;

/// Bytes every log topic counts.
pub const LOG_TOPIC_SIZE: u64 = 32;

/// What a transaction or a frame counts: data-size bytes and write records.
///
/// The KV count a node reports is the write-record count; it has no tracker of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LimitUsage {
    /// Data-size bytes.
    pub data_size: u64,
    /// Account and storage write records.
    pub write_records: u64,
}

/// One write record.
pub(crate) const WRITE_RECORD: LimitUsage =
    LimitUsage { data_size: WRITE_RECORD_SIZE, write_records: 1 };

impl LimitUsage {
    /// Nothing counted.
    pub const ZERO: Self = Self { data_size: 0, write_records: 0 };

    /// Both counters added, saturating.
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            data_size: self.data_size.saturating_add(other.data_size),
            write_records: self.write_records.saturating_add(other.write_records),
        }
    }

    /// Both counters subtracted, saturating at zero.
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self {
            data_size: self.data_size.saturating_sub(other.data_size),
            write_records: self.write_records.saturating_sub(other.write_records),
        }
    }

    /// Both counters multiplied by `n`, saturating.
    pub const fn times(self, n: u64) -> Self {
        Self {
            data_size: self.data_size.saturating_mul(n),
            write_records: self.write_records.saturating_mul(n),
        }
    }
}

alloy_sol_types::sol! {
    /// The revert data of a frame a resource limit stopped.
    ///
    /// `kind` is the [`LimitKind`] discriminant and `limit` the limit that was crossed. A frame
    /// can revert with the same bytes on its own; whether a limit stopped the transaction is
    /// reported by the transaction outcome, not read off the output.
    #[derive(Debug, PartialEq, Eq)]
    error MegaLimitExceeded(uint8 kind, uint64 limit);
}

/// A resource dimension a limit meters.
///
/// The discriminants are the `kind` of [`MegaLimitExceeded`] and keep the values the legacy engine
/// encoded, so a contract that decodes the legacy revert data decodes Satin's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LimitKind {
    /// Bytes of data a transaction produces; metered by the data-size limit.
    DataSize,
    /// Key-value updates; metered by the state-growth and KV limits.
    KVUpdate,
    /// Compute gas, the regular gas a transaction spends; capped by detention.
    ComputeGas,
    /// Net new state; metered by the state-growth and KV limits.
    StateGrowth,
}

impl LimitKind {
    /// The `kind` of [`MegaLimitExceeded`].
    pub const fn as_u8(&self) -> u8 {
        match self {
            Self::DataSize => 0,
            Self::KVUpdate => 1,
            Self::ComputeGas => 2,
            Self::StateGrowth => 3,
        }
    }

    /// The dimension a `kind` of [`MegaLimitExceeded`] names, if any.
    pub const fn from_u8(kind: u8) -> Option<Self> {
        match kind {
            0 => Some(Self::DataSize),
            1 => Some(Self::KVUpdate),
            2 => Some(Self::ComputeGas),
            3 => Some(Self::StateGrowth),
            _ => None,
        }
    }
}

/// The verdict of a limit check.
///
/// A transaction-level exceed stops the transaction: the frame that crosses it reverts, the
/// transaction is latched and every frame above reverts in turn. A frame-local exceed (a frame
/// budget) reverts the frame alone and its caller resumes. `Exempt` is sticky for the
/// transaction: nothing it does is stopped by a limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LimitCheck {
    /// Every limit holds.
    #[default]
    WithinLimit,
    /// A limit was crossed.
    ExceedsLimit {
        /// The dimension crossed.
        kind: LimitKind,
        /// The limit crossed.
        limit: u64,
        /// The usage that crossed it.
        used: u64,
        /// Whether the limit is a frame budget rather than a transaction-level limit.
        frame_local: bool,
    },
    /// The transaction is exempt from metering.
    Exempt,
}

impl LimitCheck {
    /// Whether a limit was crossed. `Exempt` is not.
    #[inline]
    pub const fn exceeded_limit(&self) -> bool {
        matches!(self, Self::ExceedsLimit { .. })
    }

    /// Whether the check passed. `Exempt` is a state of its own, not a pass.
    #[inline]
    pub const fn within_limit(&self) -> bool {
        matches!(self, Self::WithinLimit)
    }

    /// Whether the transaction is exempt from metering.
    #[inline]
    pub const fn is_exempt(&self) -> bool {
        matches!(self, Self::Exempt)
    }

    /// Whether a frame budget, rather than a transaction-level limit, was crossed.
    #[inline]
    pub const fn is_frame_local(&self) -> bool {
        matches!(self, Self::ExceedsLimit { frame_local: true, .. })
    }

    /// The [`MegaLimitExceeded`] revert data of a crossed limit; empty otherwise.
    pub fn revert_data(&self) -> Bytes {
        match self {
            Self::ExceedsLimit { kind, limit, .. } => {
                MegaLimitExceeded { kind: kind.as_u8(), limit: *limit }.abi_encode().into()
            }
            Self::WithinLimit | Self::Exempt => Bytes::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_limit_usage_arithmetic_saturates() {
        let max = LimitUsage { data_size: u64::MAX, write_records: u64::MAX };
        assert_eq!(max.saturating_add(WRITE_RECORD), max);
        assert_eq!(LimitUsage::ZERO.saturating_sub(WRITE_RECORD), LimitUsage::ZERO);
        assert_eq!(WRITE_RECORD.times(3), LimitUsage { data_size: 120, write_records: 3 });
        assert_eq!(WRITE_RECORD.times(u64::MAX).data_size, u64::MAX);
        assert_eq!(
            WRITE_RECORD.saturating_add(WRITE_RECORD).saturating_sub(WRITE_RECORD),
            WRITE_RECORD
        );
    }

    /// `Exempt` passes no predicate that would stop a frame, and has no revert data.
    #[test]
    fn test_limit_check_exempt_predicate_truth_table() {
        let exempt = LimitCheck::Exempt;
        assert!(!exempt.exceeded_limit());
        assert!(!exempt.within_limit());
        assert!(exempt.is_exempt());
        assert!(!exempt.is_frame_local());
        assert!(exempt.revert_data().is_empty());
    }

    /// `within_limit` follows the variant.
    #[test]
    fn test_within_limit_reflects_variant() {
        assert!(LimitCheck::WithinLimit.within_limit());
        let exceeded = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 100,
            used: 150,
            frame_local: false,
        };
        assert!(!exceeded.within_limit());
        assert!(exceeded.exceeded_limit());
        assert!(!exceeded.is_frame_local());
    }

    /// Every discriminant survives the round trip, and an unknown one maps to nothing.
    #[test]
    fn test_limit_kind_u8_roundtrip() {
        for (kind, expected) in [
            (LimitKind::DataSize, 0),
            (LimitKind::KVUpdate, 1),
            (LimitKind::ComputeGas, 2),
            (LimitKind::StateGrowth, 3),
        ] {
            assert_eq!(kind.as_u8(), expected, "{kind:?}");
            assert_eq!(
                LimitKind::from_u8(kind.as_u8()),
                Some(kind),
                "round-trip failed for {kind:?}"
            );
        }
        assert_eq!(LimitKind::from_u8(4), None);
    }

    /// The revert data is the ABI encoding of `MegaLimitExceeded(uint8,uint64)`.
    #[test]
    fn test_revert_data_encodes_mega_limit_exceeded() {
        let check = LimitCheck::ExceedsLimit {
            kind: LimitKind::StateGrowth,
            limit: 7,
            used: 9,
            frame_local: true,
        };
        let data = check.revert_data();
        assert_eq!(&data[..4], MegaLimitExceeded::SELECTOR.as_slice());
        let decoded = MegaLimitExceeded::abi_decode(&data).unwrap();
        assert_eq!(decoded, MegaLimitExceeded { kind: 3, limit: 7 });
        assert_eq!(MegaLimitExceeded::SIGNATURE, "MegaLimitExceeded(uint8,uint64)");
    }
}
