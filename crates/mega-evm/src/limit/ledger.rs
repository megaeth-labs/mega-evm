//! The journal's ledger: what the transaction's own execution keeps, counted at the journal's
//! writes and rewound with the journal's checkpoints.
//!
//! The ledger lives in [`MegaJournal`](crate::MegaJournal), beside revm's journal, and keeps one
//! level per checkpoint revm's journal holds open. A revert restores the counts the level saw when
//! it opened, so what the ledger holds is what the journal keeps, whoever reverted what: revm for a
//! frame that fails, Satin for a stop, a tool for a checkpoint of its own.
//!
//! # Frame levels and rollback levels
//!
//! A checkpoint opens one of two kinds of level:
//!
//! - a **frame level**, for each frame revm starts: the first checkpoint revm opens while it
//!   decides a call's start, or the one a creation's account checkpoint returns. Only the engine's
//!   start window ([`open_start`](Ledger::open_start)) opens one. It carries the frame's data-size
//!   budget, the account the frame runs as and whether that account is already recorded;
//! - a **rollback level**, for every other checkpoint: the pre-execution phase's, a system call's,
//!   a precompile's, a tool's. It carries what its revert restores and nothing else.
//!
//! Budgets, the per-frame dedup of a frame's own account and the running frame's check use the
//! nearest frame level. A rollback level may enclose a frame start (a tool's checkpoint opened
//! before a call and closed after it) as long as the two close in order. A revert that would close
//! a level from below — a checkpoint reverted while a level opened after it is still open — is not
//! supported, and trips in debug builds.
//!
//! # What is counted where
//!
//! - the body and the Oracle hints, as the ledger's base, which no revert takes back;
//! - the records of the applied EIP-7702 authorities, inside the pre-execution level;
//! - a frame start's records and its transfer log, inside the start window, at the two writes revm
//!   makes for the start: a call's value transfer, and a creation's account checkpoint. A
//!   creation's creator nonce record is counted before the creation's level opens, so the
//!   creation's failure leaves it with the creator, and the creation's budget and usage are
//!   measured from before it;
//! - a completed write's record, on the running frame's level, fed by the Host's completed-write
//!   hook;
//! - deployed code, on the creation's level before revm commits it.
//!
//! Nothing a tool or a precompile writes through the journal is counted: the journal's own write
//! methods count nothing, and the start window is open only while revm decides a start.
//!
//! The ledger takes every decision: the counts, the frame budgets, the start verdicts, the
//! refunds of the history a caller paid for a start's records, and the check of a caller its child
//! returns into. Satin's side of each frame start ([`StartSide`]) — what its caller paid for its
//! records, the history allowance the frame was granted, a built frame's mark — lives beside it on
//! [`AdditionalLimit`](crate::AdditionalLimit), one side per frame result revm returns.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_primitives::{Address, U256};
use revm::context_interface::{
    host::{CompletedWrite, FrameStart, FrameStartKind},
    journaled_state::JournalCheckpoint,
};

use super::{
    record::{write_effect, write_moves_value, HistoryBytes, RecordEffect, WriteCount},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, TRANSFER_LOG, WRITE_RECORD,
};

/// What the ledger counts that the journal rewinds: additions and refunds, kept apart so that a
/// frame's own usage saturates at zero on its own — a child can take back a slot its parent
/// dirtied — and the log and code bytes history counts beside the write records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// What was counted.
    used: LimitUsage,
    /// What was taken back: a slot written back to its original value.
    refund: LimitUsage,
    /// The log and code bytes among [`used`](Self::used).
    log_and_code_bytes: u64,
}

impl Counts {
    /// What was counted since `base`, an earlier snapshot of the same count, net of what was taken
    /// back since: a frame's usage when `base` is where the frame began.
    const fn usage_since(&self, base: &Self) -> LimitUsage {
        self.used.saturating_sub(base.used).saturating_sub(self.refund.saturating_sub(base.refund))
    }
}

/// A frame start's number in its transaction, in the order the starts were handed to revm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StartId(u64);

/// Who a frame level's frame is: its depth, and the start that opened the level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameIdentity {
    /// The frame's depth.
    pub(crate) depth: usize,
    /// The frame's start.
    pub(crate) start: StartId,
}

/// A frame level's frame: who it is, and what it may keep.
#[derive(Clone, Copy, Debug)]
struct FrameLevel {
    /// The frame's start.
    start: StartId,
    /// The frame's depth.
    depth: usize,
    /// Where the frame's usage is measured from: where its level opened, or, for a creation,
    /// before its creator's nonce record.
    at_base: Counts,
    /// The most data-size bytes the frame may keep.
    budget: u64,
    /// The account the frame runs as.
    account: Option<Address>,
    /// Whether a write to the frame's own account is already counted, so a value transfer or a
    /// creation from the frame does not count it again.
    recorded: bool,
    /// Whether the frame is returning, so revm may close its level.
    returning: bool,
}

impl FrameLevel {
    /// What the frame keeps so far, the children it kept included.
    const fn usage(&self, counts: &Counts) -> LimitUsage {
        counts.usage_since(&self.at_base)
    }

    /// The data-size bytes the frame may still keep.
    const fn remaining(&self, counts: &Counts) -> u64 {
        self.budget.saturating_sub(self.usage(counts).data_size)
    }
}

/// One open checkpoint of revm's journal.
#[derive(Clone, Copy, Debug)]
struct Level {
    /// The checkpoint revm's journal opened it at, which its revert reverts to.
    opened_at: JournalCheckpoint,
    /// The counts its revert restores.
    at_open: Counts,
    /// Whether the nearest frame level below it had its account recorded when it opened, which its
    /// revert restores.
    recorded_at_open: bool,
    /// The start the next frame start would be when it opened: a start at or after it began
    /// inside it.
    next_start_at_open: StartId,
    /// The frame, for a frame level; `None` for a rollback level.
    frame: Option<FrameLevel>,
}

/// What the engine tells the ledger about a frame start it is about to hand revm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StartFacts {
    /// The frame's depth.
    pub(crate) depth: usize,
    /// The account starting the frame.
    pub(crate) caller: Address,
    /// The call's target; `None` for a creation.
    pub(crate) target: Option<Address>,
    /// Whether the start transfers value: a call carrying a non-zero transfer, or a creation with
    /// an endowment.
    pub(crate) transfers_value: bool,
}

/// The write records a frame's start makes, split by whose failure takes them back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameStartRecords {
    /// Records on the frame's own level, which its failure takes back.
    pub(crate) on_level: u64,
    /// Whether the frame's start also writes its caller's account.
    pub(crate) caller: bool,
}

impl FrameStartRecords {
    /// No record at all.
    pub(crate) const NONE: Self = Self { on_level: 0, caller: false };
}

/// The start window, open while revm decides one frame start.
#[derive(Clone, Copy, Debug)]
struct Window {
    facts: StartFacts,
    /// The start the window is open for.
    start: StartId,
    /// Whether the start's frame level opened.
    opened: bool,
    /// Whether the start's write — a call's transfer, a creation's account checkpoint — was seen.
    counted: bool,
}

/// What a frame start counted, as the ledger saw it while revm decided the start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartReport {
    /// The start, once revm decided it in a start window; `None` for a start answered before.
    pub(crate) start: Option<StartId>,
    /// The records counted on the start's own level: a value call's recipient, a creation's
    /// account.
    pub(crate) on_level: u64,
    /// Whether the start counted its caller's record on its own level: a value transfer's sender.
    pub(crate) caller_on_level: bool,
    /// Whether the start counted its creator's nonce record, below its own level, where the
    /// creation's failure leaves it.
    pub(crate) creator_record: bool,
    /// Whether the start counted the transfer log revm journaled for it.
    pub(crate) transfer_log: bool,
    /// Whether revm refused the start's value transfer.
    pub(crate) refused: bool,
    /// The verdict on the start, held to the limits while its level was open; `None` when nothing
    /// was held: a refused transfer, a creation that failed before its level opened, a start revm
    /// never decided.
    pub(crate) verdict: Option<LimitCheck>,
}

/// What became of the last start revm decided: whether its level was kept — committed, and not
/// taken back since — and whether anything it counted is still held, which a revert to before the
/// start takes back, a creator's nonce record included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fate {
    start: StartId,
    level_kept: bool,
    start_kept: bool,
}

/// A point the ledger and revm's journal can be taken back to together
/// ([`MegaJournal::revert_to`](crate::MegaJournal)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    /// Where revm's journal stood.
    pub(crate) position: JournalCheckpoint,
    /// The counts.
    counts: Counts,
    /// Whether the nearest frame level had its account recorded; `None` when there is none.
    recorded: Option<bool>,
    /// The start the next frame start would be: a start at or after it is taken back with the
    /// mark.
    next_start: StartId,
    /// The number of levels open: a revert to the mark finds at most these.
    levels: usize,
    /// For a built frame's own mark, its start, whose creator record the mark keeps.
    own_start: Option<StartId>,
}

/// The facts of a creation's start, taken before anything of it is counted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CreationStart {
    /// The creation's budget, from what its creator had left before its nonce record.
    budget: u64,
    /// Where the creation's usage is measured from: before its creator's nonce record.
    at_base: Counts,
    /// Whether the creator's nonce record was counted.
    creator_record: bool,
    /// The creation's depth.
    depth: usize,
}

/// The ledger. See the module documentation.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ledger {
    counts: Counts,
    /// The body's data-size bytes.
    body: u64,
    /// The Oracle hints' data-size bytes.
    hints: u64,
    /// One level per checkpoint revm's journal holds open, the latest last.
    levels: Vec<Level>,
    /// The start window, while one is open.
    window: Option<Window>,
    /// What the start of the window counted.
    last_start: Option<StartReport>,
    /// What became of the last start revm decided.
    last_decided: Option<Fate>,
    /// The next frame start.
    next_start: StartId,
    /// The transaction's sender, whose account the body counts.
    sender: Address,
    /// Whether the transaction's call target is an applied EIP-7702 authority, whose record the
    /// authorities' records hold.
    target_is_authority: bool,
    /// Whether a value movement journals an EIP-7708 transfer log.
    transfer_logs: bool,
    limits: EvmTxRuntimeLimits,
    /// Whether the transaction is held to no limit.
    exempt: bool,
}

/// `counted`, with the transfer log revm journaled for the value it moved when `logged`.
const fn with_transfer_log(counted: LimitUsage, logged: bool) -> LimitUsage {
    if logged {
        counted.saturating_add(TRANSFER_LOG)
    } else {
        counted
    }
}

impl Ledger {
    /// Holds every transaction from the next one on to `limits`.
    pub(crate) const fn set_limits(&mut self, limits: EvmTxRuntimeLimits) {
        self.limits = limits;
    }

    /// Starts a transaction, held to the limits or to none when `exempt`, whose body weighs `body`
    /// bytes and whose value movements journal transfer logs or not.
    pub(crate) fn begin_transaction(&mut self, exempt: bool, transfer_logs: bool, body: u64) {
        self.counts = Counts::default();
        self.body = body;
        self.hints = 0;
        self.levels.clear();
        self.window = None;
        self.last_start = None;
        self.last_decided = None;
        self.next_start = StartId::default();
        self.sender = Address::ZERO;
        self.target_is_authority = false;
        self.transfer_logs = transfer_logs;
        self.exempt = exempt;
    }

    /* What the ledger holds */

    /// What the transaction keeps so far: its data-size bytes and write records.
    pub(crate) fn usage(&self) -> LimitUsage {
        LimitUsage { data_size: self.body.saturating_add(self.hints), write_records: 0 }
            .saturating_add(self.counts.used)
            .saturating_sub(self.counts.refund)
    }

    /// The history bytes the transaction keeps beside its write records: its logs and the code it
    /// deposited.
    pub(crate) const fn log_and_code_bytes(&self) -> u64 {
        self.counts.log_and_code_bytes
    }

    /// The data-size bytes of the transaction's body, which the transaction counts before any
    /// frame and no revert takes back.
    pub(crate) const fn body(&self) -> u64 {
        self.body
    }

    /// Whether the transaction's body, with one write record for each of its `authorizations`,
    /// fits its data-size limit: what validation holds a transaction to.
    ///
    /// Every entry of the authorization list counts, whether it will apply or not, so the figure is
    /// an upper bound on what the transaction counts before its first frame — the body, then one
    /// record for each distinct authority that applies — and a transaction validation admits
    /// cannot cross the limit there. An exempt transaction is held to nothing.
    pub(crate) fn admits_tx_body(&self, authorizations: u64) -> bool {
        self.exempt ||
            !LimitUsage { data_size: self.body, write_records: 0 }
                .saturating_add(WRITE_RECORD.times(authorizations))
                .exceeds_data_size(self.limits.tx_data_size_limit)
    }

    /// Whether the transaction's call target is an applied EIP-7702 authority, whose record the
    /// authorities' records hold, so the transaction's own frame writes no recipient.
    pub(crate) const fn target_is_authority(&self) -> bool {
        self.target_is_authority
    }

    /// The number of levels open, which is revm's journal depth.
    pub(crate) fn depth(&self) -> usize {
        self.levels.len()
    }

    /// The running frame's level: the nearest frame level.
    fn frame_level(&self) -> Option<&FrameLevel> {
        self.levels.iter().rev().find_map(|level| level.frame.as_ref())
    }

    /// The running frame's level, mutably.
    fn frame_level_mut(&mut self) -> Option<&mut FrameLevel> {
        self.levels.iter_mut().rev().find_map(|level| level.frame.as_mut())
    }

    /// The level of the frame below the running one: the one that started it.
    fn calling_level_mut(&mut self) -> Option<&mut FrameLevel> {
        self.levels.iter_mut().rev().filter_map(|level| level.frame.as_mut()).nth(1)
    }

    /// Who the running frame is: the identity its level was opened with.
    pub(crate) fn running_frame(&self) -> Option<FrameIdentity> {
        self.frame_level().map(|level| FrameIdentity { depth: level.depth, start: level.start })
    }

    /// Whether the transaction's value movements journal EIP-7708 transfer logs, which the ledger
    /// counts where the value moves.
    #[cfg(test)]
    pub(crate) const fn counts_transfer_logs(&self) -> bool {
        self.transfer_logs
    }

    /// The running frame's budget and what it keeps so far.
    #[cfg(test)]
    pub(crate) fn running_budget(&self) -> Option<(u64, LimitUsage)> {
        self.frame_level().map(|level| (level.budget, level.usage(&self.counts)))
    }

    /// What became of `start`, which must be the last start revm decided: whether its level was
    /// kept, and whether what it counted below its level — a creator's nonce record — is still
    /// held.
    pub(crate) fn fate(&self, start: StartId) -> (bool, bool) {
        let fate = self.last_decided.filter(|fate| fate.start == start);
        debug_assert!(fate.is_some(), "a start's fate is read as its frame returns");
        fate.map_or((false, false), |fate| (fate.level_kept, fate.start_kept))
    }

    /* Verdicts */

    /// The verdict on `used` crossing a data-size `limit`, a frame budget when `frame_local`:
    /// within, whatever it crossed, for an exempt transaction.
    const fn crossed(&self, limit: u64, used: u64, frame_local: bool) -> LimitCheck {
        if self.exempt {
            return LimitCheck::WithinLimit;
        }
        LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, limit, used, frame_local }
    }

    /// Holds what the transaction keeps to its limit, then what the running frame keeps to its
    /// budget.
    pub(crate) fn check(&self) -> LimitCheck {
        let (kept, limit) = (self.usage(), self.limits.tx_data_size_limit);
        if kept.exceeds_data_size(limit) {
            return self.crossed(limit, kept.data_size, false);
        }
        if let Some(level) = self.frame_level() {
            let used = level.usage(&self.counts);
            if used.exceeds_data_size(level.budget) {
                return self.crossed(level.budget, used.data_size, true);
            }
        }
        LimitCheck::WithinLimit
    }

    /// The budget of a frame starting now: a share of what the running frame has left, or, for
    /// the transaction's own frame, what the transaction has left, under the frame cap.
    fn child_budget(&self) -> u64 {
        let forwarded = match self.frame_level() {
            Some(parent) => share_of_remaining(parent.remaining(&self.counts)),
            None => self.limits.tx_data_size_limit.saturating_sub(self.usage().data_size),
        };
        forwarded.min(self.limits.frame_data_size_limit)
    }

    /// The records a value call from `from` to `to` at `depth` makes: its recipient, on its own
    /// level, and whether it records its caller's account, which the calling frame has
    /// (`caller_recorded`) or has not recorded yet.
    ///
    /// The transaction's own frame records the recipient unless it is the sender or an applied
    /// authority. Any other records the recipient unless it is the caller or the sender, and the
    /// caller's own account once per frame; a `CALLCODE` or a call to itself records the caller
    /// although revm moves nothing.
    fn call_records(
        &self,
        depth: usize,
        from: Address,
        to: Address,
        caller_recorded: bool,
    ) -> (u64, bool) {
        if depth == 0 {
            return (u64::from(to != from && !self.target_is_authority), false);
        }
        (u64::from(to != from && to != self.sender), !caller_recorded)
    }

    /// The write records the start `start` describes makes, for the opcode about to start it,
    /// which pays for them once it took the gas it forwards: the frame's own budget carries none
    /// of them, which is what lets a value transfer's allowance pay for what the recipient does
    /// with it.
    ///
    /// A call that moves no value writes nothing. A value call writes its recipient, unless that
    /// is the caller itself or the transaction's sender, whose account the body counts. A creation
    /// writes the account it creates. Either writes the caller's account — a transfer's sender, a
    /// creation's creator nonce — unless the running frame already recorded it. They are the
    /// records the start is counted by once revm makes it ([`on_transfer`](Self::on_transfer),
    /// [`before_creation`](Self::before_creation)); a debug build holds the two answers to each
    /// other where the frame starts.
    pub(crate) fn start_records(&self, start: &FrameStart) -> FrameStartRecords {
        let recorded = self.frame_level().is_some_and(|level| level.recorded);
        let target = match start.kind {
            FrameStartKind::Create | FrameStartKind::Create2 => {
                return FrameStartRecords { on_level: 1, caller: !recorded };
            }
            FrameStartKind::Call |
            FrameStartKind::CallCode |
            FrameStartKind::DelegateCall |
            FrameStartKind::StaticCall => start.target,
        };
        match target {
            Some(target) if !start.value.is_zero() => {
                // An opcode runs inside a frame, so the frame it starts is below the
                // transaction's own.
                let depth = self.frame_level().map_or(1, |level| level.depth + 1);
                let (on_level, caller) = self.call_records(depth, start.caller, target, recorded);
                FrameStartRecords { on_level, caller }
            }
            _ => FrameStartRecords::NONE,
        }
    }

    /// The verdict on a start revm is not asked to decide — a call gas detention answers without
    /// running the precompile it targets — had revm made it: the records it would count and the
    /// transfer log of the value it would move, held to the transaction's limit and to the budget
    /// its frame would get. Nothing is counted.
    pub(crate) fn hypothetical_start(&self, facts: &StartFacts) -> LimitCheck {
        let Some(target) = facts.target else {
            unreachable!("only a call is answered without revm deciding its start")
        };
        let counted = if facts.transfers_value {
            let recorded = self.frame_level().is_some_and(|level| level.recorded);
            let (on_level, caller) = self.call_records(facts.depth, facts.caller, target, recorded);
            with_transfer_log(
                WRITE_RECORD.times(on_level + u64::from(caller)),
                self.transfer_logs && target != facts.caller,
            )
        } else {
            LimitUsage::ZERO
        };
        let (kept, limit) = (self.usage().saturating_add(counted), self.limits.tx_data_size_limit);
        if kept.exceeds_data_size(limit) {
            return self.crossed(limit, kept.data_size, false);
        }
        let budget = self.child_budget();
        if counted.exceeds_data_size(budget) {
            return self.crossed(budget, counted.data_size, true);
        }
        LimitCheck::WithinLimit
    }

    /* What the transaction counts outside the journal's writes */

    /// Counts the records of the applied EIP-7702 authorities, `authorities` distinct accounts
    /// other than the transaction's `sender`, `target_is_authority` if the call target is one.
    /// Inside the pre-execution level, whose revert takes them back.
    ///
    /// The records cannot cross the data-size limit: validation admitted the body with a record for
    /// every entry of the authorization list ([`admits_tx_body`](Self::admits_tx_body)), and only
    /// an entry can apply. A debug build asserts it.
    pub(crate) fn count_authorities(
        &mut self,
        sender: Address,
        authorities: u64,
        target_is_authority: bool,
    ) {
        self.sender = sender;
        self.target_is_authority = target_is_authority;
        self.counts.used = self.counts.used.saturating_add(WRITE_RECORD.times(authorities));
        debug_assert!(
            self.exempt || !self.usage().exceeds_data_size(self.limits.tx_data_size_limit),
            "validation admits no body whose authority records cross the data-size limit"
        );
    }

    /// Admits an Oracle hint of `bytes`: held to the transaction's limit before it is counted, and
    /// never taken back once counted.
    pub(crate) fn admit_hint(&mut self, bytes: u64) -> LimitCheck {
        let hint = LimitUsage { data_size: bytes, write_records: 0 };
        let (reached, limit) = (self.usage().saturating_add(hint), self.limits.tx_data_size_limit);
        if reached.exceeds_data_size(limit) {
            let stop = self.crossed(limit, reached.data_size, false);
            if stop.exceeded_limit() {
                return stop;
            }
        }
        self.hints = self.hints.saturating_add(bytes);
        LimitCheck::WithinLimit
    }

    /// What `write`, which the running opcode completed, keeps, and the history bytes it appends or
    /// takes back: what [`count_write`](Self::count_write) counts, and what the Host's
    /// completed-write hook charges.
    ///
    /// A `SELFDESTRUCT` that moved value to another account also keeps the EIP-7708 transfer log
    /// revm journaled for the move, with the beneficiary's record. The log is the running frame's,
    /// as the move is: the frame's own journal checkpoint takes both back when the frame fails. It
    /// is data size and nothing else — no write record, and no history: the bytes reported are the
    /// record's alone.
    pub(crate) fn write_count(&self, write: &CompletedWrite<'_>) -> (WriteCount, HistoryBytes) {
        let effect = write_effect(write, self.sender);
        let history = effect.history_bytes();
        let added = match effect {
            RecordEffect::None => LimitUsage::ZERO,
            RecordEffect::Record(usage) => usage,
            RecordEffect::Refund(usage) => {
                return (WriteCount { taken: usage, ..WriteCount::NONE }, history);
            }
        };
        // A log's bytes are history beside the write records; a record's are counted as one.
        let log_bytes = match write {
            CompletedWrite::Log { .. } => added.data_size,
            CompletedWrite::Sstore { .. } | CompletedWrite::SelfDestruct { .. } => 0,
        };
        let added = with_transfer_log(added, self.transfer_logs && write_moves_value(write));
        (WriteCount { added, log_bytes, ..WriteCount::NONE }, history)
    }

    /// Counts what a completed write keeps on the running frame's level: what it adds, or what it
    /// takes back. The verdict is the check after an addition; a record taken back holds nothing.
    pub(crate) fn count_write(&mut self, count: &WriteCount) -> LimitCheck {
        if count.taken != LimitUsage::ZERO {
            self.counts.refund = self.counts.refund.saturating_add(count.taken);
            return LimitCheck::WithinLimit;
        }
        self.counts.log_and_code_bytes =
            self.counts.log_and_code_bytes.saturating_add(count.log_bytes);
        if count.added == LimitUsage::ZERO {
            return LimitCheck::WithinLimit;
        }
        self.counts.used = self.counts.used.saturating_add(count.added);
        self.check()
    }

    /// Counts `bytes` of code a creation deposits, on the creation's level before revm commits it.
    pub(crate) fn count_code(&mut self, bytes: u64) -> LimitCheck {
        if bytes == 0 {
            return LimitCheck::WithinLimit;
        }
        let code = LimitUsage { data_size: bytes, write_records: 0 };
        self.counts.used = self.counts.used.saturating_add(code);
        self.counts.log_and_code_bytes = self.counts.log_and_code_bytes.saturating_add(bytes);
        self.check()
    }

    /// Runs the running frame — a `keylessDeploy` call's — as `creator`, whose nonce the creation
    /// it starts writes: recorded already when the creator is the transaction's sender.
    pub(crate) fn set_frame_creator(&mut self, creator: Address) {
        let sender = self.sender;
        if let Some(level) = self.frame_level_mut() {
            level.account = Some(creator);
            level.recorded = creator == sender;
        }
    }

    /// Takes back the nonce record a creation left the running frame, a `keylessDeploy` call's,
    /// when its nonce bump is taken back. `false` when there was none to take.
    pub(crate) fn take_back_creator_record(&mut self) -> bool {
        let sender = self.sender;
        let Some(level) = self.frame_level_mut() else { return false };
        if level.account == Some(sender) || !level.recorded {
            return false;
        }
        level.recorded = false;
        self.counts.used = self.counts.used.saturating_sub(WRITE_RECORD);
        true
    }

    /* The start window */

    /// Opens the start window for the start `facts` describes, which revm decides next.
    pub(crate) fn open_start(&mut self, facts: StartFacts) {
        debug_assert!(self.window.is_none(), "start windows do not nest");
        if facts.depth == 0 {
            self.sender = facts.caller;
        }
        let start = self.next_start;
        self.next_start = StartId(start.0 + 1);
        self.last_start = Some(StartReport { start: Some(start), ..Default::default() });
        self.window = Some(Window { facts, start, opened: false, counted: false });
    }

    /// Closes the start window once revm decided the start, and reports what the start counted. A
    /// start whose level never opened is decided here.
    pub(crate) fn close_start(&mut self) -> StartReport {
        if let Some(window) = self.window.take().filter(|window| !window.opened) {
            self.last_decided =
                Some(Fate { start: window.start, level_kept: false, start_kept: true });
        }
        self.last_start.take().unwrap_or_default()
    }

    /// The start report of the open window.
    fn report_mut(&mut self) -> &mut StartReport {
        self.last_start.get_or_insert_with(StartReport::default)
    }

    /* Levels */

    /// Opens a level at `opened_at`, a frame level when `frame` is given.
    fn open_level(&mut self, opened_at: JournalCheckpoint, frame: Option<FrameLevel>) {
        let recorded_at_open = self.frame_level().is_some_and(|level| level.recorded);
        self.levels.push(Level {
            opened_at,
            at_open: self.counts,
            recorded_at_open,
            next_start_at_open: self.next_start,
            frame,
        });
    }

    /// Opens the frame level of the window's start at `opened_at`, its usage measured from
    /// `at_base` under `budget`, running as `account`, recorded or not: the frame the window's
    /// start builds.
    fn open_frame_level(
        &mut self,
        opened_at: JournalCheckpoint,
        at_base: Counts,
        budget: u64,
        account: Address,
        recorded: bool,
    ) {
        let Some(window) = &mut self.window else {
            unreachable!("a frame level opens inside its start's window")
        };
        window.opened = true;
        let (start, depth) = (window.start, window.facts.depth);
        let frame = FrameLevel {
            start,
            depth,
            at_base,
            budget,
            account: Some(account),
            recorded,
            returning: false,
        };
        self.open_level(opened_at, Some(frame));
    }

    /// revm's journal opened a checkpoint at `opened_at`: the frame level of a call whose start the
    /// window is open for, when it is the first checkpoint of the window; a rollback level
    /// otherwise.
    pub(crate) fn on_checkpoint(&mut self, opened_at: JournalCheckpoint) {
        let Some(window) = self.window.filter(|window| !window.opened) else {
            return self.open_level(opened_at, None);
        };
        let StartFacts { depth, caller, target, transfers_value } = window.facts;
        let Some(target) = target else {
            // A creation's frame level is the one its account checkpoint opens.
            return self.open_level(opened_at, None);
        };
        // The one start rule that stays an interpretation of the input: whether the frame's own
        // account is recorded before anything of the frame is counted.
        let recorded = if depth == 0 {
            target == caller || self.target_is_authority || transfers_value
        } else {
            let inherited = self
                .frame_level()
                .is_some_and(|parent| parent.account == Some(target) && parent.recorded);
            inherited || target == self.sender || transfers_value
        };
        self.open_frame_level(opened_at, self.counts, self.child_budget(), target, recorded);
        let verdict = self.check();
        self.report_mut().verdict = Some(verdict);
    }

    /// revm moved `value` from `from` to `to` for the call whose start the window is open for
    /// (`refused` when it could not), and journaled a transfer log or not (`logged`): counts the
    /// records the transfer makes on the call's level ([`call_records`](Self::call_records)), then
    /// holds the start to the limits.
    pub(crate) fn on_transfer(
        &mut self,
        from: Address,
        to: Address,
        value: U256,
        refused: bool,
        logged: bool,
    ) {
        let Some(window) = &mut self.window else { return };
        if window.counted || !window.opened || window.facts.target.is_none() {
            return;
        }
        window.counted = true;
        let depth = window.facts.depth;
        if refused {
            let report = self.report_mut();
            report.refused = true;
            report.verdict = None;
            return;
        }
        if value.is_zero() {
            return;
        }
        let caller_recorded = self.calling_level_mut().is_none_or(|caller| caller.recorded);
        let (on_level, caller_on_level) = self.call_records(depth, from, to, caller_recorded);
        if caller_on_level {
            if let Some(caller) = self.calling_level_mut() {
                caller.recorded = true;
            }
        }
        let counted = WRITE_RECORD.times(on_level + u64::from(caller_on_level));
        self.counts.used = self.counts.used.saturating_add(with_transfer_log(counted, logged));
        let verdict = self.check();
        let report = self.report_mut();
        report.on_level = on_level;
        report.caller_on_level = caller_on_level;
        report.transfer_log = logged;
        report.verdict = Some(verdict);
    }

    /// revm is about to open a creation's account checkpoint for the start of the window: takes the
    /// creation's budget and base, then counts the creator's nonce record, before anything of the
    /// creation. `None` outside a creation's start window.
    pub(crate) fn before_creation(&mut self) -> Option<CreationStart> {
        let window = self.window.as_mut()?;
        if window.counted || window.facts.target.is_some() {
            return None;
        }
        window.counted = true;
        let depth = window.facts.depth;
        let (budget, at_base) = (self.child_budget(), self.counts);
        let mut creator_record = false;
        if depth > 0 {
            if let Some(level) = self.frame_level_mut() {
                creator_record = !level.recorded;
                level.recorded = true;
            }
        }
        if creator_record {
            self.counts.used = self.counts.used.saturating_add(WRITE_RECORD);
        }
        Some(CreationStart { budget, at_base, creator_record, depth })
    }

    /// revm opened (`Some`) or failed to open (`None`) the account checkpoint of a creation at
    /// `address`, and journaled a transfer log or not (`logged`): opens the creation's frame level,
    /// counts the created account and the log on it, then holds the start to the limits. The
    /// creation's usage includes its creator's nonce record; its revert keeps it.
    ///
    /// A checkpoint opened outside a creation's start window opens a rollback level.
    pub(crate) fn after_creation(
        &mut self,
        start: Option<CreationStart>,
        address: Address,
        opened: Option<JournalCheckpoint>,
        logged: bool,
    ) {
        let Some(start) = start else {
            if let Some(opened_at) = opened {
                self.open_level(opened_at, None);
            }
            return;
        };
        self.report_mut().creator_record = start.creator_record;
        let Some(opened_at) = opened else { return };
        debug_assert!(start.depth == self.window.map_or(start.depth, |window| window.facts.depth));
        self.open_frame_level(opened_at, start.at_base, start.budget, address, true);
        self.counts.used = self.counts.used.saturating_add(with_transfer_log(WRITE_RECORD, logged));
        let verdict = self.check();
        let report = self.report_mut();
        report.on_level = 1;
        report.transfer_log = logged;
        report.verdict = Some(verdict);
    }

    /// The running frame is returning: revm closes its level next, committing or reverting it.
    pub(crate) fn frame_returning(&mut self) {
        if let Some(level) = self.frame_level_mut() {
            level.returning = true;
        }
    }

    /// Asserts a frame level closes as revm closes one: while its start is being decided, or once
    /// its frame is returning. A frame level any other checkpoint closes — a tool's checkpoint
    /// reverted while a frame opened after it still runs — would leave the frame running without
    /// its level.
    fn assert_frame_closes(&self, frame: &FrameLevel) {
        debug_assert!(
            self.window.is_some() || frame.returning,
            "a revert closes the latest level: a frame's level closes only when the frame returns"
        );
    }

    /// revm's journal committed its latest checkpoint: the level closes, and a frame level hands
    /// its frame's flag to a caller running as the same account.
    pub(crate) fn on_commit(&mut self) {
        let Some(level) = self.levels.pop() else { return };
        let Some(frame) = level.frame else { return };
        self.assert_frame_closes(&frame);
        if let Some(caller) = self.frame_level_mut() {
            if frame.account.is_some() && frame.account == caller.account {
                caller.recorded |= frame.recorded;
            }
        }
        self.last_decided = Some(Fate { start: frame.start, level_kept: true, start_kept: true });
    }

    /// revm's journal reverted to `checkpoint`: the latest level, which must be the one opened
    /// there, closes, and the counts and the flag of the nearest frame level below it are what they
    /// were when it opened.
    ///
    /// A frame level's revert keeps what its start counted below it, a creator's nonce record. A
    /// rollback level a start began inside — a tool's checkpoint around a call — takes everything
    /// the start counted back with it.
    pub(crate) fn on_revert(&mut self, checkpoint: JournalCheckpoint) {
        let Some(level) = self.levels.pop() else { return };
        debug_assert!(
            level.opened_at == checkpoint,
            "a revert closes the latest level: a revert below a level still open is not supported"
        );
        self.counts = level.at_open;
        if let Some(caller) = self.frame_level_mut() {
            caller.recorded = level.recorded_at_open;
        }
        match level.frame {
            Some(frame) => {
                self.assert_frame_closes(&frame);
                self.last_decided =
                    Some(Fate { start: frame.start, level_kept: false, start_kept: true });
            }
            None => self.take_back_starts_from(level.next_start_at_open, None),
        }
    }

    /// Notes that the last start decided is taken back when it is `start` or later: its level is no
    /// longer kept, and, unless it is `own_start`, nothing it counted is.
    fn take_back_starts_from(&mut self, start: StartId, own_start: Option<StartId>) {
        if let Some(fate) = &mut self.last_decided {
            if fate.start >= start {
                fate.level_kept = false;
                fate.start_kept &= own_start == Some(fate.start);
            }
        }
    }

    /// revm's journal ended the transaction: every level closes. What was counted stays readable
    /// until the next transaction begins.
    pub(crate) fn on_transaction_end(&mut self) {
        self.levels.clear();
        self.window = None;
    }

    /* Marks */

    /// A mark at `position`, where revm's journal stands now.
    pub(crate) fn mark(&self, position: JournalCheckpoint) -> Mark {
        Mark {
            position,
            counts: self.counts,
            recorded: self.frame_level().map(|level| level.recorded),
            next_start: self.next_start,
            levels: self.levels.len(),
            own_start: None,
        }
    }

    /// The mark a frame revm just built is taken back to when it is rewritten into a failure: its
    /// own checkpoint, the counts its level opened with — after a creation's creator record — and
    /// whether the frame below had its account recorded then. Never where the frame's usage is
    /// measured from, which for a creation is before the creator's record.
    pub(crate) fn frame_start_mark(&self) -> Option<Mark> {
        let level = self.levels.last()?;
        let frame = level.frame?;
        let below = self.levels.len() - 1;
        let caller = self.levels[..below].iter().rev().any(|level| level.frame.is_some());
        Some(Mark {
            position: level.opened_at,
            counts: level.at_open,
            recorded: caller.then_some(level.recorded_at_open),
            next_start: frame.start,
            levels: below,
            own_start: Some(frame.start),
        })
    }

    /// Takes the ledger back to `mark`, which revm's journal is being reverted to: the counts, the
    /// running frame's flag, and what became of the last start decided.
    pub(crate) fn revert_to(&mut self, mark: &Mark) {
        debug_assert!(
            self.levels.len() <= mark.levels,
            "a revert to a mark finds every level opened since closed"
        );
        self.counts = mark.counts;
        if let (Some(recorded), Some(level)) = (mark.recorded, self.frame_level_mut()) {
            level.recorded = recorded;
        }
        self.take_back_starts_from(mark.next_start, mark.own_start);
    }
}

/// Satin's side of one frame start: what its caller paid for the records it makes, what the
/// ledger saw it count, the history allowance the frame was granted, and, for a frame revm built,
/// the mark it is taken back to when it is rewritten into a failure. One per frame result revm
/// will return.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StartSide {
    /// The frame's depth.
    depth: usize,
    /// What the caller paid for the records on the frame's own level.
    on_level_charge: u64,
    /// What the caller paid for the record of its own account.
    caller_charge: u64,
    /// What the ledger saw the start count; the default for a start answered before revm decided
    /// it, which counted nothing.
    report: StartReport,
    /// The mark of a frame revm built.
    mark: Option<Mark>,
    /// What is left of the history allowance the frame was granted, if it was granted one. It
    /// never enters the frame's gas, and what it does not spend disappears with the frame.
    stipend: u64,
    /// The records the charge was computed from, when the charge was made from one and nothing
    /// could rewrite the start between the two: what a debug build holds the ledger's count to.
    #[cfg(debug_assertions)]
    charged: Option<FrameStartRecords>,
}

impl StartSide {
    /// The side of a start at `depth` whose caller paid `on_level_charge` and `caller_charge` for
    /// the records `charged`, when the charge was computed from them.
    pub(crate) const fn new(
        depth: usize,
        on_level_charge: u64,
        caller_charge: u64,
        charged: Option<FrameStartRecords>,
    ) -> Self {
        #[cfg(not(debug_assertions))]
        let _ = charged;
        Self {
            depth,
            on_level_charge,
            caller_charge,
            report: StartReport {
                start: None,
                on_level: 0,
                caller_on_level: false,
                creator_record: false,
                transfer_log: false,
                refused: false,
                verdict: None,
            },
            mark: None,
            stipend: 0,
            #[cfg(debug_assertions)]
            charged,
        }
    }

    /// Attaches what the ledger saw the start count and the built frame's mark, and holds the
    /// count to the records the start's charge was computed from.
    ///
    /// The two are separate answers to one question, asked at two points: the charge at the
    /// opcode, on the facts revm's instruction hands the frame-start hook — or before execution,
    /// on the transaction, for its own frame; or at a `keylessDeploy` call's first run, on the
    /// creation it starts — and the count, on the writes revm made for the start. They agree
    /// because the start revm makes carries the caller, target and value the charge was computed
    /// from, and nothing between the two changes it: an interceptor answers a frame or lets it
    /// start as it is, and a creation reaches none. A divergence trips in a debug build rather
    /// than mis-charging the caller and mis-splitting the refund its failure gets back. An
    /// inspector that may rewrite the start runs between the two, and can split them: under one
    /// the start is not checked. Only a start revm made and held to the limits is checked: one it
    /// refused, or failed before its level opened, counted nothing, and the charge comes back.
    pub(crate) fn attach(&mut self, report: StartReport, mark: Option<Mark>) {
        self.report = report;
        self.mark = mark;
        #[cfg(debug_assertions)]
        if let Some(charged) =
            self.charged.filter(|_| report.start.is_some() && report.verdict.is_some())
        {
            let counted = FrameStartRecords {
                on_level: report.on_level,
                caller: report.caller_on_level || report.creator_record,
            };
            debug_assert_eq!(
                charged, counted,
                "the records a frame's start makes changed after its caller was charged for them",
            );
        }
    }

    /// The frame's depth.
    pub(crate) const fn depth(&self) -> usize {
        self.depth
    }

    /// Grants the frame a history allowance of `amount`.
    pub(crate) const fn grant_stipend(&mut self, amount: u64) {
        self.stipend = amount;
    }

    /// Draws up to `amount` from the frame's history allowance and reports what it gave; the frame
    /// pays the rest out of its gas.
    pub(crate) const fn consume_stipend(&mut self, amount: u64) -> u64 {
        let drawn = if self.stipend < amount { self.stipend } else { amount };
        self.stipend -= drawn;
        drawn
    }

    /// The start, once revm decided it.
    pub(crate) const fn start(&self) -> Option<StartId> {
        self.report.start
    }

    /// The mark of a frame revm built.
    pub(crate) const fn mark(&self) -> Option<&Mark> {
        self.mark.as_ref()
    }

    /// The history gas the caller gets back once the frame returned: what it paid less the price
    /// of the start's records `ledger` holds.
    ///
    /// The records on the start's own level are held when that level was kept — committed, and not
    /// taken back since; a creator's nonce record, counted below the creation's level, unless a
    /// revert to before the start took it back. So a start answered before revm decided it, or
    /// refused before its writes, gets everything back; a call answered with a success keeps its
    /// records; a failed precompile, and a frame that fails, give theirs back, a creator's
    /// surviving record aside.
    pub(crate) fn refund(&self, ledger: &Ledger) -> u64 {
        let (level_kept, start_kept) =
            self.report.start.map_or((false, false), |start| ledger.fate(start));
        let on_level = level_kept && self.report.on_level > 0;
        let caller = (level_kept && self.report.caller_on_level) ||
            (start_kept && self.report.creator_record);
        let mut refund = 0;
        if !on_level {
            refund += self.on_level_charge;
        }
        if !caller {
            refund += self.caller_charge;
        }
        refund
    }
}

/// `remaining` × [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`], rounded down.
///
/// The share is `N / D = 1 − 1 / K` with `K = D / (D − N)` a whole number, so the floor of the
/// product is `remaining − ⌈remaining / K⌉`, which stays in `u64` and never wraps. A `u128`
/// product would call the 128-bit division routine on every frame start, where a division of a
/// `u64` by a constant compiles to a multiplication.
const fn share_of_remaining(remaining: u64) -> u64 {
    const N: u64 = FRAME_DATA_SHARE_NUMERATOR;
    const D: u64 = FRAME_DATA_SHARE_DENOMINATOR;
    const _: () = assert!(N < D && D.is_multiple_of(D - N));
    const K: u64 = D / (D - N);
    remaining - remaining.div_ceil(K)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AUTHORIZATION_SIZE, TRANSFER_LOG_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE};
    use alloy_primitives::address;
    use revm::context_interface::context::SelfDestructResult;

    const SENDER: Address = address!("00000000000000000000000000000000000f0001");
    const CALLEE: Address = address!("00000000000000000000000000000000000f0002");
    const TARGET: Address = address!("00000000000000000000000000000000000f0003");

    /// A ledger holding a transaction with a body of `body` bytes under `limits`, exempt or not,
    /// whose value movements journal transfer logs when `transfer_logs`. The sender is whoever
    /// starts the transaction's own frame.
    fn ledger(limits: EvmTxRuntimeLimits, exempt: bool, transfer_logs: bool, body: u64) -> Ledger {
        let mut ledger = Ledger::default();
        ledger.set_limits(limits);
        ledger.begin_transaction(exempt, transfer_logs, body);
        ledger
    }

    /// [`ledger`], with [`SENDER`] as the transaction's sender before any frame starts.
    fn ledger_from_sender(limits: EvmTxRuntimeLimits, transfer_logs: bool) -> Ledger {
        let mut ledger = ledger(limits, false, transfer_logs, 0);
        ledger.count_authorities(SENDER, 0, false);
        ledger
    }

    /// Limits every dimension holds at zero: anything a transaction counts crosses one.
    fn zero_limits() -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(0).with_frame_data_size_limit(0)
    }

    /// A checkpoint of revm's journal as the ledger sees one: distinct per open level.
    fn checkpoint(ledger: &Ledger) -> JournalCheckpoint {
        JournalCheckpoint { log_i: 0, journal_i: ledger.depth(), selfdestructed_i: 0 }
    }

    /// Starts a call from `caller` to `target` at `depth` as revm starts one, moving `value`, and
    /// journaling a transfer log for it when the ledger's transaction does; reports what the start
    /// counted.
    fn start_call(
        ledger: &mut Ledger,
        depth: usize,
        caller: Address,
        target: Address,
        value: u64,
    ) -> StartReport {
        let facts = StartFacts { depth, caller, target: Some(target), transfers_value: value != 0 };
        ledger.open_start(facts);
        ledger.on_checkpoint(checkpoint(ledger));
        let logged = ledger.transfer_logs && value != 0 && target != caller;
        ledger.on_transfer(caller, target, U256::from(value), false, logged);
        ledger.close_start()
    }

    /// Returns from the running frame as revm does: its level closes, committed or reverted.
    fn end_frame(ledger: &mut Ledger, failed: bool) {
        ledger.frame_returning();
        let opened_at = ledger.levels.last().expect("a frame is running").opened_at;
        if failed {
            ledger.on_revert(opened_at);
        } else {
            ledger.on_commit();
        }
    }

    /// The stop of a data-size crossing.
    const fn stop(limit: u64, used: u64, frame_local: bool) -> LimitCheck {
        LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, limit, used, frame_local }
    }

    #[test]
    fn test_share_of_remaining_is_the_floor_of_the_exact_product() {
        let exact = |x: u64| {
            (u128::from(x) * u128::from(FRAME_DATA_SHARE_NUMERATOR) /
                u128::from(FRAME_DATA_SHARE_DENOMINATOR)) as u64
        };
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        let edges = [0, 1, 49, 50, 51, 99, 100, 101, u64::MAX, u64::MAX - 1, u64::MAX / 100 * 100];
        for v in edges.into_iter().chain((0..100_000).map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x >> (x % 64)
        })) {
            assert_eq!(share_of_remaining(v), exact(v), "{v}");
        }
    }

    /// A hint is held to the transaction's data-size limit before it is counted: one that fits is
    /// counted, one that would cross is the stop at the usage it would have reached and leaves the
    /// count as it was, and a later one that fits is counted, the crossing having stopped nothing
    /// but its own call. An exempt transaction's is counted whatever it crosses.
    #[test]
    fn test_a_hint_that_would_cross_the_limit_is_not_counted() {
        let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(100);
        let mut held = ledger(limits, false, false, 40);
        assert_eq!(held.admit_hint(59), LimitCheck::WithinLimit);
        assert_eq!(held.usage().data_size, 99);
        assert_eq!(held.admit_hint(2), stop(100, 101, false), "two more bytes cross");
        assert_eq!(held.usage().data_size, 99, "the hint that crossed is not counted");
        assert_eq!(held.admit_hint(1), LimitCheck::WithinLimit, "one more fits exactly");
        assert_eq!(held.usage().data_size, 100, "and is counted");

        let mut exempt = ledger(limits, true, false, 40);
        assert_eq!(exempt.admit_hint(61), LimitCheck::WithinLimit);
        assert_eq!(exempt.usage().data_size, 101, "an exempt transaction's hint is counted");
    }

    /// A child frame's budget is 98% of what its parent has left, three frames down, and the
    /// transaction's own frame gets what the transaction has left.
    #[test]
    fn test_a_child_frame_gets_98_percent_of_what_its_parent_has_left() {
        let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(10_000);
        let mut ledger = ledger(limits, false, false, 310);
        let budget = |ledger: &Ledger| ledger.running_budget().expect("a frame is running").0;

        start_call(&mut ledger, 0, SENDER, CALLEE, 0);
        let first = 10_000 - 310;
        assert_eq!(budget(&ledger), first);

        start_call(&mut ledger, 1, CALLEE, TARGET, 0);
        let child = share_of_remaining(first);
        assert_eq!(child, 9_496);
        assert_eq!(budget(&ledger), child);

        start_call(&mut ledger, 2, TARGET, SENDER, 0);
        let grandchild = share_of_remaining(child);
        assert_eq!(budget(&ledger), grandchild);

        start_call(&mut ledger, 3, SENDER, CALLEE, 0);
        assert_eq!(
            budget(&ledger),
            share_of_remaining(grandchild),
            "the fourth frame, at depth 3, still takes 98% of what is left"
        );
    }

    /// The frame cap binds when it is tighter than the share of what the parent has left.
    #[test]
    fn test_the_frame_cap_binds_when_it_is_tighter_than_the_share() {
        let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(100);
        let mut ledger = ledger(limits, false, false, 0);
        start_call(&mut ledger, 0, SENDER, CALLEE, 0);
        assert_eq!(ledger.running_budget().map(|(budget, _)| budget), Some(100));
        start_call(&mut ledger, 1, CALLEE, TARGET, 0);
        assert_eq!(ledger.running_budget().map(|(budget, _)| budget), Some(98));
    }

    /// A transaction that is not exempt is stopped wherever it crosses a limit: the first check, at
    /// its frame's start, is the stop, and a later start that crosses again is a stop of its own,
    /// with the usage it reached, not the first one's. An exempt transaction is stopped by no
    /// limit, at the transaction or at a frame, and what it uses is counted all the same.
    #[test]
    fn test_a_transaction_is_stopped_where_it_crosses_unless_exempt() {
        for exempt in [false, true] {
            let mut ledger = ledger(zero_limits(), exempt, false, TX_BODY_SIZE);
            let first = start_call(&mut ledger, 0, SENDER, CALLEE, 0);
            let second = start_call(&mut ledger, 1, CALLEE, TARGET, 1);
            let verdicts = (first.verdict, second.verdict, ledger.check());
            let expected = if exempt {
                (
                    Some(LimitCheck::WithinLimit),
                    Some(LimitCheck::WithinLimit),
                    LimitCheck::WithinLimit,
                )
            } else {
                let records = TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE;
                (
                    Some(stop(0, TX_BODY_SIZE, false)),
                    Some(stop(0, records, false)),
                    stop(0, records, false),
                )
            };
            assert_eq!(verdicts, expected, "exempt {exempt}");
            assert_eq!(
                ledger.usage(),
                LimitUsage { data_size: TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE, write_records: 2 },
                "exempt {exempt}: counted all the same"
            );
        }
    }

    /// Validation admits no body whose authority records cross the data-size limit, so a debug
    /// build trips on records that do: under a limit lowered to the body and one record, two
    /// applied authorities cross it.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "validation admits no body whose authority records cross")]
    fn test_authority_records_that_cross_the_limit_trip() {
        let limits = EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(TX_BODY_SIZE + WRITE_RECORD_SIZE);
        let mut ledger = ledger(limits, false, false, TX_BODY_SIZE);
        ledger.count_authorities(SENDER, 2, false);
    }

    /// Applied authorities are recorded for an exempt transaction, whatever they cross.
    #[test]
    fn test_an_exempt_transaction_records_its_authorities() {
        let mut ledger = ledger(zero_limits(), true, false, 0);
        ledger.count_authorities(SENDER, 2, false);
        assert_eq!(ledger.usage(), WRITE_RECORD.times(2));
    }

    /// A transfer log is data size and nothing else: no write record, and no history byte beside
    /// the records — the transaction's history is its body and its records, whatever it moved.
    #[test]
    fn test_a_transfer_log_is_data_size_and_never_history() {
        let mut ledger = ledger(EvmTxRuntimeLimits::no_limits(), false, true, TX_BODY_SIZE);
        start_call(&mut ledger, 0, SENDER, CALLEE, 1);
        start_call(&mut ledger, 1, CALLEE, TARGET, 1);
        assert_eq!(
            ledger.usage(),
            LimitUsage {
                data_size: TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE + 2 * TRANSFER_LOG_SIZE,
                write_records: 2,
            },
            "the recipient of each move — the inner one's caller is the outer one's — and two logs",
        );
        assert_eq!(ledger.log_and_code_bytes(), 0, "no history byte beside the records");
    }

    /// A frame that fails takes its transfer log back with its records, as revm's checkpoint
    /// takes back the move and the log.
    #[test]
    fn test_a_failed_frame_gives_its_transfer_log_back() {
        let mut ledger = ledger(EvmTxRuntimeLimits::no_limits(), false, true, 0);
        start_call(&mut ledger, 0, SENDER, CALLEE, 0);
        start_call(&mut ledger, 1, CALLEE, TARGET, 1);
        assert_eq!(ledger.usage().data_size, 2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE);
        end_frame(&mut ledger, true);
        assert_eq!(ledger.usage(), LimitUsage::ZERO);
    }

    /// A frame start whose transfer log crosses a limit is stopped at its start, with a stop that
    /// names the limit it crossed, its own budget or the transaction's limit. The records alone
    /// fit, so it is the log that crosses; at exactly the limit it holds.
    #[test]
    fn test_a_frame_start_whose_transfer_log_crosses_is_stopped() {
        let records = 2 * WRITE_RECORD_SIZE;
        let needs = records + TRANSFER_LOG_SIZE;
        // The smallest frame cap whose share, the budget of a child of the first frame, holds it.
        let cap = (needs..).find(|cap| share_of_remaining(*cap) >= needs).unwrap();
        assert_eq!(share_of_remaining(cap), needs);
        for (cap, fits) in [(cap, true), (cap - 1, false)] {
            let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(cap);
            let mut ledger = ledger(limits, false, true, 0);
            start_call(&mut ledger, 0, SENDER, CALLEE, 0);
            let report = start_call(&mut ledger, 1, CALLEE, TARGET, 1);
            let expected =
                if fits { LimitCheck::WithinLimit } else { stop(needs - 1, needs, true) };
            assert_eq!(report.verdict, Some(expected), "a cap of {cap}");
        }

        // The transaction's own value: one record, the recipient's, and the log.
        let needs = WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE;
        for (tx_limit, fits) in [(needs, true), (needs - 1, false), (WRITE_RECORD_SIZE, false)] {
            let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(tx_limit);
            let mut ledger = ledger(limits, false, true, 0);
            let report = start_call(&mut ledger, 0, SENDER, CALLEE, 1);
            let expected =
                if fits { LimitCheck::WithinLimit } else { stop(tx_limit, needs, false) };
            assert_eq!(report.verdict, Some(expected), "a limit of {tx_limit}");
        }
    }

    /// A child's return holds its caller to the ledger's check with what the child left it, every
    /// time: a failed creation's nonce record lands on the creator unchecked, and the creator is
    /// stopped before it runs on when the record puts it over its budget; a success hands its
    /// caller no more than the share it gave, and the caller runs on.
    #[test]
    fn test_a_childs_return_holds_its_caller_to_what_it_left() {
        // A frame cap of 100: the transaction's own frame gets 100, a child 98.
        let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(100);
        for (case, left, failed, stopped) in [
            ("a success within its share", 98, false, false),
            ("a failed creation's nonce record, within", 60, true, false),
            ("a failed creation's nonce record, over the budget", 61, true, true),
        ] {
            let mut ledger = ledger(limits, false, false, 0);
            start_call(&mut ledger, 0, SENDER, CALLEE, 0);
            // What the caller kept of its own before the start.
            let count = WriteCount {
                added: LimitUsage { data_size: left, write_records: 0 },
                ..WriteCount::NONE
            };
            assert_eq!(ledger.count_write(&count), LimitCheck::WithinLimit, "{case}");
            if failed {
                ledger.open_start(StartFacts {
                    depth: 1,
                    caller: CALLEE,
                    target: None,
                    transfers_value: false,
                });
                let start = ledger.before_creation();
                ledger.after_creation(start, TARGET, Some(checkpoint(&ledger)), false);
                let _ = ledger.close_start();
            } else {
                start_call(&mut ledger, 1, CALLEE, TARGET, 0);
            }
            end_frame(&mut ledger, failed);
            let check = ledger.check();
            assert_eq!(check.exceeded_limit(), stopped, "{case}: {check:?}");
            if stopped {
                assert_eq!(check, stop(100, left + WRITE_RECORD_SIZE, true), "{case}");
            }
        }
    }

    /// Validation admits a body whose bytes, with one record for each entry of the authorization
    /// list, are within the transaction's limit, and nothing over it; an exempt transaction
    /// whatever it carries.
    #[test]
    fn test_validation_admits_a_body_and_its_authority_records_within_the_limit() {
        let body = TX_BODY_SIZE + 2 * AUTHORIZATION_SIZE;
        let at = body + 2 * WRITE_RECORD_SIZE;
        let under = |limit: u64, exempt: bool| {
            ledger(
                EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
                exempt,
                false,
                body,
            )
        };
        assert!(under(at, false).admits_tx_body(2), "at the limit");
        assert!(!under(at - 1, false).admits_tx_body(2), "one byte over it");
        assert!(under(at - 1, false).admits_tx_body(1), "one record fewer");
        assert!(!under(body - 1, false).admits_tx_body(0), "the body alone over it");
        assert!(under(0, true).admits_tx_body(2), "an exempt transaction");
        assert_eq!(under(at, false).body(), body);
        assert_eq!(under(at, false).usage().data_size, body, "the body is counted");
    }

    /// Counts `write` on `ledger` as the completed-write hook does, and reports the verdict and
    /// the history bytes.
    fn commit(ledger: &mut Ledger, write: &CompletedWrite<'_>) -> (LimitCheck, HistoryBytes) {
        let (count, history) = ledger.write_count(write);
        (ledger.count_write(&count), history)
    }

    /// A `SELFDESTRUCT` that moved value to another account counts its transfer log with the
    /// beneficiary's record, in one check, and reports the record's history alone. To the sender,
    /// whose account the body counts, it is the log alone, and the log alone can cross; and nothing
    /// counts a log when the transaction's value movements journal none.
    #[test]
    fn test_a_destruction_counts_its_transfer_log_with_its_beneficiary() {
        let moved = SelfDestructResult { had_value: true, ..Default::default() };
        let destruction =
            |target| CompletedWrite::SelfDestruct { address: CALLEE, target, result: &moved };
        let mut logging = ledger_from_sender(EvmTxRuntimeLimits::no_limits(), true);
        let (check, history) = commit(&mut logging, &destruction(TARGET));
        assert_eq!(check, LimitCheck::WithinLimit);
        assert_eq!(history, HistoryBytes::Appended(WRITE_RECORD_SIZE), "the record's alone");
        assert_eq!(
            logging.usage(),
            LimitUsage { data_size: WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE, write_records: 1 },
        );

        let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TRANSFER_LOG_SIZE - 1);
        let mut tight = ledger_from_sender(limits, true);
        let (check, history) = commit(&mut tight, &destruction(SENDER));
        assert_eq!(history, HistoryBytes::None);
        assert_eq!(
            check,
            LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit: TRANSFER_LOG_SIZE - 1,
                used: TRANSFER_LOG_SIZE,
                frame_local: false,
            },
            "the log alone crosses",
        );

        let mut silent = ledger_from_sender(EvmTxRuntimeLimits::no_limits(), false);
        let _ = commit(&mut silent, &destruction(TARGET));
        assert_eq!(silent.usage(), WRITE_RECORD, "no log, no bytes");
    }
}
