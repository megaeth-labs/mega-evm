//! Resource limits of the Satin engine.
//!
//! The common execution layer defines what a limit reports when it is crossed: the dimension
//! ([`LimitKind`]), the verdict of a check ([`LimitCheck`]) and the revert data a stopped frame
//! returns ([`MegaLimitExceeded`]). The mechanisms that meter a dimension fill these in: the
//! data-size limit, the KV limit and the state-gas limit below, and detention for compute gas.
//!
//! Gas detention caps compute, [`LimitKind::ComputeGas`]: its state lives beside this module, in
//! [`Detention`](crate::Detention), because what it meters is gas rather than anything the lanes
//! count, and it holds a frame to the cap with the gas the frame may spend. It stops a transaction
//! through the same latch ([`AdditionalLimit::latch`]), with the compute the transaction may reach
//! as the limit, and the limit as what was used ([`LimitCheck::ExceedsLimit`]).
//!
//! It also counts what those limits meter at the sites the data-size limit counts: data-size
//! bytes and write records, on a lane per frame ([`AdditionalLimit`]). The Host stages what it
//! observes ([`StagedRecord`]) and the opcode commits it once it completed.
//!
//! # The data-size limit
//!
//! A transaction is held to [`EvmTxRuntimeLimits::tx_data_size_limit`] and every frame to a
//! budget. The transaction's own frame gets what the transaction has left once its body is
//! counted, and a child gets [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`] of
//! what its parent has left, under [`EvmTxRuntimeLimits::frame_data_size_limit`]. A frame that
//! crosses its budget reverts alone and its caller resumes; a transaction that crosses its limit
//! is stopped through the latch ([`AdditionalLimit`]). A creation stopped at its start still bumps
//! its creator's nonce, and the record of that write lands on the creator: the creator is held to
//! its budget with it before it runs on, and reverts alone if it crossed.
//!
//! What is counted, and when:
//!
//! - the body, before any frame; neither a revert nor an out-of-gas takes it back;
//! - the records of the applied EIP-7702 authorities, before the first frame;
//! - the records a frame's start makes, when the frame starts;
//! - a storage write's record, a log's bytes and the record of a `SELFDESTRUCT`'s beneficiary, once
//!   the opcode completed;
//! - deployed code, on the creation's lane before the creation is committed, so a crossing leaves
//!   no code behind. Only code revm would deposit counts: code starting with `0xEF` or over the
//!   code-size limit, and a creation that cannot pay for its deposit, fail the creation alone and
//!   are not counted;
//! - an Oracle hint's payload, on the transaction's own lane, before it is forwarded.
//!
//! A record is checked against the limits before its history is charged: a record the limit
//! rejects is not kept, so it is not charged, and the stop is what its frame reports. At a frame
//! start the order is the other way round — the caller pays for the records at its opcode, before
//! the frame starts and its records are counted — so a caller that cannot pay runs out of gas
//! first. A body over the limit latches the transaction before it runs, and then nothing is
//! charged for what the stop takes back: no authorization is applied, no record made outside a
//! frame is charged, the first frame's start is charged nothing, and the first frame is answered
//! with the stop. The account a deposit-like transaction creates for its caller is charged all the
//! same, because it exists whatever the transaction does.
//!
//! # The KV limit
//!
//! The KV count is the write-record count the lanes keep: one record per account or storage write
//! the transaction keeps, deduplicated per frame, taken back with a slot written back to its
//! original value and with the frame that fails. The sender's account and the accounts fees are
//! credited to are the body's, not records. [`EvmTxRuntimeLimits::tx_kv_update_limit`] holds it
//! by the data-size limit's rules in its own unit: the transaction's own frame gets what the
//! transaction has left, a child the same share of what its parent has left, under
//! [`EvmTxRuntimeLimits::frame_kv_update_limit`]; a frame over its budget reverts alone and a
//! transaction over its limit is stopped. Every check holds the data size before the records.
//!
//! A record weighs [`WRITE_RECORD_SIZE`] bytes of data size, counted and taken back with them, so
//! the KV count never weighs more than the data size, and a KV limit binds only below the
//! data-size limit's fortieth.
//!
//! # The state-gas limit
//!
//! EIP-8037 charges state gas for exactly the state a transaction adds, so the state a transaction
//! grows is the state gas it spends: [`EvmTxRuntimeLimits::tx_state_gas_limit`] holds it, and
//! nothing counts new accounts and slots beside it. What is held is net — what a frame refilled
//! and what a failed frame rolled back is out of it — and is the state gas charged before the
//! first frame plus what every frame on the call stack holds (the `state_gas` module). The limit is
//! per transaction, with no frame budget: a crossing anywhere stops the transaction, reported as
//! [`LimitKind::StateGrowth`] with the limit in gas.
//!
//! The limit holds each charge where it is made, once it is made, so a charge the frame cannot
//! pay is an out-of-gas whatever the limit: the authorities' before the first frame, which are
//! taken back on a crossing; the first frame's recipient or created account, which the first frame
//! is then answered with the stop for; a fresh slot and a destruction's new beneficiary, which
//! stop the frame; and a new account a `CALL`, `CREATE` or `CREATE2` adds, which its opcode is
//! charged for upfront and the limit holds once revm has decided the frame. A frame revm refuses —
//! a value call its caller cannot fund, one past the call-stack limit — gives that charge back and
//! is never held for it; a frame revm builds, or answers with a success, returns the stop.
//!
//! Deployed code is held just before `return_create` charges it, as its bytes are, so a crossing
//! leaves no code behind — and only once `return_create` is sure to make the charge: a creation
//! that cannot pay the regular costs it charges first, or the state gas itself, runs out of gas
//! there whatever the limit.
//!
//! Wherever the state gas and a record cross together at one site, the state gas is the stop
//! reported. At a frame start the records are held before revm builds the frame, and a frame they
//! stop adds no account, so its upfront state gas is given back rather than held.
//!
//! It is a limit on gas, so it counts state at the SALT price: a slot or an account in a bucket
//! `m` times the minimum costs `m` times the schedule's entry, and reaches the limit that many
//! times sooner.
//!
//! Every limit is unlimited unless a caller sets it. A block executor installs the ones its block
//! limits carry, whose default holds a transaction to
//! [`TX_DATA_LIMIT`](crate::constants::TX_DATA_LIMIT) of data size and to nothing else.
//!
//! # The exemption
//!
//! The protocol's own work is held to none of these per-transaction limits: a system-originated
//! transaction ([`crate::system::is_system_originated`]) and a system call — the pre-block calls
//! among them — run under [`LimitCheck::Exempt`], sticky for the transaction, which the one place
//! every stop comes from answers whatever they cross: the data size, the KV count, the state gas
//! and the frame budgets of the first two. It is the set that pays no history gas, exempt for the
//! same reason: the protocol's maintenance must not fail on a resource limit. What such a
//! transaction uses is counted all the same and reported in its usage and in the block's
//! counters, as a deposit's is. A user's deposit is not in the set and is held to every limit.
//!
//! # The byte table
//!
//! The sizes below are what one of those things weighs, and they are the whole byte table of the
//! engine. The data-size limit meters a transaction against them and history gas prices the same
//! counts, and the pairing is **per record**: a record's history bytes are its own data size, so
//! a log costs history for exactly the bytes the limit counts it at. A mechanism that needs a
//! size takes it from here rather than writing its own number.
//!
//! Per transaction the two totals are not the same number, and are not meant to be. Two sites
//! part them, both by decision:
//!
//! - an Oracle hint's payload is data size the transaction counts and history it does not pay. The
//!   bytes go to the node's oracle service, not into a block, so there is nothing to price;
//! - [`TX_FIXED_WRITE_RECORDS`] is an upper bound on the accounts a transaction's inclusion writes,
//!   and nothing checks afterwards which of them something else wrote again. A transfer whose
//!   recipient is the block beneficiary or a fee vault pays a record the body already bound. It is
//!   an over-charge, never an under-charge.

mod frame_limit;
#[allow(clippy::module_inception)]
mod limit;
mod record;
mod state_gas;

pub use limit::AdditionalLimit;
pub(crate) use record::HistoryBytes;
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

/// Bytes every transaction counts for its envelope: the fields a transaction carries beside its
/// calldata, its access list and its authorizations.
pub const TX_BASE_SIZE: u64 = 110;

/// Write records every transaction makes whatever it runs, counted in its body rather than at the
/// site that makes them: the sender's account, and the four accounts a transaction's fees are
/// credited to — the block beneficiary, the L1 fee vault, the base fee vault and the operator fee
/// vault.
///
/// It is an upper bound. The reward credits fewer accounts when a fee is zero or when the
/// beneficiary is one of the vaults, but the body is priced before the transaction runs, so it
/// carries the bound rather than the count.
pub const TX_FIXED_WRITE_RECORDS: u64 = 5;

/// Bytes every transaction's body counts: its envelope and the records of the writes it makes
/// whatever it runs.
pub const TX_BODY_SIZE: u64 = TX_BASE_SIZE + WRITE_RECORD_SIZE * TX_FIXED_WRITE_RECORDS;

/// Bytes one EIP-7702 authorization counts: the authorization tuple and its signature.
pub const AUTHORIZATION_SIZE: u64 = 101;

/// Bytes one access-list address counts: the address itself.
pub const ACCESS_LIST_ADDRESS_SIZE: u64 = 20;

/// Bytes one access-list storage key counts: the key itself.
pub const ACCESS_LIST_SLOT_SIZE: u64 = 32;

/// Numerator of the share of its parent's remaining data-size budget a child frame receives.
///
/// A child gets [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`] of what its
/// parent has left. The fraction is what keeps a deep call from spending the whole transaction
/// on its innermost frame.
pub const FRAME_DATA_SHARE_NUMERATOR: u64 = 98;

/// Denominator of [`FRAME_DATA_SHARE_NUMERATOR`].
pub const FRAME_DATA_SHARE_DENOMINATOR: u64 = 100;

/// What a transaction or a frame counts: data-size bytes and write records.
///
/// The write-record count is the KV count a node reports, and the KV limit holds it; it has no
/// tracker of its own. Every record weighs [`WRITE_RECORD_SIZE`] bytes of data size and is taken
/// back with them, so `write_records × WRITE_RECORD_SIZE` never exceeds `data_size`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LimitUsage {
    /// Data-size bytes.
    pub data_size: u64,
    /// Account and storage write records: the KV count.
    pub write_records: u64,
}

/// Limits one transaction's data size, write records, state gas and compute after a read of
/// volatile data.
///
/// [`tx_data_size_limit`](Self::tx_data_size_limit) and
/// [`tx_kv_update_limit`](Self::tx_kv_update_limit) stop the transaction. A frame's own budget in
/// each dimension is derived from them: the transaction's frame gets what the transaction has
/// left, and each child gets [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`] of
/// what its parent has left. Crossing a frame budget reverts that frame alone.
///
/// [`frame_data_size_limit`](Self::frame_data_size_limit) and
/// [`frame_kv_update_limit`](Self::frame_kv_update_limit) are a further cap on every frame's
/// budget.
///
/// [`tx_state_gas_limit`](Self::tx_state_gas_limit) holds the transaction's state gas and stops
/// the transaction; it has no frame budget.
///
/// [`block_env_access_compute_gas_limit`](Self::block_env_access_compute_gas_limit) and
/// [`oracle_access_compute_gas_limit`](Self::oracle_access_compute_gas_limit) are gas detention's
/// caps: the compute a transaction may still spend once it read volatile data. They default to
/// the spec's, [`BLOCK_ENV_ACCESS_COMPUTE_GAS`] and [`ORACLE_ACCESS_COMPUTE_GAS`]; every other
/// limit is unlimited unless a caller sets it. [`no_limits`](Self::no_limits) leaves every one
/// unlimited, the caps included, and a transaction whose caps are both unlimited is not detained:
/// limits built from it do not execute the chain.
///
/// [`BLOCK_ENV_ACCESS_COMPUTE_GAS`]: crate::constants::BLOCK_ENV_ACCESS_COMPUTE_GAS
/// [`ORACLE_ACCESS_COMPUTE_GAS`]: crate::constants::ORACLE_ACCESS_COMPUTE_GAS
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EvmTxRuntimeLimits {
    /// The most data-size bytes the transaction may keep. Crossing it stops the transaction.
    pub tx_data_size_limit: u64,
    /// A cap on every frame's data-size budget, applied after the share of what its parent has
    /// left. Crossing it reverts the frame alone.
    pub frame_data_size_limit: u64,
    /// The most write records the transaction may keep: its KV count. Crossing it stops the
    /// transaction.
    pub tx_kv_update_limit: u64,
    /// A cap on every frame's write-record budget, applied after the share of what its parent has
    /// left. Crossing it reverts the frame alone.
    pub frame_kv_update_limit: u64,
    /// The most EIP-8037 state gas the transaction may hold, net of what it refilled and of what
    /// its failed frames rolled back: the limit on the state it grows. Crossing it stops the
    /// transaction.
    ///
    /// It is a limit on gas, so it counts the state at the price the transaction pays for it: a
    /// slot or an account in a crowded SALT bucket costs its bucket's multiple of the schedule's
    /// entry, and reaches the limit that many times sooner.
    pub tx_state_gas_limit: u64,
    /// The compute a transaction may still spend once it read the block environment or the block
    /// beneficiary's account: its compute at the read plus this. Crossing it stops the
    /// transaction.
    ///
    /// `u64::MAX` caps nothing, and with the Oracle's cap unlimited too the transaction is not
    /// detained at all, as under [`no_limits`](Self::no_limits): that is for equivalence runs and
    /// tests, not for executing the chain, which runs on the spec's cap, the default.
    pub block_env_access_compute_gas_limit: u64,
    /// The compute a transaction may still spend once it read the Oracle's storage: its compute
    /// at the read plus this. Crossing it stops the transaction.
    ///
    /// `u64::MAX` caps nothing, and with the block-environment cap unlimited too the transaction
    /// is not detained at all, as under [`no_limits`](Self::no_limits): that is for equivalence
    /// runs and tests, not for executing the chain, which runs on the spec's cap, the default.
    pub oracle_access_compute_gas_limit: u64,
}

impl Default for EvmTxRuntimeLimits {
    /// Gas detention's caps at the spec's, every other limit unlimited.
    fn default() -> Self {
        Self::no_limits()
            .with_block_env_access_compute_gas_limit(crate::constants::BLOCK_ENV_ACCESS_COMPUTE_GAS)
            .with_oracle_access_compute_gas_limit(crate::constants::ORACLE_ACCESS_COMPUTE_GAS)
    }
}

impl EvmTxRuntimeLimits {
    /// No limit at all: gas detention's caps are unlimited too, so no read of volatile data caps
    /// anything, and no transaction is detained.
    ///
    /// It turns detention off together with every other per-transaction limit. It is what the
    /// execution-spec gate's equivalence mode installs, and what tests use to take the limits
    /// out; the chain executes on [`Default`], which holds detention's caps at the spec's.
    pub const fn no_limits() -> Self {
        Self {
            tx_data_size_limit: u64::MAX,
            frame_data_size_limit: u64::MAX,
            tx_kv_update_limit: u64::MAX,
            frame_kv_update_limit: u64::MAX,
            tx_state_gas_limit: u64::MAX,
            block_env_access_compute_gas_limit: u64::MAX,
            oracle_access_compute_gas_limit: u64::MAX,
        }
    }

    /// Sets the transaction's data-size limit.
    pub const fn with_tx_data_size_limit(mut self, limit: u64) -> Self {
        self.tx_data_size_limit = limit;
        self
    }

    /// Caps every frame's data-size budget at `limit`.
    pub const fn with_frame_data_size_limit(mut self, limit: u64) -> Self {
        self.frame_data_size_limit = limit;
        self
    }

    /// Sets the transaction's KV limit: the most write records it may keep.
    pub const fn with_tx_kv_update_limit(mut self, limit: u64) -> Self {
        self.tx_kv_update_limit = limit;
        self
    }

    /// Caps every frame's write-record budget at `limit`.
    pub const fn with_frame_kv_update_limit(mut self, limit: u64) -> Self {
        self.frame_kv_update_limit = limit;
        self
    }

    /// Sets the transaction's state-gas limit: the most state gas it may hold.
    pub const fn with_tx_state_gas_limit(mut self, limit: u64) -> Self {
        self.tx_state_gas_limit = limit;
        self
    }

    /// Sets the compute a transaction may still spend once it read the block environment or the
    /// block beneficiary's account.
    pub const fn with_block_env_access_compute_gas_limit(mut self, limit: u64) -> Self {
        self.block_env_access_compute_gas_limit = limit;
        self
    }

    /// Sets the compute a transaction may still spend once it read the Oracle's storage.
    pub const fn with_oracle_access_compute_gas_limit(mut self, limit: u64) -> Self {
        self.oracle_access_compute_gas_limit = limit;
        self
    }

    /// The transaction's limits on what it keeps, one per dimension of [`LimitUsage`].
    pub(crate) const fn tx_usage_limit(&self) -> LimitUsage {
        LimitUsage { data_size: self.tx_data_size_limit, write_records: self.tx_kv_update_limit }
    }

    /// The caps on every frame's budget, one per dimension of [`LimitUsage`].
    pub(crate) const fn frame_usage_limit(&self) -> LimitUsage {
        LimitUsage {
            data_size: self.frame_data_size_limit,
            write_records: self.frame_kv_update_limit,
        }
    }
}

/// One write record.
pub(crate) const WRITE_RECORD: LimitUsage =
    LimitUsage { data_size: WRITE_RECORD_SIZE, write_records: 1 };

/// A budget with no bound in either dimension.
pub(crate) const UNLIMITED: LimitUsage =
    LimitUsage { data_size: u64::MAX, write_records: u64::MAX };

impl LimitUsage {
    /// Nothing counted.
    pub const ZERO: Self = Self { data_size: 0, write_records: 0 };

    /// The first dimension in which `self` is over `limit` — data size, then write records — with
    /// the limit crossed and the usage that crossed it; `None` when both hold.
    pub(crate) const fn crossing(self, limit: Self) -> Option<(LimitKind, u64, u64)> {
        if self.data_size > limit.data_size {
            return Some((LimitKind::DataSize, limit.data_size, self.data_size));
        }
        if self.write_records > limit.write_records {
            return Some((LimitKind::KVUpdate, limit.write_records, self.write_records));
        }
        None
    }

    /// Each counter the smaller of the two.
    pub(crate) fn min(self, other: Self) -> Self {
        Self {
            data_size: self.data_size.min(other.data_size),
            write_records: self.write_records.min(other.write_records),
        }
    }

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
    /// Key-value updates: the write records a transaction keeps, one per account or storage
    /// write; metered by the KV limit.
    KVUpdate,
    /// Compute gas, the regular gas a transaction spends; capped by detention.
    ComputeGas,
    /// Net new state, metered in EIP-8037 state gas by the state-gas limit: the `limit` of its
    /// stop is the transaction's state-gas limit, in gas. The legacy engine counted the same
    /// dimension in new accounts and slots.
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
        ///
        /// For [`LimitKind::ComputeGas`] it is the limit itself. Gas detention stops a frame on a
        /// regular charge its spendable gas could not pay, and the charge's size is not kept; the
        /// spendable gas the frame had counts as spent, which brings the transaction's compute to
        /// the limit exactly.
        used: u64,
        /// Whether the limit is a frame budget rather than a transaction-level limit.
        frame_local: bool,
    },
    /// The transaction is exempt from every per-transaction limit: it is the protocol's own work.
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

    /// The byte table, written out so a change to one of the sizes is a visible diff.
    #[test]
    fn test_the_byte_table_is_written_out() {
        assert_eq!(WRITE_RECORD_SIZE, 40);
        assert_eq!(LOG_BASE_SIZE, 32);
        assert_eq!(LOG_TOPIC_SIZE, 32);
        assert_eq!(TX_BASE_SIZE, 110);
        assert_eq!(TX_FIXED_WRITE_RECORDS, 5);
        assert_eq!(TX_BODY_SIZE, 310);
        assert_eq!(AUTHORIZATION_SIZE, 101);
        assert_eq!(ACCESS_LIST_ADDRESS_SIZE, 20);
        assert_eq!(ACCESS_LIST_SLOT_SIZE, 32);
        // A storage write and an account write are the same record.
        assert_eq!(WRITE_RECORD, LimitUsage { data_size: WRITE_RECORD_SIZE, write_records: 1 });
    }

    /// `no_limits` leaves every dimension unlimited: it is what the execution-spec gate installs
    /// in equivalence mode, and what a bare context runs under. Every field is named, so a limit
    /// added later cannot be left out of it.
    #[test]
    fn test_no_limits_leaves_every_dimension_unlimited() {
        let EvmTxRuntimeLimits {
            tx_data_size_limit,
            frame_data_size_limit,
            tx_kv_update_limit,
            frame_kv_update_limit,
            tx_state_gas_limit,
            block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit,
        } = EvmTxRuntimeLimits::no_limits();
        for limit in [
            tx_data_size_limit,
            frame_data_size_limit,
            tx_kv_update_limit,
            frame_kv_update_limit,
            tx_state_gas_limit,
            block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit,
        ] {
            assert_eq!(limit, u64::MAX);
        }
    }

    /// The default holds gas detention's caps at the spec's and leaves every other limit
    /// unlimited; a caller's limits change either cap alone.
    #[test]
    fn test_the_default_caps_detention_at_the_specs_and_nothing_else() {
        use crate::constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS};
        let default = EvmTxRuntimeLimits::default();
        assert_eq!(default.block_env_access_compute_gas_limit, BLOCK_ENV_ACCESS_COMPUTE_GAS);
        assert_eq!(default.oracle_access_compute_gas_limit, ORACLE_ACCESS_COMPUTE_GAS);
        assert_eq!(
            default
                .with_block_env_access_compute_gas_limit(u64::MAX)
                .with_oracle_access_compute_gas_limit(u64::MAX),
            EvmTxRuntimeLimits::no_limits()
        );
        let limits = EvmTxRuntimeLimits::no_limits()
            .with_block_env_access_compute_gas_limit(3)
            .with_oracle_access_compute_gas_limit(4);
        assert_eq!(
            (limits.block_env_access_compute_gas_limit, limits.oracle_access_compute_gas_limit),
            (3, 4)
        );
    }

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
        assert!(!exceeded.is_exempt());
        assert!(!LimitCheck::WithinLimit.is_exempt());
        assert!(!LimitCheck::WithinLimit.exceeded_limit());
        assert!(LimitCheck::WithinLimit.revert_data().is_empty());
        let frame_local = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 100,
            used: 150,
            frame_local: true,
        };
        assert!(frame_local.is_frame_local());
        assert!(frame_local.exceeded_limit());
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
