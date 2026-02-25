mod compute_gas;
mod data_size;
mod frame_limit;
mod kv_update;
mod limit;
mod state_growth;

pub use data_size::*;
pub use limit::*;
pub(crate) use frame_limit::{FrameLimitTracker, TxRuntimeLimit};

use crate::MegaHaltReason;

/// Identifies which resource limit was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    /// Data size limit (bytes of data transmitted and stored).
    DataSize,
    /// Key-value update limit (number of state-modifying operations).
    KVUpdate,
    /// Compute gas limit (cumulative EVM instruction gas).
    ComputeGas,
    /// State growth limit (net new accounts and storage slots).
    StateGrowth,
}

/// Result of a limit check, indicating whether any resource limit has been exceeded.
#[derive(Debug, Default, Clone, Copy)]
pub enum LimitCheck {
    /// All limits are within their configured thresholds.
    #[default]
    WithinLimit,
    /// A limit has been exceeded.
    ExceedsLimit {
        /// Which resource limit was exceeded.
        kind: LimitKind,
        /// The configured limit.
        limit: u64,
        /// The current usage.
        used: u64,
        /// Whether this exceed is from a frame-local budget (absorbable at frame boundary)
        /// vs a TX-level limit (must propagate to halt the transaction).
        frame_local: bool,
    },
}

impl LimitCheck {
    /// Returns `true` if any limit has been exceeded.
    #[inline]
    pub const fn exceeded_limit(&self) -> bool {
        !matches!(self, Self::WithinLimit)
    }

    /// Returns `true` if all limits are within their configured thresholds.
    #[inline]
    pub const fn within_limit(&self) -> bool {
        matches!(self, Self::WithinLimit)
    }

    /// Returns whether this is a frame-local exceed.
    #[inline]
    pub const fn is_frame_local(&self) -> bool {
        matches!(self, Self::ExceedsLimit { frame_local: true, .. })
    }

    /// Returns the [`MegaHaltReason`] if the limit has been exceeded, otherwise returns `None`.
    pub fn maybe_halt_reason(&self) -> Option<MegaHaltReason> {
        match self {
            Self::ExceedsLimit { kind: LimitKind::DataSize, limit, used, .. } => {
                Some(MegaHaltReason::DataLimitExceeded { limit: *limit, actual: *used })
            }
            Self::ExceedsLimit { kind: LimitKind::KVUpdate, limit, used, .. } => {
                Some(MegaHaltReason::KVUpdateLimitExceeded { limit: *limit, actual: *used })
            }
            Self::ExceedsLimit { kind: LimitKind::ComputeGas, limit, used, .. } => {
                Some(MegaHaltReason::ComputeGasLimitExceeded { limit: *limit, actual: *used })
            }
            Self::ExceedsLimit { kind: LimitKind::StateGrowth, limit, used, .. } => {
                Some(MegaHaltReason::StateGrowthLimitExceeded { limit: *limit, actual: *used })
            }
            Self::WithinLimit => None,
        }
    }
}
