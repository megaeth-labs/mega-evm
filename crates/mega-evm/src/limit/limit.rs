//! The per-transaction state of the common execution layer.

use alloy_primitives::Bytes;
use revm::{
    context_interface::cfg::gas::{GasTracker, PoolAmounts, PoolId},
    handler::FrameResult,
    interpreter::InstructionResult,
};

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

#[cfg(debug_assertions)]
use super::StartLedger;
use super::{
    CreatorBefore, EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, COUNTED, DATA_SIZE_POOL,
    KV_UPDATE_POOL, STATE_GROWTH_POOL, TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE,
};

/// What the common execution layer tracks for the running transaction: what it keeps of the
/// data-size, KV-update and state-growth limits and of the frame budgets, the stops its frames
/// return, whether it is exempt from every limit, and the figures the settled transaction reports.
///
/// It lives on the [`MegaContext`](crate::MegaContext), is begun before each transaction and
/// system call, and is driven by the Host's hooks (a completed write, a frame's start, a code
/// deposit) and by the frame lifecycle (a start revm decided, a frame's return, the stops).
///
/// # The three counted dimensions
///
/// Data size, KV updates (write records) and state growth are each limited per transaction and
/// per frame ([`COUNTED`]). A frame's budget in a dimension is a pool on its gas tracker
/// ([`LimitKind::pool`]), which the revm fork forwards and settles as it does the reservoir: the
/// transaction's own frame gets what the transaction has left once its body and the counts made
/// before it are counted ([`first_frame_pools`](Self::first_frame_pools)), every child the
/// schedule's share of what its caller has left (98/100: the caller keeps
/// `GasId::pool_kept_percent`) under the frame cap, a failing frame's draws go back to its pool and
/// a successful one hands its signed net to its caller. A count is drawn where it is made, on the
/// pool of the frame that makes it ([`draw`](Self::draw)): a completed write's record or log, a
/// frame start's records and transfer log on the started frame, deployed code on the creating
/// frame. A creator's nonce record is the creating frame's own, drawn on its pools once the
/// creation returned into it with the nonce bumped ([`close_start`](Self::close_start) hands back
/// what tells): the creation's budgets were taken before it.
///
/// The transaction's limits are held beside the pools, on what the transaction keeps
/// ([`usage`](Self::usage)): its body, its Oracle hints, the records of its applied authorities,
/// and what its frames keep ([`Kept`]), counted with every draw and restored to what it was at a
/// start when that start's frame kept nothing ([`close_start`](Self::close_start)). The transfer
/// logs the transaction reports apart from its bytes are kept beside them, and restored with them.
///
/// An operation that crosses more than one dimension stops on the first of them in [`COUNTED`]'s
/// order, and inside a dimension the frame's budget is tested before the transaction's limit: the
/// stop names the budget when the budget is crossed, the transaction's limit only when it is not.
/// A fall — a record taken back, a byte given back — crosses nothing.
///
/// # The stop
///
/// A limit crossed stops the frame that crossed it with a revert, not a halt: the frame reverts
/// with [`MegaLimitExceeded`](crate::MegaLimitExceeded) as its output, whether it crossed the
/// transaction's limit or its own frame budget, and its caller resumes as after any revert. What
/// the frame counted goes with it, so a caller that resumes is back within the limits, but where
/// the failure put back a record a frame above made while an Oracle hint the failed frame sent
/// stays: the caller is then stopped as it resumes ([`hold_caller`](Self::hold_caller)). A
/// transaction never keeps more than its limit: a caller that carries on is held to what is left,
/// and a charge that crosses again stops the frame that makes it. A crossing the transaction's own
/// frame makes is the transaction's result, which settles like any EIP-8037 revert: its unspent
/// regular gas and the reservoir go back to the sender, who pays only for what ran. Exceptional
/// halts (a real out-of-gas, an invalid opcode) are not limit stops and still burn the frame's gas.
///
/// # The exemption
///
/// The protocol's own work is held to none of the per-transaction limits: a system-originated
/// transaction and a system call are begun exempt, before their body is counted, and the mark is
/// sticky for the transaction ([`is_exempt`](Self::is_exempt)). Every stop the layer hands out
/// comes from [`crossed`](Self::crossed), which answers an exempt transaction with
/// [`LimitCheck::WithinLimit`] whatever it crossed, and an exempt transaction's pools are
/// unlimited. What it uses is counted all the same, so its usage is reported as any other
/// transaction's is.
#[derive(Clone, Debug, Default)]
pub struct AdditionalLimit {
    limits: EvmTxRuntimeLimits,
    /// What the running execution is, decided when it is begun and sticky for it: the exemption
    /// follows from it ([`is_exempt`](Self::is_exempt)).
    class: TxClass,
    /// The crossing the result of the frame returning next was made of, noted where the stop
    /// became that result ([`note_frame_stop`](Self::note_frame_stop)) and taken when the frame
    /// returns.
    frame_stop: Option<LimitCheck>,
    /// The crossing that produced the result of the transaction's own frame, if one did. See
    /// [`limit_exceeded`](Self::limit_exceeded).
    limit_exceeded: Option<LimitCheck>,
    /// The data-size bytes of the transaction's body.
    body: u64,
    /// The data-size bytes of the Oracle hints the transaction forwarded, never taken back.
    hints: u64,
    /// The write records of the EIP-7702 authorities the transaction applied.
    authorities: u64,
    /// The accounts the EIP-7702 authorities the transaction applied created.
    created_authorities: u64,
    /// Whether the transaction created its caller's account before its first frame: a
    /// deposit-like transaction whose caller had none. Never taken back.
    created_caller: bool,
    /// The stop the transaction's own frame returns before it runs, when its authorization list
    /// would have kept more than a limit allows and was skipped
    /// ([`skip_authorization_list`](Self::skip_authorization_list)).
    pre_frame_stop: Option<LimitCheck>,
    /// The history gas pre-execution charged for the write record the transaction's own frame
    /// makes, kept from that charge until the frame's input is built, which carries it as its
    /// start charge ([`take_first_frame_charge`](Self::take_first_frame_charge)).
    first_frame_charge: Option<u64>,
    /// What the transaction's frames keep, every running frame counted as if it succeeds.
    kept: Kept,
    /// One per start revm decided whose result has not returned, the latest last.
    decided: Vec<Decided>,
    /// The bytes of the body the transaction's intrinsic history gas pays for: the body once
    /// validation admitted that gas, and none before or for a transaction that pays no history.
    intrinsic_history_bytes: u64,
    /// The history gas the settled transaction spent.
    history_gas_spent: u64,
    /// The history bytes the settled transaction appended.
    history_bytes: u64,
    /// The EIP-8037 reservoir the settled transaction or system call left unspent: none from where
    /// an execution builds its gas until it reports what it settled with.
    reservoir_remaining: Option<u64>,
    /// In a debug build, the engine's own ledger of what each open start owes and holds, checked
    /// against the fork's figures at every settlement. It is not begun with the transaction: the
    /// start lifecycle drives it, from the transaction's start to the lifecycle's end.
    #[cfg(debug_assertions)]
    ledger: StartLedger,
}

/// What the running execution is, decided when the layer begins it from what the transaction is
/// and the configuration it runs, and kept until the next one begins: what it is held to and what
/// it pays follow from it, so replacing the transaction the context holds without beginning it
/// changes neither.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TxClass {
    /// The protocol's own work, a system-originated transaction or a system call: held to no
    /// per-transaction limit, not detained, priced at the minimum bucket and paying no history.
    ProtocolOwn,
    /// A user's deposit: held to every limit like any transaction, and paying no history, its
    /// bytes paid for on L1.
    Deposit,
    /// Any other transaction under the neutral configuration, which prices no history: held as
    /// an ordinary transaction is, paying no history. Switching the context to the neutral
    /// configuration makes an ordinary execution this until the next one begins, whatever
    /// configuration the context is switched to in between.
    Neutral,
    /// Any other transaction, which pays history for the bytes it appends.
    #[default]
    Ordinary,
}

/// What a transaction's frames keep: the signed net of each counted dimension, indexed by its
/// pool, and, among the bytes, the EIP-7708 transfer logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Kept {
    /// The net count of each dimension: what the frames added less what they took back.
    net: [i64; PoolId::COUNT],
    /// EIP-7708 transfer logs.
    transfer_logs: u64,
}

impl Kept {
    /// The net count of `kind`.
    const fn net(&self, kind: LimitKind) -> i64 {
        match kind.pool() {
            Some(pool) => self.net[pool.index()],
            None => 0,
        }
    }
}

/// What one write, or one frame start, adds to what a frame keeps and takes back from it, in each
/// counted dimension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counted {
    /// What it adds, per pool: data-size bytes (records, a log's bytes, deployed code, a transfer
    /// log), write records, new accounts and slots.
    pub(crate) added: PoolAmounts,
    /// What it takes back, per pool: the records of accounts and slots returned to what the
    /// transaction loaded, with their bytes; accounts and slots it removes.
    pub(crate) taken: PoolAmounts,
    /// Whether an EIP-7708 transfer log is among the bytes added.
    pub(crate) transfer_log: bool,
}

impl Counted {
    /// Nothing counted.
    pub(crate) const NONE: Self =
        Self { added: PoolAmounts::ZERO, taken: PoolAmounts::ZERO, transfer_log: false };

    /// `added` write records made and `taken` taken back, with an EIP-7708 transfer log when
    /// `transfer_log`: each record is [`WRITE_RECORD_SIZE`] bytes and one KV update.
    pub(crate) const fn records(added: u64, taken: u64, transfer_log: bool) -> Self {
        let log = if transfer_log { TRANSFER_LOG_SIZE } else { 0 };
        Self {
            added: PoolAmounts::ZERO
                .with(DATA_SIZE_POOL, WRITE_RECORD_SIZE.saturating_mul(added).saturating_add(log))
                .with(KV_UPDATE_POOL, added),
            taken: PoolAmounts::ZERO
                .with(DATA_SIZE_POOL, WRITE_RECORD_SIZE.saturating_mul(taken))
                .with(KV_UPDATE_POOL, taken),
            transfer_log,
        }
    }

    /// This count with `rise` new accounts or slots and `fall` accounts or slots it removes: its
    /// state growth.
    pub(crate) const fn with_growth(self, rise: u64, fall: u64) -> Self {
        Self {
            added: self.added.with(STATE_GROWTH_POOL, rise),
            taken: self.taken.with(STATE_GROWTH_POOL, fall),
            transfer_log: self.transfer_log,
        }
    }

    /// `bytes` bytes that are no write record: a log's, or deployed code.
    pub(crate) const fn bytes(bytes: u64) -> Self {
        Self { added: PoolAmounts::ZERO.with(DATA_SIZE_POOL, bytes), ..Self::NONE }
    }

    /// This count with `bytes` more bytes that are no write record taken back: deployed code a
    /// destruction takes back.
    pub(crate) const fn plus_bytes_taken(self, bytes: u64) -> Self {
        let taken = self.taken.get(DATA_SIZE_POOL).saturating_add(bytes);
        Self { taken: self.taken.with(DATA_SIZE_POOL, taken), ..self }
    }

    /// This count and `other` added together: one operation's two parts, whose transfer logs are
    /// at most one between them.
    pub(crate) fn plus(self, other: Self) -> Self {
        debug_assert!(!(self.transfer_log && other.transfer_log), "one transfer log per operation");
        let mut sum = Self { transfer_log: self.transfer_log || other.transfer_log, ..self };
        for pool in PoolId::ALL {
            sum.added.set(pool, self.added.get(pool).saturating_add(other.added.get(pool)));
            sum.taken.set(pool, self.taken.get(pool).saturating_add(other.taken.get(pool)));
        }
        sum
    }

    /// The data-size bytes it adds.
    pub(crate) const fn bytes_added(&self) -> u64 {
        self.added.get(DATA_SIZE_POOL)
    }

    /// The data-size bytes it takes back.
    pub(crate) const fn bytes_taken(&self) -> u64 {
        self.taken.get(DATA_SIZE_POOL)
    }

    /// Whether it adds nothing and takes nothing back.
    const fn is_none(&self) -> bool {
        self.added.is_zero() && self.taken.is_zero()
    }

    /// Whether it adds anything to `pool` or takes anything back from it: a count that leaves a
    /// pool alone crosses neither the frame's budget in it nor, unless the transaction is held to
    /// its limits whatever it counts, the transaction's limit, and a draw and a refill of nothing
    /// change neither the pool nor what the frames keep.
    const fn touches(&self, pool: PoolId) -> bool {
        self.added.get(pool) != 0 || self.taken.get(pool) != 0
    }

    /// What it adds to `pool` less what it takes back from it.
    const fn rise(&self, pool: PoolId) -> i128 {
        self.added.get(pool) as i128 - self.taken.get(pool) as i128
    }
}

/// A start revm decided whose result has not returned.
#[derive(Clone, Copy, Debug)]
struct Decided {
    /// The started frame's depth, which its result returns at.
    depth: usize,
    /// What the frames kept when the start began, before its own records.
    at_start: Kept,
    /// The history gas the start's caller gets back once the frame returned with its start kept:
    /// the price of the records the start took back, less what the caller paid for records the
    /// start did not make.
    credit: u64,
    /// For a creation, its creator as it stood before revm decided the start, which tells once the
    /// creation returned what record its nonce bump made.
    creator: Option<CreatorBefore>,
}

impl AdditionalLimit {
    /// A layer enforcing `limits`.
    pub fn new(limits: EvmTxRuntimeLimits) -> Self {
        Self { limits, ..Default::default() }
    }

    /// The limits enforced.
    pub const fn limits(&self) -> &EvmTxRuntimeLimits {
        &self.limits
    }

    /// Replaces the limits enforced from the next transaction on.
    pub(crate) const fn set_limits(&mut self, limits: EvmTxRuntimeLimits) {
        self.limits = limits;
    }

    /// Clears the state for a new transaction, keeping allocations and the limits, and begins it:
    /// a transaction of `class`, its body weighing `body` bytes.
    ///
    /// The exemption is decided before the body is counted: the protocol's own work is exempt
    /// whatever it carries.
    pub(crate) fn begin_transaction(&mut self, class: TxClass, body: u64) {
        self.class = class;
        self.frame_stop = None;
        self.limit_exceeded = None;
        self.body = body;
        self.hints = 0;
        self.authorities = 0;
        self.created_authorities = 0;
        self.created_caller = false;
        self.pre_frame_stop = None;
        self.first_frame_charge = None;
        self.kept = Kept::default();
        self.decided.clear();
        self.intrinsic_history_bytes = 0;
        self.history_gas_spent = 0;
        self.history_bytes = 0;
        self.reservoir_remaining = None;
    }

    /* What the transaction keeps */

    /// What the transaction keeps so far, every running frame counted as if it succeeds: its
    /// data-size bytes, write records and state growth, each the dimension's net, never below zero.
    pub(crate) const fn usage(&self) -> LimitUsage {
        LimitUsage {
            data_size: positive(self.net(LimitKind::DataSize)),
            write_records: positive(self.net(LimitKind::KvUpdate)),
            state_growth: positive(self.net(LimitKind::StateGrowth)),
        }
    }

    /// The transaction's net count of `kind`: what sits on no frame — the body, the hints, the
    /// authorities' records and the accounts they created, the caller a deposit created — and
    /// what the frames keep.
    const fn net(&self, kind: LimitKind) -> i128 {
        let frames = self.kept.net(kind) as i128;
        let outside = match kind {
            LimitKind::DataSize => {
                self.body as i128 +
                    self.hints as i128 +
                    WRITE_RECORD_SIZE as i128 * self.authorities as i128
            }
            LimitKind::KvUpdate => self.authorities as i128,
            LimitKind::StateGrowth => {
                self.created_authorities as i128 + self.created_caller as i128
            }
            LimitKind::ComputeGas => 0,
        };
        outside + frames
    }

    /// The transaction's limit on `kind`.
    const fn tx_limit(&self, kind: LimitKind) -> u64 {
        match kind {
            LimitKind::DataSize => self.limits.tx_data_size_limit,
            LimitKind::KvUpdate => self.limits.tx_kv_update_limit,
            LimitKind::StateGrowth => self.limits.tx_state_growth_limit,
            LimitKind::ComputeGas => u64::MAX,
        }
    }

    /// The data-size bytes of the transaction's body, which it counts before any frame and no
    /// revert takes back.
    pub(crate) const fn body(&self) -> u64 {
        self.body
    }

    /// The bytes of the body the transaction's intrinsic history gas pays for: the body once
    /// validation admitted that gas ([`admit_body_history`](Self::admit_body_history)), and none
    /// before or for a transaction that pays no history.
    pub(crate) const fn intrinsic_history_bytes(&self) -> u64 {
        self.intrinsic_history_bytes
    }

    /// Records that validation admitted the history gas of the transaction's body: its gas limit
    /// covers it, and the transaction pays it before its first frame.
    pub(crate) const fn admit_body_history(&mut self) {
        self.intrinsic_history_bytes = self.body;
    }

    /// Whether the transaction's body, with one write record for each of its `authorizations`,
    /// fits its data-size limit: what validation holds a transaction to.
    ///
    /// Every entry of the authorization list counts, whether it will apply or not, so the figure is
    /// an upper bound on what the transaction counts before its first frame — the body, then one
    /// record for each distinct authority that applies — and a transaction validation admits
    /// cannot cross the limit there. An exempt transaction is held to nothing.
    pub(crate) const fn admits_tx_body(&self, authorizations: u64) -> bool {
        self.is_exempt() ||
            self.body.saturating_add(WRITE_RECORD_SIZE.saturating_mul(authorizations)) <=
                self.limits.tx_data_size_limit
    }

    /// Counts the records of the applied EIP-7702 authorities, `authorities` distinct accounts
    /// other than the transaction's sender, and the `created` accounts among them the application
    /// created, made before the first frame. An out-of-gas before the first frame takes them back
    /// ([`take_back_authorities`](Self::take_back_authorities)).
    ///
    /// The records cannot cross the data-size limit: validation admitted the body with a record for
    /// every entry of the authorization list ([`admits_tx_body`](Self::admits_tx_body)), and only
    /// an entry can apply. Neither they nor the created accounts cross the KV or the state-growth
    /// limit: a list that would was skipped before it was applied
    /// ([`hold_authorization_list`](Self::hold_authorization_list)). A debug build asserts both.
    pub(crate) fn count_authorities(&mut self, authorities: u64, created: u64) {
        self.authorities = authorities;
        self.created_authorities = created;
        debug_assert!(
            self.is_exempt() || !self.usage().exceeds_data_size(self.limits.tx_data_size_limit),
            "validation admits no body whose authority records cross the data-size limit"
        );
        debug_assert!(
            self.is_exempt() ||
                (authorities == 0 && created == 0) ||
                [LimitKind::KvUpdate, LimitKind::StateGrowth]
                    .iter()
                    .all(|&kind| self.net(kind) <= self.tx_limit(kind) as i128),
            "an authorization list that would cross the KV or state-growth limit is skipped"
        );
    }

    /// Takes back the authorities' records and the accounts they created, which an out-of-gas
    /// before the first frame takes back with the authorizations.
    pub(crate) const fn take_back_authorities(&mut self) {
        self.authorities = 0;
        self.created_authorities = 0;
    }

    /// Counts the caller account a deposit-like transaction creates before its first frame: one
    /// account of state growth on the transaction, which no failure takes back, the deposit's own
    /// included. It is one account, so it cannot cross a state-growth limit of one or more by
    /// itself, and it is held to none: under a limit of nothing it leaves the first frame nothing.
    /// A deposit carries no authorization list, so nothing else is counted before its frame.
    pub(crate) const fn count_created_caller(&mut self) {
        self.created_caller = true;
    }

    /// Whether an authorization list of `entries` could keep more than the transaction's KV or
    /// state-growth limit allows: each entry changes one authority at most and creates one at
    /// most, so a list no longer than what both limits leave cannot cross them, and needs no
    /// simulation.
    pub(crate) const fn authorization_list_may_cross(&self, entries: u64) -> bool {
        let records = self.tx_limit(LimitKind::KvUpdate) as i128 - self.net(LimitKind::KvUpdate);
        let created =
            self.tx_limit(LimitKind::StateGrowth) as i128 - self.net(LimitKind::StateGrowth);
        !self.is_exempt() && (entries as i128 > records || entries as i128 > created)
    }

    /// The verdict on an authorization list that would keep `records` write records and create
    /// `created` accounts were it applied, before the first frame: KV updates first, then state
    /// growth, each against the transaction's limit, since no frame budget exists yet.
    pub(crate) const fn hold_authorization_list(&self, records: u64, created: u64) -> LimitCheck {
        let counted = Counted::records(records, 0, false).with_growth(created, 0);
        let check = self.transaction_crossed(LimitKind::KvUpdate, &counted, false);
        if check.exceeded_limit() {
            return check;
        }
        self.transaction_crossed(LimitKind::StateGrowth, &counted, false)
    }

    /// Skips the transaction's authorization list, which would have kept more than `stop` allows:
    /// none of it is applied or counted, and the transaction's own frame returns `stop` before it
    /// runs ([`take_pre_frame_stop`](Self::take_pre_frame_stop)).
    pub(crate) fn skip_authorization_list(&mut self, stop: LimitCheck) {
        debug_assert!(stop.exceeded_limit(), "only a crossing skips the list");
        self.pre_frame_stop = Some(stop);
    }

    /// The stop the transaction's own frame returns before it runs, if its authorization list was
    /// skipped; taken once.
    pub(crate) const fn take_pre_frame_stop(&mut self) -> Option<LimitCheck> {
        self.pre_frame_stop.take()
    }

    /// Keeps `history`, the history gas pre-execution just charged for the write record the
    /// transaction's own frame makes, for the frame's input to carry
    /// ([`take_first_frame_charge`](Self::take_first_frame_charge)).
    pub(crate) const fn keep_first_frame_charge(&mut self, history: u64) {
        self.first_frame_charge = Some(history);
    }

    /// The history gas pre-execution charged for the write record the transaction's own frame
    /// makes; taken once, by the frame's input. `None` where no pre-execution charged it since
    /// the last was taken: a system call, which skips pre-execution.
    pub(crate) const fn take_first_frame_charge(&mut self) -> Option<u64> {
        self.first_frame_charge.take()
    }

    /// The pools of the transaction's own frame: what the transaction has left of each limit once
    /// its body and the counts made before the frame are counted, the data size under the frame
    /// cap; unlimited for an exempt transaction.
    pub(crate) const fn first_frame_pools(&self) -> PoolAmounts {
        if self.is_exempt() {
            return PoolAmounts::UNLIMITED;
        }
        let caps = self.frame_caps();
        let mut pools = PoolAmounts::UNLIMITED;
        let mut index = 0;
        while index < COUNTED.len() {
            let kind = COUNTED[index];
            let Some(pool) = kind.pool() else { unreachable!() };
            let left = self.tx_limit(kind) as i128 - self.net(kind);
            let left = if left < 0 {
                0
            } else if left > u64::MAX as i128 {
                u64::MAX
            } else {
                left as u64
            };
            let cap = caps.get(pool);
            pools.set(pool, if left < cap { left } else { cap });
            index += 1;
        }
        pools
    }

    /// The most each of a frame's pools may start with: the data-size frame cap, and no cap on the
    /// other pools; nothing for an exempt transaction.
    pub(crate) const fn frame_caps(&self) -> PoolAmounts {
        if self.is_exempt() {
            return PoolAmounts::UNLIMITED;
        }
        PoolAmounts::UNLIMITED.with(DATA_SIZE_POOL, self.limits.frame_data_size_limit)
    }

    /* Verdicts */

    /// The verdict on `used` crossing `kind`'s `limit`, a frame budget when `frame_local` and a
    /// transaction-level limit otherwise: the stop the running frame reverts with, alone, its
    /// [`LimitCheck::revert_data`] naming the limit crossed.
    ///
    /// Every stop comes from here, so this is where the exemption applies: an exempt transaction's
    /// verdict is [`LimitCheck::WithinLimit`], whatever it crossed.
    pub(crate) const fn crossed(
        &self,
        kind: LimitKind,
        limit: u64,
        used: u64,
        frame_local: bool,
    ) -> LimitCheck {
        if self.is_exempt() {
            return LimitCheck::WithinLimit;
        }
        LimitCheck::ExceedsLimit { kind, limit, used, frame_local }
    }

    /// The verdict on a frame budget of `kind`, whose pool holds `remaining` after a net draw of
    /// `spent`, crossed by `rise` more: the budget is the pool the frame started with, which its
    /// pool and its net draw add up to, and what it used is its net draw with the rise in.
    fn budget_crossed(&self, kind: LimitKind, remaining: u64, spent: i64, rise: u64) -> LimitCheck {
        let budget = remaining.saturating_add_signed(spent);
        let used = positive(spent as i128 + rise as i128);
        self.crossed(kind, budget, used, true)
    }

    /// The verdict on the transaction keeping `counted` more of `kind`: its limit crossed, or
    /// within. A count that does not rise crosses nothing, unless `held` asks for the transaction
    /// to be held to its limit whatever it counts.
    const fn transaction_crossed(
        &self,
        kind: LimitKind,
        counted: &Counted,
        held: bool,
    ) -> LimitCheck {
        let Some(pool) = kind.pool() else { return LimitCheck::WithinLimit };
        let rise = counted.rise(pool);
        if !held && rise <= 0 {
            return LimitCheck::WithinLimit;
        }
        let used = self.net(kind) + rise;
        let limit = self.tx_limit(kind);
        if used > limit as i128 {
            return self.crossed(kind, limit, positive(used), false);
        }
        LimitCheck::WithinLimit
    }

    /// The verdict on `counted` kept on a frame whose pools `gas` holds: in each counted dimension
    /// in order, the frame's budget, then the transaction's limit; the first crossing is the
    /// verdict. `held` holds the transaction to its limits whatever `counted` adds; otherwise a
    /// dimension `counted` leaves alone crosses nothing and is not looked at
    /// ([`Counted::touches`]).
    fn check(&self, gas: &GasTracker, counted: &Counted, held: bool) -> LimitCheck {
        for kind in COUNTED {
            let Some(pool) = kind.pool() else { continue };
            if !held && !counted.touches(pool) {
                debug_assert!(
                    self.transaction_crossed(kind, counted, false).within_limit(),
                    "a dimension a count leaves alone crosses nothing"
                );
                continue;
            }
            let rise = counted.rise(pool);
            let remaining = gas.pool_remaining(pool);
            if rise > remaining as i128 {
                let spent = gas.pool_spent(pool);
                let check = self.budget_crossed(kind, remaining, spent, rise as u64);
                if check.exceeded_limit() {
                    return check;
                }
            }
            let check = self.transaction_crossed(kind, counted, held);
            if check.exceeded_limit() {
                return check;
            }
        }
        LimitCheck::WithinLimit
    }

    /// Counts what the frame whose pools `gas` holds keeps of `counted`, held to the frame's
    /// budgets and the transaction's limits in the order [`check`](Self::check) holds them: the
    /// amounts `counted` takes back go back to each pool first, and the amounts it adds are drawn.
    /// A crossing counts nothing and draws nothing, and is the stop the frame reverts with. A
    /// count that adds nothing and takes nothing holds nothing.
    pub(crate) fn draw(&mut self, gas: &mut GasTracker, counted: &Counted) -> LimitCheck {
        if counted.is_none() {
            return LimitCheck::WithinLimit;
        }
        let check = self.check(gas, counted, false);
        if check.exceeded_limit() {
            return check;
        }
        Self::apply(gas, counted);
        self.count(counted);
        #[cfg(debug_assertions)]
        self.ledger.draw(counted);
        LimitCheck::WithinLimit
    }

    /// Draws `counted` on the pools `gas` holds, which the checks made sure can pay it; a pool it
    /// leaves alone is not visited.
    fn apply(gas: &mut GasTracker, counted: &Counted) {
        for pool in PoolId::ALL {
            if !counted.touches(pool) {
                continue;
            }
            gas.refill_pool(pool, counted.taken.get(pool));
            let drew = gas.record_pool(pool, counted.added.get(pool));
            debug_assert!(
                drew || gas.pool_remaining(pool) == u64::MAX,
                "the checks admit the draw"
            );
            if !drew {
                gas.clear_pool_crossing(pool);
            }
        }
    }

    /// Adds `counted` to what the frames keep.
    ///
    /// What a frame takes back the transaction's frames counted: a record or a byte taken back was
    /// counted by a frame that is still on the stack or merged into one, so the transaction's net
    /// stays at or above zero, which a debug build asserts. A frame's own net goes below zero when
    /// it takes back what another frame counted — a slot its caller wrote, cleared, or the parts of
    /// an account the transaction created, given back to the frame that destroys it — and the
    /// signed net merges into its caller as any net does.
    fn count(&mut self, counted: &Counted) {
        let kept = &mut self.kept;
        for pool in PoolId::ALL {
            if !counted.touches(pool) {
                continue;
            }
            let net = &mut kept.net[pool.index()];
            *net = net
                .saturating_add_unsigned(counted.added.get(pool))
                .saturating_sub_unsigned(counted.taken.get(pool));
        }
        debug_assert!(
            kept.net[DATA_SIZE_POOL.index()] >= 0 && kept.net[KV_UPDATE_POOL.index()] >= 0,
            "a frame takes back only what the transaction's frames keep"
        );
        kept.transfer_logs += u64::from(counted.transfer_log);
    }

    /// Admits an Oracle hint of `bytes`: held to the transaction's limit before it is counted, and
    /// never taken back once counted. A hint is on no frame's budget.
    pub(crate) fn admit_hint(&mut self, bytes: u64) -> LimitCheck {
        let check = self.transaction_crossed(LimitKind::DataSize, &Counted::bytes(bytes), false);
        if check.within_limit() {
            self.hints = self.hints.saturating_add(bytes);
        }
        check
    }

    /* A frame's start */

    /// Opens the start of the frame at `depth` that revm just decided, before its records are
    /// counted: what the frames keep now is what the frame's failure restores
    /// ([`close_start`](Self::close_start)). A creation's `creator`, read before revm decided it,
    /// is handed back when the creation returns.
    pub(crate) fn open_start(&mut self, depth: usize, creator: Option<CreatorBefore>) {
        self.decided.push(Decided { depth, at_start: self.kept, credit: 0, creator });
    }

    /// Holds the start revm just decided — a frame it built, or a call it answered with a success —
    /// whose frame's pools `gas` holds, to the limits, and counts `counted`, the records, state
    /// growth and transfer log it made, read off the journal, on those pools
    /// ([`draw`](Self::draw)): the verdict is the stop the frame returns before it runs, or the
    /// one its answer gives way to. An answer that failed kept nothing and is not held. `credit` is
    /// what the start's caller gets back once the frame returned with its start kept.
    ///
    /// A start is held to the transaction's limits whatever it counts, nothing included: what the
    /// transaction keeps may already stand above a limit, as a frame that resumes may find it
    /// ([`hold_caller`](Self::hold_caller)), and the caller a deposit-like transaction created is
    /// counted unchecked ([`count_created_caller`](Self::count_created_caller)); a start is where
    /// a frame first meets it.
    pub(crate) fn count_start(
        &mut self,
        gas: &mut GasTracker,
        counted: &Counted,
        credit: u64,
    ) -> LimitCheck {
        let check = self.check(gas, counted, true);
        if check.exceeded_limit() {
            return check;
        }
        Self::apply(gas, counted);
        self.count(counted);
        #[cfg(debug_assertions)]
        self.ledger.draw(counted);
        if let Some(start) = self.decided.last_mut() {
            start.credit = credit;
        }
        LimitCheck::WithinLimit
    }

    /// Closes the start of the frame whose result returns at `depth`, `kept` or not, and answers
    /// the history its caller gets back for it, with the creator of a creation as it stood before
    /// revm decided the start: a frame whose result says it failed has what the frames keep
    /// restored to what they kept when its start began, as its pools are, and its caller gets
    /// nothing back; one that kept its start, the start's credit.
    ///
    /// A creation's creator is what its nonce record is read from once the result merged into the
    /// creator ([`Accounts::creator_counted`](super::Accounts)): the record is the creator's, so it
    /// is counted after the creation's own counts are closed, and outlives the creation's failure.
    ///
    /// A result revm did not decide the start of — the depth guard's, an interceptor's, an
    /// inspector's — has no start to close, and no creator: revm bumped no nonce for it. The depth
    /// guard's and an interceptor's carry the start charge their input declared, which revm gives
    /// back; an inspector's carries what the inspector built it with.
    pub(crate) fn close_start(&mut self, depth: usize, kept: bool) -> (u64, Option<CreatorBefore>) {
        let Some(&start) = self.decided.last().filter(|start| start.depth == depth) else {
            return (0, None);
        };
        self.decided.pop();
        if kept {
            (start.credit, start.creator)
        } else {
            self.kept = start.at_start;
            (0, start.creator)
        }
    }

    /// The stop the frame a child just returned into returns before it runs on, if what the
    /// transaction keeps now stands above one of its limits: in each counted dimension in order,
    /// held whatever the return counted.
    ///
    /// A child's success hands its caller nothing the limits did not hold as it was counted. A
    /// failure restores what the frames kept when its start began, which is more than they keep
    /// now when the child took back records or state a frame above it made, while what the
    /// transaction keeps on no frame stays: an Oracle hint the child sent into the room the take
    /// back freed leaves the transaction above its limit, and the caller is stopped for it.
    pub(crate) fn hold_caller(&self) -> Option<LimitCheck> {
        for kind in COUNTED {
            let check = self.transaction_crossed(kind, &Counted::NONE, true);
            if check.exceeded_limit() {
                return Some(check);
            }
        }
        None
    }

    /* The stops and the exemption */

    /// The crossing that produced the result of the transaction's own frame, if one did: what
    /// [`MegaTransactionOutcome::limit_exceeded`](crate::MegaTransactionOutcome) reports.
    ///
    /// It is a crossing the transaction's own frame made, of the transaction's limit or of its own
    /// budget. A crossing below that frame produced the result of the frame that crossed, which its
    /// caller caught: a caller that then reverts with the same bytes made its own result, and is no
    /// stop. Neither is a stop an inspector rewrote into another result: the frame's result must
    /// still be the stop's revert.
    ///
    /// It is the running transaction's, or the last one's until the next transaction or system
    /// call starts: each resets it first.
    pub const fn limit_exceeded(&self) -> Option<&LimitCheck> {
        self.limit_exceeded.as_ref()
    }

    /// Notes `stop` as the crossing the result of the frame returning next is made of, and gives
    /// the [`MegaLimitExceeded`](crate::MegaLimitExceeded) revert data that frame stops with.
    ///
    /// Every site that turns a crossing into a frame's result takes the revert data from here, and
    /// the frame returns before any other does: so the stop is the returning frame's when its
    /// result returns ([`take_frame_stop`](Self::take_frame_stop)), and the transaction's when
    /// that frame is the transaction's own ([`limit_exceeded`](Self::limit_exceeded)).
    pub(crate) fn note_frame_stop(&mut self, stop: &LimitCheck) -> Bytes {
        debug_assert!(stop.exceeded_limit(), "only a crossing stops a frame");
        self.frame_stop = Some(*stop);
        stop.revert_data()
    }

    /// Takes the stop noted for the frame whose `result` returns now; for the transaction's own
    /// frame (`outermost`), it is the transaction's [`limit_exceeded`](Self::limit_exceeded) while
    /// the result is still the stop's revert. A frame with no stop noted leaves the transaction's
    /// as it is: the transaction's own frame returns through here once.
    pub(crate) fn take_frame_stop(&mut self, result: &FrameResult, outermost: bool) {
        let Some(noted) = self.frame_stop.take() else { return };
        if outermost {
            self.limit_exceeded = Some(noted).filter(|stop| is_the_revert_of(result, stop));
        }
    }

    /// Whether the running transaction is exempt from every per-transaction limit: the protocol's
    /// own work is.
    pub const fn is_exempt(&self) -> bool {
        matches!(self.class, TxClass::ProtocolOwn)
    }

    /// What the running (or last) execution is, decided when it was begun.
    pub(crate) const fn class(&self) -> TxClass {
        self.class
    }

    /// Makes the running (or last) execution pay no history where it would, as the neutral
    /// configuration the context is switched to prices none; the next execution the layer begins
    /// decides again.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) const fn price_no_history(&mut self) {
        if matches!(self.class, TxClass::Ordinary) {
            self.class = TxClass::Neutral;
        }
    }

    /* The start ledger */

    /// The engine's own ledger of what each open start owes and holds, which a debug build keeps
    /// beside the fork's figures and checks against them.
    #[cfg(debug_assertions)]
    pub(crate) const fn ledger_mut(&mut self) -> &mut StartLedger {
        &mut self.ledger
    }

    /* What the settled transaction reports */

    /// The history gas the last settled transaction spent.
    pub const fn history_gas_spent(&self) -> u64 {
        self.history_gas_spent
    }

    /// The EIP-8037 reservoir the last settled transaction or system call left unspent, which
    /// its sender gets back, as a transaction reports it
    /// ([`MegaGasUsage::reservoir_remaining`](crate::MegaGasUsage::reservoir_remaining)). It is
    /// read before the EIP-7623 floor absorbs it, and a failed deposit leaves none.
    pub const fn reservoir_remaining(&self) -> u64 {
        match self.reservoir_remaining {
            Some(reservoir) => reservoir,
            None => 0,
        }
    }

    /// Forgets the reservoir the last execution reported: the running one builds its gas, and
    /// reports what it leaves unspent once it settles.
    pub(crate) const fn forget_reservoir_remaining(&mut self) {
        self.reservoir_remaining = None;
    }

    /// Records the reservoir the settled transaction or system call left unspent.
    pub(crate) const fn set_reservoir_remaining(&mut self, reservoir: u64) {
        self.reservoir_remaining = Some(reservoir);
    }

    /// Records `reservoir` as what the settled execution left unspent, unless the execution
    /// reported it already, where it read it before the result was built.
    pub(crate) fn set_reservoir_remaining_unless_reported(&mut self, reservoir: u64) {
        self.reservoir_remaining.get_or_insert(reservoir);
    }

    /// The history bytes the last settled transaction appended, whoever paid for them, which a
    /// transaction reports as [`MegaGasUsage::history_bytes`](crate::MegaGasUsage::history_bytes);
    /// [`history_gas_spent`](Self::history_gas_spent) is what the transaction's own gas paid of
    /// their price.
    pub const fn history_bytes(&self) -> u64 {
        self.history_bytes
    }

    /// Settles the history figures once the transaction's own frame returned: `spent`, the history
    /// gas its gas spent, and the history bytes it appended when it pays history at all (`priced`),
    /// which are its body and the bytes it keeps but for its Oracle hints and its transfer logs.
    pub(crate) fn settle_history(&mut self, priced: bool, spent: u64) {
        self.history_gas_spent = spent;
        self.history_bytes = if priced {
            self.intrinsic_history_bytes
                .saturating_add(WRITE_RECORD_SIZE.saturating_mul(self.authorities))
                .saturating_add(positive(self.kept.net(LimitKind::DataSize) as i128))
                .saturating_sub(TRANSFER_LOG_SIZE.saturating_mul(self.kept.transfer_logs))
        } else {
            0
        };
    }
}

/// `value`, or zero when it is negative, as a `u64` that saturates.
const fn positive(value: i128) -> u64 {
    if value <= 0 {
        0
    } else if value > u64::MAX as i128 {
        u64::MAX
    } else {
        value as u64
    }
}

/// Whether `result` is the revert `stop` makes a frame return: a revert whose output is the
/// stop's [`MegaLimitExceeded`](crate::MegaLimitExceeded).
fn is_the_revert_of(result: &FrameResult, stop: &LimitCheck) -> bool {
    let result = result.interpreter_result();
    result.result == InstructionResult::Revert && result.output == stop.revert_data()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A count is none exactly when it adds and takes back nothing in every pool: `NONE` and the
    /// default are, and so is a count of no records or no bytes; a count that adds or takes back
    /// a record, adds bytes or a transfer log, or moves state growth either way is not.
    #[test]
    fn test_a_count_is_none_only_when_it_moves_no_pool() {
        let cases = [
            ("NONE", Counted::NONE, true),
            ("the default", Counted::default(), true),
            ("no records", Counted::records(0, 0, false), true),
            ("no bytes", Counted::bytes(0), true),
            ("no growth", Counted::NONE.with_growth(0, 0), true),
            ("a record made", Counted::records(1, 0, false), false),
            ("a record taken back", Counted::records(0, 1, false), false),
            ("a transfer log", Counted::records(0, 0, true), false),
            ("bytes", Counted::bytes(1), false),
            ("a rise", Counted::NONE.with_growth(1, 0), false),
            ("a fall", Counted::NONE.with_growth(0, 1), false),
        ];
        for (case, counted, none) in cases {
            assert_eq!(counted.is_none(), none, "{case}: {counted:?}");
        }
    }

    /// An exempt layer answers every crossing as within the limits, in any dimension and at the
    /// transaction or at a frame; the next transaction begins held to its limits again.
    #[test]
    fn test_the_exemption_answers_every_crossing_and_lasts_one_transaction() {
        let mut limit = AdditionalLimit::default();
        limit.begin_transaction(TxClass::ProtocolOwn, 0);
        assert!(limit.is_exempt());
        for kind in [LimitKind::DataSize, LimitKind::ComputeGas] {
            for frame_local in [false, true] {
                let verdict = limit.crossed(kind, 0, 1, frame_local);
                assert_eq!(verdict, LimitCheck::WithinLimit, "{kind:?}, frame-local {frame_local}");
            }
        }
        limit.begin_transaction(TxClass::Ordinary, 0);
        assert!(!limit.is_exempt());
        assert_eq!(
            limit.crossed(LimitKind::ComputeGas, 0, 1, false),
            LimitCheck::ExceedsLimit {
                kind: LimitKind::ComputeGas,
                limit: 0,
                used: 1,
                frame_local: false
            },
            "the next transaction is held to its limits"
        );
    }

    /// A frame budget admits a count that uses exactly what the pool holds and stops one more:
    /// the boundary is the pool's remaining amount, in each counted pool, with the transaction's
    /// limit out of the way so only the frame's budget decides.
    #[test]
    fn test_a_count_equal_to_the_pools_remaining_is_within_the_frame_budget() {
        let remaining = 6_u64;
        for pool in PoolId::ALL {
            for (added, within) in
                [(remaining - 1, true), (remaining, true), (remaining + 1, false)]
            {
                let mut layer = AdditionalLimit::new(EvmTxRuntimeLimits::no_limits());
                layer.begin_transaction(TxClass::Ordinary, 0);
                let mut gas = GasTracker::new(1_000, 1_000, 0);
                gas.set_pools_remaining(PoolAmounts::ZERO.with(pool, remaining));
                let mut counted = Counted::NONE;
                counted.added.set(pool, added);
                let verdict = layer.draw(&mut gas, &counted);
                let case = format!("{pool:?}, adds {added} to {remaining}");
                if within {
                    assert_eq!(verdict, LimitCheck::WithinLimit, "{case}");
                    assert_eq!(gas.pool_remaining(pool), remaining - added, "{case}");
                    assert_eq!(
                        layer.kept.net[pool.index()],
                        i64::try_from(added).unwrap(),
                        "{case}"
                    );
                } else {
                    assert!(verdict.exceeded_limit(), "{case}");
                    assert!(
                        matches!(verdict, LimitCheck::ExceedsLimit { frame_local: true, .. }),
                        "{case}: {verdict:?}"
                    );
                    assert_eq!(gas.pool_remaining(pool), remaining, "{case}: nothing drawn");
                    assert_eq!(layer.kept.net[pool.index()], 0, "{case}: nothing counted");
                }
            }
        }
    }

    /// `draw` as it was before it learned to leave alone the pools a count does not touch: every
    /// pool's budget and every dimension's limit checked off a snapshot of the pools, and every
    /// pool drawn, refilled and counted. The reference [`draw`](AdditionalLimit::draw) is held to.
    fn draw_by_every_pool(
        limit: &mut AdditionalLimit,
        gas: &mut GasTracker,
        counted: &Counted,
    ) -> LimitCheck {
        if counted.is_none() {
            return LimitCheck::WithinLimit;
        }
        let remaining = gas.pools_remaining();
        let spent = PoolId::ALL.map(|pool| gas.pool_spent(pool));
        for kind in COUNTED {
            let Some(pool) = kind.pool() else { continue };
            let rise = counted.rise(pool);
            if rise > 0 && rise > remaining.get(pool) as i128 {
                let check = limit.budget_crossed(
                    kind,
                    remaining.get(pool),
                    spent[pool.index()],
                    rise as u64,
                );
                if check.exceeded_limit() {
                    return check;
                }
            }
            let check = limit.transaction_crossed(kind, counted, false);
            if check.exceeded_limit() {
                return check;
            }
        }
        for pool in PoolId::ALL {
            gas.refill_pool(pool, counted.taken.get(pool));
            if !gas.record_pool(pool, counted.added.get(pool)) {
                gas.clear_pool_crossing(pool);
            }
            let net = &mut limit.kept.net[pool.index()];
            *net = net
                .saturating_add_unsigned(counted.added.get(pool))
                .saturating_sub_unsigned(counted.taken.get(pool));
        }
        limit.kept.transfer_logs += u64::from(counted.transfer_log);
        LimitCheck::WithinLimit
    }

    /// A draw that visits only the pools its count touches decides, draws and counts exactly as
    /// one that visits every pool: for every count that adds to, takes back from or leaves alone
    /// each of the three pools, with and without a transfer log, on frames whose budget in each
    /// pool is exhausted, short of the count or ample, under transaction limits that the count
    /// crosses or does not, after frames kept nothing, a little or more, and for an exempt
    /// transaction, whose pools are unlimited. A count takes back no more data size and no more
    /// records than the frames keep, which the engine never does and a debug build asserts.
    #[test]
    fn test_a_draw_over_the_pools_it_touches_matches_one_over_every_pool() {
        // What a count does to one pool: nothing, add, take back, or both.
        let moves = [(0, 0), (5, 0), (0, 2), (7, 3)];
        // A frame's pool before the draw: empty, short of the count, ample; an exempt
        // transaction's frames start with unlimited pools.
        let mut cases = 0;
        for (class, pools) in
            [(TxClass::Ordinary, &[0_u64, 4, 100][..]), (TxClass::ProtocolOwn, &[u64::MAX - 2][..])]
        {
            for tx_limit in [10_u64, u64::MAX] {
                for kept in [0_u64, 3, 8] {
                    let limits = EvmTxRuntimeLimits::no_limits()
                        .with_tx_data_size_limit(tx_limit)
                        .with_tx_kv_update_limit(tx_limit)
                        .with_tx_state_growth_limit(tx_limit);
                    let mut layer = AdditionalLimit::new(limits);
                    layer.begin_transaction(class, 0);
                    let mut prior = Counted::NONE;
                    for pool in PoolId::ALL {
                        prior.added.set(pool, kept);
                    }
                    layer.count(&prior);
                    for a in moves {
                        for b in moves {
                            for c in moves {
                                for transfer_log in [false, true] {
                                    for &pa in pools {
                                        for &pb in pools {
                                            for &pc in pools {
                                                let mut counted = Counted::NONE;
                                                let pools = [
                                                    DATA_SIZE_POOL,
                                                    KV_UPDATE_POOL,
                                                    STATE_GROWTH_POOL,
                                                ];
                                                for (pool, (added, taken)) in
                                                    pools.into_iter().zip([a, b, c])
                                                {
                                                    counted.added.set(pool, added);
                                                    counted.taken.set(pool, taken);
                                                }
                                                counted.transfer_log = transfer_log;
                                                let takes_more = [DATA_SIZE_POOL, KV_UPDATE_POOL]
                                                    .into_iter()
                                                    .any(|pool| counted.taken.get(pool) > kept);
                                                if takes_more {
                                                    continue;
                                                }
                                                // Two drawn already, so a frame's net is not zero.
                                                let mut gas = GasTracker::new(1_000, 1_000, 0);
                                                gas.set_pools_remaining(
                                                    PoolAmounts::ZERO
                                                        .with(DATA_SIZE_POOL, pa + 2)
                                                        .with(KV_UPDATE_POOL, pb + 2)
                                                        .with(STATE_GROWTH_POOL, pc + 2),
                                                );
                                                for pool in PoolId::ALL {
                                                    assert!(gas.record_pool(pool, 2));
                                                }
                                                let (mut by_touched, mut by_every) =
                                                    (layer.clone(), layer.clone());
                                                let (mut gas_touched, mut gas_every) = (gas, gas);
                                                let verdict =
                                                    by_touched.draw(&mut gas_touched, &counted);
                                                let reference = draw_by_every_pool(
                                                    &mut by_every,
                                                    &mut gas_every,
                                                    &counted,
                                                );
                                                let case = format!(
                                                    "{class:?}, limit {tx_limit}, kept {kept}, \
                                                     {counted:?}, pools {pa}/{pb}/{pc}"
                                                );
                                                assert_eq!(verdict, reference, "{case}");
                                                assert_eq!(
                                                    by_touched.kept, by_every.kept,
                                                    "{case}"
                                                );
                                                assert_eq!(
                                                    gas_touched.pools_remaining(),
                                                    gas_every.pools_remaining(),
                                                    "{case}"
                                                );
                                                for pool in PoolId::ALL {
                                                    assert_eq!(
                                                        (
                                                            gas_touched.pool_spent(pool),
                                                            gas_touched.pool_crossing(pool)
                                                        ),
                                                        (
                                                            gas_every.pool_spent(pool),
                                                            gas_every.pool_crossing(pool)
                                                        ),
                                                        "{case}"
                                                    );
                                                }
                                                cases += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // With nothing kept, a count takes nothing back from the first two pools: 2 of 4 moves
        // each, so 16 of the 64 moves; with 3 or 8 kept, every move. The ordinary transaction runs
        // on 27 pool states, the exempt one on one.
        assert_eq!(cases, 2 * (16 + 64 + 64) * 2 * (27 + 1));
    }

    /// The history pre-execution charged for the first frame's record reaches that frame's input
    /// once: taken, it is gone, and an execution that skips pre-execution — a system call, begun
    /// or run straight through the handler after a transaction — finds none and prices its own.
    #[test]
    fn test_the_first_frame_charge_is_taken_once_and_lasts_one_transaction() {
        let mut limit = AdditionalLimit::default();
        limit.begin_transaction(TxClass::Ordinary, 0);
        assert_eq!(limit.take_first_frame_charge(), None, "nothing charged yet");
        limit.keep_first_frame_charge(7);
        assert_eq!(limit.take_first_frame_charge(), Some(7));
        assert_eq!(limit.take_first_frame_charge(), None, "taken by the frame's input");

        limit.keep_first_frame_charge(7);
        limit.begin_transaction(TxClass::ProtocolOwn, 0);
        assert_eq!(limit.take_first_frame_charge(), None, "the next execution begins without it");
    }

    /// Validation admits no body whose authority records cross the data-size limit, so a debug
    /// build trips on records that do: under a limit lowered to the body and one record, two
    /// applied authorities cross it.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "validation admits no body whose authority records cross")]
    fn test_authority_records_that_cross_the_limit_trip() {
        let limits = EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(crate::TX_BODY_SIZE + WRITE_RECORD_SIZE);
        let mut limit = AdditionalLimit::new(limits);
        limit.begin_transaction(TxClass::Ordinary, crate::TX_BODY_SIZE);
        limit.count_authorities(2, 0);
    }

    /// An authorization list may cross exactly when it has more entries than the KV or the
    /// state-growth limit leaves the transaction, the limit less what the transaction already
    /// counts: a list that fills the room exactly cannot cross, one entry more may. An exempt
    /// transaction's list never may, whatever its length.
    #[test]
    fn test_an_authorization_list_may_cross_only_past_what_both_limits_leave() {
        let records = |kv| Counted::records(kv, 0, false);
        let growth = |accounts| Counted::NONE.with_growth(accounts, 0);
        // (case, KV limit, state-growth limit, counted before the list, entries, may cross)
        let cases = [
            ("no entries", 3, 5, Counted::NONE, 0, false),
            ("entries fill the KV room", 3, 10, Counted::NONE, 3, false),
            ("one entry past the KV room", 3, 10, Counted::NONE, 4, true),
            ("entries fill the state-growth room", 10, 3, Counted::NONE, 3, false),
            ("one entry past the state-growth room", 10, 3, Counted::NONE, 4, true),
            ("entries fill the KV room left after records", 5, 10, records(2), 3, false),
            ("one entry past the KV room left after records", 5, 10, records(2), 4, true),
            ("entries fill the state-growth room left after growth", 10, 5, growth(2), 3, false),
            ("one entry past the state-growth room left after growth", 10, 5, growth(2), 4, true),
            ("no KV room", 0, 10, Counted::NONE, 1, true),
            ("no state-growth room", 10, 0, Counted::NONE, 1, true),
            ("the longest list under no limit", u64::MAX, u64::MAX, Counted::NONE, u64::MAX, false),
        ];
        for (case, kv, state_growth, before, entries, may_cross) in cases {
            let limits = EvmTxRuntimeLimits::no_limits()
                .with_tx_kv_update_limit(kv)
                .with_tx_state_growth_limit(state_growth);
            let mut limit = AdditionalLimit::new(limits);
            limit.begin_transaction(TxClass::Ordinary, crate::TX_BODY_SIZE);
            limit.count(&before);
            assert_eq!(limit.authorization_list_may_cross(entries), may_cross, "{case}");
        }

        let limits = EvmTxRuntimeLimits::no_limits()
            .with_tx_kv_update_limit(0)
            .with_tx_state_growth_limit(0);
        let mut limit = AdditionalLimit::new(limits);
        limit.begin_transaction(TxClass::ProtocolOwn, crate::TX_BODY_SIZE);
        for entries in [1, u64::MAX] {
            assert!(!limit.authorization_list_may_cross(entries), "exempt, {entries} entries");
        }
    }
}
