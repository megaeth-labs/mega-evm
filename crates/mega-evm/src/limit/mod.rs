//! Resource limits of the Satin engine.
//!
//! The common execution layer defines what a limit reports when it is crossed: the dimension
//! ([`LimitKind`]), the verdict of a check ([`LimitCheck`]) and the revert data a stopped frame
//! returns ([`MegaLimitExceeded`]). The mechanisms that meter a dimension fill these in: the
//! data-size limit below, and detention for compute gas.
//!
//! Gas detention caps compute, [`LimitKind::ComputeGas`]: its state lives beside this module, in
//! [`Detention`](crate::Detention), because what it meters is gas rather than bytes, and it holds a
//! frame to the cap with the gas the frame may spend. It stops the frame
//! that crosses the cap with the same revert ([`AdditionalLimit`]), with the compute the
//! transaction may reach as the limit, and its compute at the crossing as what was used
//! ([`LimitCheck::ExceedsLimit`]).
//!
//! What the data-size limit meters — data-size bytes, among them the write records — is drawn from
//! each frame's byte pool on the engine's start ledger, which a child's start takes its share of
//! and which settles when revm settles the start: a frame that fails gives its draws back, and a
//! frame that succeeds hands its caller what it kept. A write an opcode makes is drawn once the
//! opcode completed, in the Host's completed-write hook, which revm calls after every charge the
//! opcode makes; a frame start's records once revm decided the start, on the pool its frame starts
//! with. [`AdditionalLimit`] holds the transaction's limit beside the pools, on what the
//! transaction keeps, with the stops and the settled figures.
//!
//! # The data-size limit
//!
//! A transaction is held to [`EvmTxRuntimeLimits::tx_data_size_limit`] and every frame to a
//! budget. The transaction's own frame gets what the transaction has left once its body is
//! counted, and a child gets [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`] of
//! what its parent has left, under [`EvmTxRuntimeLimits::frame_data_size_limit`]. A frame that
//! crosses either reverts alone, its caller resumes, and what the frame counted goes with it, so
//! the caller is back within both ([`AdditionalLimit`]), but in one case: a failed child's rollback
//! restores what the frames above it kept at its start, a record of theirs it took back included,
//! while an Oracle hint it admitted stays counted, so a hint sent into the room such a take-back
//! freed leaves the transaction over its data-size limit, and the caller returns that limit's stop
//! before it runs another instruction ([`AdditionalLimit::hold_caller`](AdditionalLimit)), whether
//! the child reverted, halted or was itself stopped. The budget is what keeps a caller's own
//! work safe from what it calls: a child inherits 98/100 of its caller's reservoir, so gas does not
//! bound the bytes it can append, and without a budget a callee could take the transaction to its
//! limit and leave its caller nothing to carry on with once it catches the stop. A creation
//! stopped at its start still bumps its creator's nonce, and the record of that write lands on the
//! creator: the creator is held to its limits with it before it runs on, and reverts alone if it
//! crossed one.
//!
//! What is counted, and when:
//!
//! - the body, before any frame; neither a revert nor an out-of-gas takes it back;
//! - the records of the applied EIP-7702 authorities, before the first frame;
//! - the records a frame's start makes, when the frame starts;
//! - a storage write's record, a log's bytes and the records of a `SELFDESTRUCT` — the account it
//!   clears and its beneficiary — once the opcode completed;
//! - deployed code, on the creation's pool before the creation is committed, so a crossing leaves
//!   no code behind. Only code revm would deposit counts: code starting with `0xEF` or over the
//!   code-size limit, and a creation that cannot pay for its deposit, fail the creation alone and
//!   are not counted;
//! - an Oracle hint's payload, as the transaction's own, before it is forwarded;
//! - an EIP-7708 transfer log, which revm journals itself for every value movement, where the value
//!   moves: with the records of the frame start that moves it — the transaction's own value, a
//!   value `CALL`, a creation's endowment — and with a `SELFDESTRUCT`'s beneficiary once the opcode
//!   completed. Each is counted by the rule a `LOG3` of one word is ([`TRANSFER_LOG_SIZE`]), on the
//!   pool of the frame whose journal checkpoint holds the move, so the failure that takes the move
//!   back takes the bytes back.
//!
//! A frame start's records are counted from what revm did, not predicted: the accounts the start
//! writes on its frame are read before revm decides the start and again once it did, and what
//! changed between, with the transfer log revm journaled, is counted and held to the limits on the
//! pool its frame starts with. The hook that charges the caller for the records prices them from
//! the journal's state at the opcode, and the start's credit settles a price that differs from
//! what revm made. A frame revm builds returns the stop on its first run when its start crossed a
//! limit, and its revert takes back the value revm moved and the transfer log it journaled; a
//! success revm answers without building a frame — a call to an account with no code, to a
//! precompile that answers — gives way to the stop, and what its start moved is taken back with
//! it. An answer that failed kept nothing, revm having taken back what its start moved, so it is
//! held to nothing and its result is revm's; a precompile gas detention answers from its price
//! before revm runs it moved nothing and is held to the price alone. A start revm refuses on its
//! caller's account — a value its caller cannot fund, a creation whose creator's nonce cannot be
//! bumped — moves and writes nothing, so it counts nothing and no limit stops it; its opcode,
//! which reads the caller's account first, charged nothing for its records.
//! A start an inspector rewrote after its opcode is not charged again: what the opcode charged
//! stands, and its declared part comes back only as it does for any start whose frame keeps
//! nothing — a frame that fails, or a start revm refuses before it bumps a nonce or moves a value.
//! Its records are those revm then made.
//!
//! A creator's nonce record is counted from what revm did, not predicted: the creator is read
//! before revm decides the creation and again once the creation returned into it, and a nonce revm
//! bumped is counted on the creator then, held to the creator's budget and then to the
//! transaction's limit, a crossing stopping the creator before it runs on. The creation's budget
//! is the share of what the creator had left at its opcode, taken before the record. The record
//! outlives the creation's failure: a creation revm fails after the bump, onto an occupied address,
//! is held to nothing at its start, and its failure discards its records and transfer log, but the
//! creator's record is counted all the same.
//!
//! A record is checked against the limits before its history is charged: a record the limit
//! rejects is not kept, so it is not charged, and the stop is what its frame reports. At a frame
//! start the order is the other way round — the caller pays for the records at its opcode, before
//! revm decides the start and its records are held to the limits — so a caller that cannot pay
//! runs out of gas first, whether or not revm would have refused the start.
//!
//! Nothing counted before the first frame crosses the limit: validation refuses a transaction
//! whose body, with a record for each entry of its authorization list, exceeds it
//! ([`TX_BODY_OVER_DATA_SIZE_LIMIT`]), and those are all a transaction counts before its first
//! frame — the body, and one record for each distinct authority that applies. A deposit refused
//! that way is included as a failed deposit, which keeps its body counted.
//!
//! # Write records
//!
//! The write-record count [`AdditionalLimit`] keeps is the KV count a node reports: one record per
//! account or storage write the transaction keeps, following the net change — a slot's while its
//! value differs from its original, an account's while its nonce, balance or code differs from what
//! the transaction loaded, whichever writes changed it, a creation's created account included —
//! taken back by a write that returns the slot or the account and with the frame that fails. The
//! sender's account and the fee credits the settlement makes are the body's, not records; a write
//! execution makes to a fee account that is not the sender is a record like a write to any other
//! account. The KV-update limit holds the count, per transaction at
//! [`EvmTxRuntimeLimits::tx_kv_update_limit`] and per frame at the same 98/100 share of what the
//! parent has left, on the KV-update pool ([`KV_UPDATE_POOL`]). A record also weighs
//! [`WRITE_RECORD_SIZE`] bytes of data size, counted and taken back with them, so at the spec's
//! numbers the data-size limit binds first; history gas prices each of them.
//!
//! # The order of the counted dimensions
//!
//! An operation that crosses more than one of data size, KV updates and state growth stops on the
//! first of them in that order ([`COUNTED`]). Inside each, the frame's budget is tested before the
//! transaction's limit, and the stop names the budget when the budget is crossed — which a frame
//! that crosses the transaction's limit does too, unless Oracle hints, on no frame's budget, used
//! what the transaction had left.
//!
//! # State growth
//!
//! The state growth [`AdditionalLimit`] keeps is the net count of the new accounts and new slots a
//! transaction keeps, one unit where EIP-8037 charges the state gas of one: a slot an `SSTORE`
//! makes non-zero from an original of zero, which a write back to zero takes back; an account a
//! value transfer, a creation, a `SELFDESTRUCT`'s balance, an applied authorization, a deposit or a
//! keyless signer's nonce bump creates. A failed frame's net goes with it, and a negative net never
//! enlarges a child's budget. The state-growth limit holds the count, per transaction at
//! [`EvmTxRuntimeLimits::tx_state_growth_limit`] and per frame at the same 98/100 share, on the
//! state-growth pool ([`STATE_GROWTH_POOL`]). EIP-8037 still prices the same state, at the SALT
//! price of the bucket it lands in; the count is what stops the 1,001st slot.
//!
//! # The limits
//!
//! They are the per-transaction half of
//! [`ProtocolLimits::for_spec`](crate::ProtocolLimits::for_spec), which for Satin holds a
//! transaction to [`TX_DATA_LIMIT`](crate::constants::TX_DATA_LIMIT) of data size,
//! [`TX_KV_UPDATE_LIMIT`](crate::constants::TX_KV_UPDATE_LIMIT) write records and
//! [`TX_STATE_GROWTH_LIMIT`](crate::constants::TX_STATE_GROWTH_LIMIT) new accounts and slots, every
//! frame to its share of each, and a detained transaction to detention's caps.
//!
//! # The exemption
//!
//! The protocol's own work is held to none of these per-transaction limits: a system-originated
//! transaction ([`crate::system::is_system_originated`]) and a system call — the pre-block calls
//! among them — are exempt ([`AdditionalLimit::is_exempt`]), sticky for the transaction, and the
//! one place every stop comes from answers that they are within the limits whatever they cross:
//! the data size, the KV updates, the state growth and the frame budgets. It is the set that pays
//! no history gas, exempt for the same reason: the protocol's maintenance must not fail on a
//! resource limit. What such a
//! transaction uses is counted all the same and reported in its usage and in the block's counters,
//! as a deposit's is. A user's deposit is not in the set and is held to every limit.
//!
//! # The byte table
//!
//! The sizes below are what one of those things weighs, and they are the whole byte table of the
//! engine. The data-size limit meters a transaction against them and history gas prices the same
//! counts, and the pairing is **per record**: a record's history bytes are its own data size, so
//! a log costs history for exactly the bytes the limit counts it at. A mechanism that needs a
//! size takes it from here rather than writing its own number.
//!
//! Per transaction the two totals are not the same number, and are not meant to be. Three sites
//! part them, each by decision:
//!
//! - an Oracle hint's payload is data size the transaction counts and history it does not pay. The
//!   bytes go to the node's oracle service, not into a block, so there is nothing to price;
//! - an EIP-7708 transfer log is data size and never history. The protocol writes it for every
//!   value movement and Ethereum prices it at nothing: it is not a byte the transaction chose to
//!   append. It is no write record either — the accounts the move writes are recorded as they are
//!   without it;
//! - [`TX_FIXED_WRITE_RECORDS`] is an upper bound on the accounts a transaction's inclusion writes,
//!   and nothing checks afterwards which of them something else wrote again. A transfer whose
//!   recipient is the block beneficiary or a fee vault pays a record the body already bound. It is
//!   an over-charge, never an under-charge.

mod ledger;
#[allow(clippy::module_inception)]
mod limit;
mod pool;
mod record;

pub(crate) use ledger::{reservoir_share, Parked, StartLedger};
#[cfg(any(test, feature = "test-utils"))]
pub use ledger::{LedgerPools, LedgerSettlement};
pub use limit::AdditionalLimit;
pub(crate) use limit::{Counted, TxClass};
pub use pool::{PoolAmounts, PoolId};
#[cfg(test)]
pub(crate) use record::emits_transfer_logs;
#[cfg(any(test, feature = "test-utils"))]
pub(crate) use record::is_transfer_log;
#[cfg(debug_assertions)]
pub(crate) use record::RecordDelta;
pub(crate) use record::{
    start_makes_no_records, Accounts, CreatorBefore, DestroyedParts, HistoryBytes, StartBefore,
    StartRecords,
};

use alloy_primitives::Bytes;
use alloy_sol_types::SolError;
use revm::context::result::ExecutionResult;

/// The pool of a frame's start on the start ledger that is the frame's data-size budget: the bytes
/// the frame may still keep.
pub(crate) const DATA_SIZE_POOL: PoolId = PoolId::new(0);

/// The pool of a frame's start on the start ledger that is the frame's KV-update budget: the write
/// records the frame may still keep.
pub(crate) const KV_UPDATE_POOL: PoolId = PoolId::new(1);

/// The pool of a frame's start on the start ledger that is the frame's state-growth budget: the
/// new accounts and new storage slots the frame may still keep, net.
pub(crate) const STATE_GROWTH_POOL: PoolId = PoolId::new(2);

/// The dimensions the common execution layer counts and limits, each drawn from the pool of a
/// frame's start it names ([`LimitKind::pool`]), in the order an operation that crosses more
/// than one of them stops on: data size, KV updates, state growth.
pub(crate) const COUNTED: [LimitKind; 3] =
    [LimitKind::DataSize, LimitKind::KvUpdate, LimitKind::StateGrowth];

/// Bytes of one write record: the key and value delta one account or storage write leaves in
/// the state diff.
pub const WRITE_RECORD_SIZE: u64 = 40;

/// Bytes every log counts for the address it carries.
pub const LOG_BASE_SIZE: u64 = 32;

/// Bytes every log topic counts.
pub const LOG_TOPIC_SIZE: u64 = 32;

/// The bytes a `LOG` with `topics` topics and `data_len` bytes of data counts, which are the
/// history bytes it appends: its own record for the address that emitted it, one per topic, and
/// the data.
#[inline]
pub const fn log_history_bytes(topics: u64, data_len: u64) -> u64 {
    LOG_BASE_SIZE.saturating_add(LOG_TOPIC_SIZE.saturating_mul(topics)).saturating_add(data_len)
}

/// Bytes an EIP-7708 transfer log counts: its address, its three topics — the event, the sender
/// and the recipient — and one word of data, the amount. It is what a `LOG3` carrying one word
/// counts, by the same rule.
pub const TRANSFER_LOG_SIZE: u64 = LOG_BASE_SIZE + 3 * LOG_TOPIC_SIZE + 32;

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

/// The message of the [`InvalidTransaction::Str`](revm::context::result::InvalidTransaction::Str)
/// a transaction is refused with when its body, with one [`WRITE_RECORD_SIZE`] record for each
/// entry of its authorization list, exceeds the transaction's data-size limit.
///
/// A transaction other than a deposit is rejected with it, by a pool as by block execution; a
/// deposit, which op-revm does not refuse for a transaction error, is included as a failed deposit
/// instead.
pub const TX_BODY_OVER_DATA_SIZE_LIMIT: &str =
    "transaction body and authorization records exceed the transaction data-size limit";

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

/// What a transaction or a frame counts: data-size bytes, write records and state growth, each the
/// dimension's net, never below zero.
///
/// The write-record count is the KV count a node reports, which the KV-update limit holds. Every
/// record weighs [`WRITE_RECORD_SIZE`] bytes of data size and is taken back with them, so
/// `write_records × WRITE_RECORD_SIZE` never exceeds `data_size`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LimitUsage {
    /// Data-size bytes.
    pub data_size: u64,
    /// Account and storage write records: the KV count.
    pub write_records: u64,
    /// New accounts and new storage slots, net: the state growth.
    pub state_growth: u64,
}

/// Limits one transaction's data size, its write records, its state growth and its compute after
/// a read of volatile data.
///
/// [`tx_data_size_limit`](Self::tx_data_size_limit),
/// [`tx_kv_update_limit`](Self::tx_kv_update_limit) and
/// [`tx_state_growth_limit`](Self::tx_state_growth_limit) are what the transaction may keep: a
/// frame whose count crosses one reverts alone. A frame's own budget in each is derived from it:
/// the transaction's frame gets what the transaction has left, and each child gets
/// [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`] of what its parent has left.
/// Crossing a frame budget reverts that frame alone.
///
/// [`frame_data_size_limit`](Self::frame_data_size_limit) is a further cap on every frame's budget.
///
/// [`block_env_access_compute_gas_limit`](Self::block_env_access_compute_gas_limit) and
/// [`oracle_access_compute_gas_limit`](Self::oracle_access_compute_gas_limit) are gas detention's
/// caps: the compute a transaction may still spend once it read volatile data.
/// [`no_limits`](Self::no_limits) leaves every limit unlimited, the caps included, and a
/// transaction whose caps are both unlimited is not detained: limits built from it do not execute
/// the chain.
///
/// The chain's values are the per-transaction half of
/// [`ProtocolLimits::for_spec`](crate::ProtocolLimits::for_spec), and [`Default`] is the default
/// spec's; other values are a tool's or a test's counterfactual.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvmTxRuntimeLimits {
    /// The most data-size bytes the transaction may keep. Crossing it reverts the frame that
    /// crosses it.
    pub tx_data_size_limit: u64,
    /// The most write records the transaction may keep: its KV count. Crossing it reverts the
    /// frame that crosses it, and every frame's KV budget is its share of what its parent has
    /// left. A limits file without it takes the default spec's.
    #[serde(default = "default_tx_kv_update_limit")]
    pub tx_kv_update_limit: u64,
    /// The most new accounts and new storage slots the transaction may keep, net: its state
    /// growth. Crossing it reverts the frame that crosses it, and every frame's state-growth
    /// budget is its share of what its parent has left. A limits file without it takes the
    /// default spec's.
    #[serde(default = "default_tx_state_growth_limit")]
    pub tx_state_growth_limit: u64,
    /// A cap on every frame's data-size budget, applied after the share of what its parent has
    /// left. Crossing it reverts the frame alone.
    pub frame_data_size_limit: u64,
    /// The compute a transaction may still spend once it read the block environment or the block
    /// beneficiary's account: its compute at the read plus this. Crossing it reverts the frame
    /// that crosses it.
    ///
    /// `u64::MAX` caps nothing, and with the Oracle's cap unlimited too the transaction is not
    /// detained at all, as under [`no_limits`](Self::no_limits): that is for equivalence runs and
    /// tests, not for executing the chain, which runs on the spec's cap, the default.
    pub block_env_access_compute_gas_limit: u64,
    /// The compute a transaction may still spend once it read the Oracle's storage: its compute
    /// at the read plus this. Crossing it reverts the frame that crosses it.
    ///
    /// `u64::MAX` caps nothing, and with the block-environment cap unlimited too the transaction
    /// is not detained at all, as under [`no_limits`](Self::no_limits): that is for equivalence
    /// runs and tests, not for executing the chain, which runs on the spec's cap, the default.
    pub oracle_access_compute_gas_limit: u64,
}

/// The default spec's transaction KV-update limit, for a limits file that does not name one.
fn default_tx_kv_update_limit() -> u64 {
    EvmTxRuntimeLimits::default().tx_kv_update_limit
}

/// The default spec's transaction state-growth limit, for a limits file that does not name one.
fn default_tx_state_growth_limit() -> u64 {
    EvmTxRuntimeLimits::default().tx_state_growth_limit
}

impl Default for EvmTxRuntimeLimits {
    /// The limits of the default spec's table: what a [`MegaContext`](crate::MegaContext) of that
    /// spec runs under.
    fn default() -> Self {
        crate::ProtocolLimits::for_spec(crate::MegaSpecId::default()).tx_runtime_limits
    }
}

impl EvmTxRuntimeLimits {
    /// No limit at all: gas detention's caps are unlimited too, so no read of volatile data caps
    /// anything, and no transaction is detained.
    ///
    /// It turns detention off together with every other per-transaction limit. It is what the
    /// execution-spec gate's equivalence mode installs, and what tests use to take the limits
    /// out; the chain executes on its spec's ([`ProtocolLimits::for_spec`]), whose detention caps
    /// are below [`MAX_TX_COMPUTE_GAS`](crate::constants::MAX_TX_COMPUTE_GAS), the most compute a
    /// transaction can spend.
    ///
    /// [`ProtocolLimits::for_spec`]: crate::ProtocolLimits::for_spec
    pub const fn no_limits() -> Self {
        Self {
            tx_data_size_limit: u64::MAX,
            tx_kv_update_limit: u64::MAX,
            tx_state_growth_limit: u64::MAX,
            frame_data_size_limit: u64::MAX,
            block_env_access_compute_gas_limit: u64::MAX,
            oracle_access_compute_gas_limit: u64::MAX,
        }
    }

    /// Sets the transaction's data-size limit.
    pub const fn with_tx_data_size_limit(mut self, limit: u64) -> Self {
        self.tx_data_size_limit = limit;
        self
    }

    /// Sets the most write records the transaction may keep.
    pub const fn with_tx_kv_update_limit(mut self, limit: u64) -> Self {
        self.tx_kv_update_limit = limit;
        self
    }

    /// Sets the most new accounts and new storage slots the transaction may keep, net.
    pub const fn with_tx_state_growth_limit(mut self, limit: u64) -> Self {
        self.tx_state_growth_limit = limit;
        self
    }

    /// Caps every frame's data-size budget at `limit`.
    pub const fn with_frame_data_size_limit(mut self, limit: u64) -> Self {
        self.frame_data_size_limit = limit;
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
}

impl LimitUsage {
    /// Nothing counted.
    pub const ZERO: Self = Self { data_size: 0, write_records: 0, state_growth: 0 };

    /// Whether `self` keeps more data-size bytes than `limit`: what validation holds a body to.
    pub(crate) const fn exceeds_data_size(self, limit: u64) -> bool {
        self.data_size > limit
    }

    /// Every counter added, saturating.
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            data_size: self.data_size.saturating_add(other.data_size),
            write_records: self.write_records.saturating_add(other.write_records),
            state_growth: self.state_growth.saturating_add(other.state_growth),
        }
    }

    /// Every counter subtracted, saturating at zero.
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self {
            data_size: self.data_size.saturating_sub(other.data_size),
            write_records: self.write_records.saturating_sub(other.write_records),
            state_growth: self.state_growth.saturating_sub(other.state_growth),
        }
    }

    /// Every counter multiplied by `n`, saturating.
    pub const fn times(self, n: u64) -> Self {
        Self {
            data_size: self.data_size.saturating_mul(n),
            write_records: self.write_records.saturating_mul(n),
            state_growth: self.state_growth.saturating_mul(n),
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
    /// The write records a transaction keeps, its KV count; metered by the KV-update limit.
    KvUpdate,
    /// Compute gas, the regular gas a transaction spends; capped by detention.
    ComputeGas,
    /// The new accounts and new storage slots a transaction keeps, net; metered by the
    /// state-growth limit.
    StateGrowth,
}

impl LimitKind {
    /// The `kind` of [`MegaLimitExceeded`].
    pub const fn as_u8(&self) -> u8 {
        match self {
            Self::DataSize => 0,
            Self::KvUpdate => 1,
            Self::ComputeGas => 2,
            Self::StateGrowth => 3,
        }
    }

    /// The dimension a `kind` of [`MegaLimitExceeded`] names, if any.
    pub const fn from_u8(kind: u8) -> Option<Self> {
        match kind {
            0 => Some(Self::DataSize),
            1 => Some(Self::KvUpdate),
            2 => Some(Self::ComputeGas),
            3 => Some(Self::StateGrowth),
            _ => None,
        }
    }

    /// The pool of a frame's start the dimension is drawn from; `None` for compute, which gas
    /// detention holds through the frame's gas instead.
    pub(crate) const fn pool(&self) -> Option<PoolId> {
        match self {
            Self::DataSize => Some(DATA_SIZE_POOL),
            Self::KvUpdate => Some(KV_UPDATE_POOL),
            Self::StateGrowth => Some(STATE_GROWTH_POOL),
            Self::ComputeGas => None,
        }
    }
}

/// The verdict of a limit check.
///
/// An exceed reverts the frame that crossed the limit, alone, and its caller resumes: a
/// transaction-level exceed and a frame-local one (a frame budget) alike, which the revert data
/// tell apart by the limit they name. A transaction exempt from the limits
/// ([`AdditionalLimit::is_exempt`]) is within them whatever it crosses.
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
        /// For [`LimitKind::ComputeGas`] it is the transaction's compute before the charge that
        /// would have crossed the limit, which the regular ledger bills less the stipends of the
        /// transaction's value calls, gas nobody paid: gas detention stops a frame on a regular
        /// charge its spendable gas could not pay, does not make it, and puts the frame's gas
        /// back to what it had before it. The charge's size is not kept, so this is at most the
        /// limit.
        used: u64,
        /// Whether the limit is a frame budget rather than a transaction-level limit.
        frame_local: bool,
    },
}

impl LimitCheck {
    /// Whether a limit was crossed.
    #[inline]
    pub const fn exceeded_limit(&self) -> bool {
        matches!(self, Self::ExceedsLimit { .. })
    }

    /// Whether the check passed.
    #[inline]
    pub const fn within_limit(&self) -> bool {
        matches!(self, Self::WithinLimit)
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
            Self::WithinLimit => Bytes::new(),
        }
    }
}

/// A limit that stopped a transaction: the dimension, the limit and the usage that crossed it, of
/// the crossing that produced the result of the transaction's own frame — a transaction-level
/// limit, or that frame's own budget in data size, KV updates or state growth. It is what an RPC
/// maps a limit stop to, in place of the halt reasons the legacy engine reported one with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitStop {
    /// The dimension crossed.
    pub kind: LimitKind,
    /// The limit crossed, in the dimension's unit: bytes or gas.
    pub limit: u64,
    /// The usage that crossed it (see [`LimitCheck::ExceedsLimit`]).
    pub used: u64,
}

impl LimitStop {
    /// The stop a transaction ended with, read off its `result` and the `limit_exceeded` it
    /// reports: [`MegaTransactionOutcome::limit_exceeded`](crate::MegaTransactionOutcome), or,
    /// for a transaction run through another entry point,
    /// [`AdditionalLimit::limit_exceeded`] on the EVM's context, read after the transaction ran
    /// and before the next transaction or system call starts on the EVM, which resets it.
    ///
    /// `Some` when both say so: `limit_exceeded` names a crossing, and `result` is a revert whose
    /// output is that limit's [`MegaLimitExceeded`]. The output alone never makes a stop. A
    /// contract can revert with the same bytes, and a frame a limit stopped below the
    /// transaction's own reverts with them while its caller runs on and may re-raise them:
    /// `limit_exceeded` is then `None`, and so is this.
    pub fn from_result<HaltReason>(
        result: &ExecutionResult<HaltReason>,
        limit_exceeded: Option<&LimitCheck>,
    ) -> Option<Self> {
        let stop = limit_exceeded?;
        let &LimitCheck::ExceedsLimit { kind, limit, used, .. } = stop else { return None };
        let ExecutionResult::Revert { output, .. } = result else { return None };
        (*output == stop.revert_data()).then_some(Self { kind, limit, used })
    }
}

/// The dimension and the limit the revert data of a limit stop names: `MegaLimitExceeded(kind,
/// limit)`, exactly as the engine encodes it, with a `kind` a [`LimitKind`] has. `None` for any
/// other bytes, an out-of-range word and trailing data included.
///
/// Bytes that decode are not a stop by themselves: see [`LimitStop::from_result`].
pub fn decode_mega_limit_exceeded(revert_data: &[u8]) -> Option<(LimitKind, u64)> {
    let decoded = MegaLimitExceeded::abi_decode(revert_data).ok()?;
    if decoded.abi_encode() != revert_data {
        return None;
    }
    Some((LimitKind::from_u8(decoded.kind)?, decoded.limit))
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
        assert_eq!(TRANSFER_LOG_SIZE, 160);
        assert_eq!(TX_BASE_SIZE, 110);
        assert_eq!(TX_FIXED_WRITE_RECORDS, 5);
        assert_eq!(TX_BODY_SIZE, 310);
        assert_eq!(AUTHORIZATION_SIZE, 101);
        assert_eq!(ACCESS_LIST_ADDRESS_SIZE, 20);
        assert_eq!(ACCESS_LIST_SLOT_SIZE, 32);
    }

    /// `no_limits` leaves every dimension unlimited: it is what the execution-spec gate installs
    /// in equivalence mode. Every field is named, so a limit added later cannot be left out of it.
    #[test]
    fn test_no_limits_leaves_every_dimension_unlimited() {
        let EvmTxRuntimeLimits {
            tx_data_size_limit,
            tx_kv_update_limit,
            tx_state_growth_limit,
            frame_data_size_limit,
            block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit,
        } = EvmTxRuntimeLimits::no_limits();
        for limit in [
            tx_data_size_limit,
            tx_kv_update_limit,
            tx_state_growth_limit,
            frame_data_size_limit,
            block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit,
        ] {
            assert_eq!(limit, u64::MAX);
        }
    }

    /// The default is the default spec's table, Satin's: the data-size limit and detention's two
    /// caps; a caller's limits change a cap alone.
    #[test]
    fn test_the_default_is_the_specs_table() {
        let default = EvmTxRuntimeLimits::default();
        assert_eq!(
            default,
            crate::ProtocolLimits::for_spec(crate::MegaSpecId::SATIN).tx_runtime_limits
        );
        assert_eq!(
            default
                .with_tx_data_size_limit(u64::MAX)
                .with_tx_kv_update_limit(u64::MAX)
                .with_tx_state_growth_limit(u64::MAX)
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
        let max =
            LimitUsage { data_size: u64::MAX, write_records: u64::MAX, state_growth: u64::MAX };
        let record = LimitUsage { data_size: WRITE_RECORD_SIZE, write_records: 1, state_growth: 1 };
        assert_eq!(max.saturating_add(record), max);
        assert_eq!(LimitUsage::ZERO.saturating_sub(record), LimitUsage::ZERO);
        assert_eq!(
            record.times(3),
            LimitUsage { data_size: 120, write_records: 3, state_growth: 3 }
        );
        assert_eq!(record.times(u64::MAX).data_size, u64::MAX);
        assert_eq!(record.saturating_add(record).saturating_sub(record), record);
    }

    /// `within_limit` follows the variant, and is the exact complement of `exceeded_limit`.
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
        assert!(!LimitCheck::WithinLimit.exceeded_limit());
        assert!(!LimitCheck::WithinLimit.is_frame_local());
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

    /// The kinds keep the legacy engine's wire encoding: data size `0`, KV updates `1`, compute
    /// `2`, state growth `3`; every discriminant survives the round trip, and an unknown kind names
    /// no dimension.
    #[test]
    fn test_limit_kind_keeps_its_wire_encoding() {
        for (kind, expected) in [
            (LimitKind::DataSize, 0),
            (LimitKind::KvUpdate, 1),
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
        for unknown in [4, 5, u8::MAX] {
            assert_eq!(LimitKind::from_u8(unknown), None, "{unknown}");
        }
    }

    /// Every kind's revert data decodes to its kind and limit, and nothing else decodes: another
    /// selector, a kind no dimension has, a `uint8` word with high bits set, trailing or missing
    /// bytes.
    #[test]
    fn test_decode_mega_limit_exceeded_accepts_exactly_the_stops_encoding() {
        let kinds = [
            LimitKind::DataSize,
            LimitKind::KvUpdate,
            LimitKind::ComputeGas,
            LimitKind::StateGrowth,
        ];
        for kind in kinds {
            let stop = LimitCheck::ExceedsLimit { kind, limit: 7, used: 8, frame_local: false };
            assert_eq!(decode_mega_limit_exceeded(&stop.revert_data()), Some((kind, 7)));
        }

        let encoded = |kind: u8| MegaLimitExceeded { kind, limit: 7 }.abi_encode();
        assert_eq!(decode_mega_limit_exceeded(&encoded(4)), None, "a kind no dimension has");
        assert_eq!(decode_mega_limit_exceeded(&encoded(5)), None, "a kind no dimension has");
        assert_eq!(decode_mega_limit_exceeded(&encoded(u8::MAX)), None);
        let mut wide = encoded(0);
        wide[4 + 30] = 1;
        assert_eq!(decode_mega_limit_exceeded(&wide), None, "a uint8 word with high bits set");
        let mut trailing = encoded(0);
        trailing.push(0);
        assert_eq!(decode_mega_limit_exceeded(&trailing), None, "trailing bytes");
        let short = encoded(0);
        assert_eq!(decode_mega_limit_exceeded(&short[..short.len() - 1]), None, "missing bytes");
        let mut selector = encoded(0);
        selector[0] ^= 1;
        assert_eq!(decode_mega_limit_exceeded(&selector), None, "another selector");
        assert_eq!(decode_mega_limit_exceeded(&[]), None);
    }

    /// A stop is read off the result and `limit_exceeded` together: the revert carrying the
    /// limit's data and a check naming it, a frame budget's as well as a transaction-level
    /// limit's. Either alone is no stop.
    #[test]
    fn test_limit_stop_needs_the_revert_and_the_check_to_agree() {
        use crate::MegaHaltReason;
        use revm::context::result::{HaltReason, ResultGas, SuccessReason};

        let check = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 3,
            used: 4,
            frame_local: false,
        };
        let revert = |output: Bytes| ExecutionResult::<MegaHaltReason>::Revert {
            gas: ResultGas::default(),
            logs: Default::default(),
            output,
        };
        let stop = LimitStop { kind: LimitKind::DataSize, limit: 3, used: 4 };

        assert_eq!(LimitStop::from_result(&revert(check.revert_data()), Some(&check)), Some(stop));
        assert_eq!(
            LimitStop::from_result(&revert(check.revert_data()), None),
            None,
            "the same bytes without the check: a contract's own revert"
        );
        let frame_local = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 3,
            used: 4,
            frame_local: true,
        };
        assert_eq!(
            LimitStop::from_result(&revert(check.revert_data()), Some(&frame_local)),
            Some(stop),
            "the transaction's own frame stopped by its budget"
        );
        let other_limit = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 9,
            used: 10,
            frame_local: false,
        };
        assert_eq!(
            LimitStop::from_result(&revert(check.revert_data()), Some(&other_limit)),
            None,
            "revert data naming another limit"
        );
        assert_eq!(LimitStop::from_result(&revert(Bytes::new()), Some(&check)), None);
        assert_eq!(
            LimitStop::from_result(&revert(check.revert_data()), Some(&LimitCheck::WithinLimit)),
            None
        );
        let success = ExecutionResult::<MegaHaltReason>::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default(),
            logs: Default::default(),
            output: revm::context::result::Output::Call(check.revert_data()),
        };
        assert_eq!(LimitStop::from_result(&success, Some(&check)), None, "a stop is a revert");
        let halt = ExecutionResult::<MegaHaltReason>::Halt {
            reason: HaltReason::OutOfGas(revm::context::result::OutOfGasError::Basic).into(),
            gas: ResultGas::default(),
            logs: Default::default(),
        };
        assert_eq!(LimitStop::from_result(&halt, Some(&check)), None);
    }

    /// The revert data is the ABI encoding of `MegaLimitExceeded(uint8,uint64)`.
    #[test]
    fn test_revert_data_encodes_mega_limit_exceeded() {
        let check = LimitCheck::ExceedsLimit {
            kind: LimitKind::ComputeGas,
            limit: 7,
            used: 9,
            frame_local: true,
        };
        let data = check.revert_data();
        assert_eq!(&data[..4], MegaLimitExceeded::SELECTOR.as_slice());
        let decoded = MegaLimitExceeded::abi_decode(&data).unwrap();
        assert_eq!(decoded, MegaLimitExceeded { kind: 2, limit: 7 });
        assert_eq!(MegaLimitExceeded::SIGNATURE, "MegaLimitExceeded(uint8,uint64)");
    }
}
