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
//! The lanes ([`AdditionalLimit`](crate::AdditionalLimit)) count the same quantities and stay the
//! ones every decision is taken on; under the `ledger-shadow` feature the two are compared where
//! they must agree.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_primitives::{Address, U256};
use revm::context_interface::journaled_state::JournalCheckpoint;

use super::{
    limit::share_of_remaining, record::WriteCount, EvmTxRuntimeLimits, LimitCheck, LimitKind,
    LimitUsage, TRANSFER_LOG, WRITE_RECORD,
};

/// Asserts an invariant of the ledger's levels in debug builds and in `ledger-shadow` builds; in
/// any other build the check is type-checked and compiled out, as `debug_assert!`'s is.
macro_rules! ledger_assert {
    ($($arg:tt)*) => {
        if cfg!(any(debug_assertions, feature = "ledger-shadow")) {
            assert!($($arg)*);
        }
    };
}
pub(crate) use ledger_assert;

/// What the ledger counts that the journal rewinds: additions and refunds, kept apart as the lanes
/// keep them, so that a frame's own usage saturates as a lane's does, and the log and code bytes
/// history counts beside the write records.
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

/// A frame level's frame: who it is, and what it may keep.
#[derive(Clone, Copy, Debug)]
struct FrameLevel {
    /// The frame's start, numbered in the order starts opened their levels in the transaction.
    seq: u64,
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
    /// The flag of the nearest frame level below it as it stood when it opened, which its revert
    /// restores.
    flag_at_open: bool,
    /// The number the next frame start would get when it opened: a frame level numbered at or
    /// above it opened inside it.
    seq_at_open: u64,
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

/// The start window, open while revm decides one frame start.
#[derive(Clone, Copy, Debug)]
struct Window {
    facts: StartFacts,
    /// Whether the start's frame level opened.
    opened: bool,
    /// Whether the start's write — a call's transfer, a creation's account checkpoint — was seen.
    counted: bool,
}

/// What a frame start counted, as the ledger saw it while revm decided the start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartReport {
    /// The number of the frame level the start opened; `None` when revm opened none.
    pub(crate) seq: Option<u64>,
    /// The records counted on the start's own level: a value call's recipient, a creation's
    /// account.
    pub(crate) on_level: u64,
    /// Whether the start counted its caller's record on its own level: a value transfer's sender.
    pub(crate) caller_on_level: bool,
    /// Whether the start counted its creator's nonce record, below its own level, where the
    /// creation's failure leaves it.
    pub(crate) creator: bool,
    /// Whether the start counted the transfer log revm journaled for it.
    pub(crate) transfer_log: bool,
    /// Whether revm refused the start's value transfer.
    pub(crate) refused: bool,
    /// The verdict on the start, held to the limits while its level was open; `None` when nothing
    /// was held: a refused transfer, a creation that failed before its level opened, a start revm
    /// never decided.
    pub(crate) verdict: Option<LimitCheck>,
}

/// A point the ledger and revm's journal can be taken back to together
/// ([`MegaJournal::revert_to`](crate::MegaJournal)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    /// Where revm's journal stood.
    pub(crate) position: JournalCheckpoint,
    /// The counts.
    counts: Counts,
    /// The flag of the nearest frame level; `None` when there is none.
    flag: Option<bool>,
    /// The number the next frame start would get: a frame level numbered at or above it is taken
    /// back with the mark.
    seq: u64,
    /// The number of levels open: a revert to the mark finds at most these.
    levels: usize,
}

/// The facts of a creation's start, taken before anything of it is counted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CreationStart {
    /// The creation's budget, from what its creator had left before its nonce record.
    budget: u64,
    /// Where the creation's usage is measured from: before its creator's nonce record.
    at_base: Counts,
    /// Whether the creator's nonce record was counted.
    creator: bool,
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
    /// What the last start counted.
    last_start: Option<StartReport>,
    /// The last frame level closed, and whether it was kept: committed, and not taken back since.
    last_closed: Option<(u64, bool)>,
    /// The number the next frame level gets.
    next_seq: u64,
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
    /// Whether a tool's checkpoint that enclosed a frame start was reverted: the lanes do not
    /// follow it, and the shadow stops comparing for the transaction.
    #[cfg(feature = "ledger-shadow")]
    diverged: bool,
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
        self.last_closed = None;
        self.next_seq = 0;
        self.sender = Address::ZERO;
        self.target_is_authority = false;
        self.transfer_logs = transfer_logs;
        self.exempt = exempt;
        #[cfg(feature = "ledger-shadow")]
        {
            self.diverged = false;
        }
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
    #[cfg(feature = "ledger-shadow")]
    pub(crate) fn log_and_code_bytes(&self) -> u64 {
        self.counts.log_and_code_bytes
    }

    /// The number of levels open, which is revm's journal depth.
    pub(crate) fn depth(&self) -> usize {
        self.levels.len()
    }

    /// The number of frame levels open: the frames on the call stack.
    #[cfg(feature = "ledger-shadow")]
    pub(crate) fn frame_levels(&self) -> usize {
        self.levels.iter().filter(|level| level.frame.is_some()).count()
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

    /// The running frame's depth and number, the identity its level was opened with.
    pub(crate) fn running_frame(&self) -> Option<(usize, u64)> {
        self.frame_level().map(|level| (level.depth, level.seq))
    }

    /// The running frame's budget and what it keeps so far.
    #[cfg(any(test, feature = "ledger-shadow"))]
    pub(crate) fn running_budget(&self) -> Option<(u64, LimitUsage)> {
        self.frame_level().map(|level| (level.budget, level.usage(&self.counts)))
    }

    /// Whether the frame level numbered `seq` is the last one closed, and was kept: committed, and
    /// not taken back since.
    pub(crate) fn kept(&self, seq: u64) -> bool {
        self.last_closed == Some((seq, true))
    }

    /// Whether a tool's checkpoint that enclosed a frame start was reverted in the transaction.
    #[cfg(feature = "ledger-shadow")]
    pub(crate) const fn diverged(&self) -> bool {
        self.diverged
    }

    /* Verdicts */

    /// The verdict on `used` crossing `kind`'s `limit`: within, whatever it crossed, for an exempt
    /// transaction.
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

    /// The verdict on a start revm is not asked to decide — a call gas detention answers without
    /// running the precompile it targets — had revm made it: the records it would count and the
    /// transfer log of the value it would move, held to the transaction's limit and to the budget
    /// its frame would get. Nothing is counted.
    pub(crate) fn hypothetical_start(&self, facts: &StartFacts) -> LimitCheck {
        let Some(target) = facts.target else {
            unreachable!("only a call is answered without revm deciding its start")
        };
        let mut counted = LimitUsage::ZERO;
        if facts.transfers_value {
            let (on_level, caller) = if facts.depth == 0 {
                (target != facts.caller && !self.target_is_authority, false)
            } else {
                let caller = !self.frame_level().is_some_and(|level| level.recorded);
                (target != facts.caller && target != self.sender, caller)
            };
            counted = WRITE_RECORD.times(u64::from(on_level) + u64::from(caller));
            if self.transfer_logs && target != facts.caller {
                counted = counted.saturating_add(TRANSFER_LOG);
            }
        }
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
    pub(crate) fn count_authorities(
        &mut self,
        sender: Address,
        authorities: u64,
        target_is_authority: bool,
    ) {
        self.sender = sender;
        self.target_is_authority = target_is_authority;
        self.counts.used = self.counts.used.saturating_add(WRITE_RECORD.times(authorities));
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
        ledger_assert!(self.window.is_none(), "start windows do not nest");
        if facts.depth == 0 {
            self.sender = facts.caller;
        }
        self.last_start = None;
        self.window = Some(Window { facts, opened: false, counted: false });
    }

    /// Closes the start window once revm decided the start, and reports what the start counted.
    pub(crate) fn close_start(&mut self) -> StartReport {
        self.window = None;
        self.last_start.take().unwrap_or_default()
    }

    /// The start report of the open window.
    fn report_mut(&mut self) -> &mut StartReport {
        self.last_start.get_or_insert_with(StartReport::default)
    }

    /* Levels */

    /// Opens a level at `opened_at`, a frame level when `frame` is given.
    fn open_level(&mut self, opened_at: JournalCheckpoint, frame: Option<FrameLevel>) {
        let flag_at_open = self.frame_level().is_some_and(|level| level.recorded);
        self.levels.push(Level {
            opened_at,
            at_open: self.counts,
            flag_at_open,
            seq_at_open: self.next_seq,
            frame,
        });
    }

    /// The number of the next frame level, taken.
    const fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
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
        let frame = FrameLevel {
            seq: self.take_seq(),
            depth,
            at_base: self.counts,
            budget: self.child_budget(),
            account: Some(target),
            recorded,
            returning: false,
        };
        self.open_level(opened_at, Some(frame));
        if let Some(window) = &mut self.window {
            window.opened = true;
        }
        let verdict = self.check();
        *self.report_mut() =
            StartReport { seq: Some(frame.seq), verdict: Some(verdict), ..Default::default() };
    }

    /// revm moved `value` from `from` to `to` for the call whose start the window is open for
    /// (`refused` when it could not), and journaled a transfer log or not (`logged`): counts the
    /// records the transfer makes on the call's level, then holds the start to the limits.
    ///
    /// The transaction's own frame records the recipient unless it is the sender or an applied
    /// authority. Any other records the recipient unless it is the caller or the sender, and the
    /// caller's own account once per frame; a `CALLCODE` or a call to itself records the caller
    /// although revm moves nothing.
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
        let mut caller_on_level = false;
        let on_level = if depth == 0 {
            u64::from(to != from && !self.target_is_authority)
        } else {
            if let Some(caller) = self.calling_level_mut() {
                caller_on_level = !caller.recorded;
                caller.recorded = true;
            }
            u64::from(to != from && to != self.sender)
        };
        let mut counted = WRITE_RECORD.times(on_level + u64::from(caller_on_level));
        if logged {
            counted = counted.saturating_add(TRANSFER_LOG);
        }
        self.counts.used = self.counts.used.saturating_add(counted);
        let verdict = self.check();
        let report = self.report_mut();
        report.on_level = on_level;
        report.caller_on_level = caller_on_level;
        report.transfer_log = logged;
        report.verdict = Some(verdict);
    }

    /// revm is about to open a creation's account checkpoint for `creator`'s start: takes the
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
        let mut creator = false;
        if depth > 0 {
            if let Some(level) = self.frame_level_mut() {
                creator = !level.recorded;
                level.recorded = true;
            }
        }
        if creator {
            self.counts.used = self.counts.used.saturating_add(WRITE_RECORD);
        }
        Some(CreationStart { budget, at_base, creator, depth })
    }

    /// revm opened (`Ok`) or failed to open (`Err`) the account checkpoint of a creation at
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
        let Some(opened_at) = opened else {
            *self.report_mut() = StartReport { creator: start.creator, ..Default::default() };
            return;
        };
        let frame = FrameLevel {
            seq: self.take_seq(),
            depth: start.depth,
            at_base: start.at_base,
            budget: start.budget,
            account: Some(address),
            recorded: true,
            returning: false,
        };
        self.open_level(opened_at, Some(frame));
        if let Some(window) = &mut self.window {
            window.opened = true;
        }
        let mut counted = WRITE_RECORD;
        if logged {
            counted = counted.saturating_add(TRANSFER_LOG);
        }
        self.counts.used = self.counts.used.saturating_add(counted);
        let verdict = self.check();
        *self.report_mut() = StartReport {
            seq: Some(frame.seq),
            on_level: 1,
            creator: start.creator,
            transfer_log: logged,
            verdict: Some(verdict),
            ..Default::default()
        };
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
        ledger_assert!(
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
        self.last_closed = Some((frame.seq, true));
    }

    /// revm's journal reverted to `checkpoint`: the latest level, which must be the one opened
    /// there, closes, and the counts and the flag of the nearest frame level below it are what they
    /// were when it opened.
    pub(crate) fn on_revert(&mut self, checkpoint: JournalCheckpoint) {
        let Some(level) = self.levels.pop() else { return };
        ledger_assert!(
            level.opened_at == checkpoint,
            "a revert closes the latest level: a revert below a level still open is not supported"
        );
        self.counts = level.at_open;
        if let Some(caller) = self.frame_level_mut() {
            caller.recorded = level.flag_at_open;
        }
        match level.frame {
            Some(frame) => {
                self.assert_frame_closes(&frame);
                self.last_closed = Some((frame.seq, false));
            }
            // A rollback level a frame started inside — a tool's checkpoint around a call — takes
            // back what the frame kept, so the frame's level is no longer kept either.
            None => {
                self.take_back_closed_from(level.seq_at_open);
                #[cfg(feature = "ledger-shadow")]
                if self.next_seq > level.seq_at_open {
                    self.diverged = true;
                }
            }
        }
    }

    /// Notes that the last frame level closed is taken back when it is numbered `seq` or above.
    const fn take_back_closed_from(&mut self, seq: u64) {
        if let Some((closed, kept)) = &mut self.last_closed {
            if *closed >= seq {
                *kept = false;
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
            flag: self.frame_level().map(|level| level.recorded),
            seq: self.next_seq,
            levels: self.levels.len(),
        }
    }

    /// The mark a frame revm just built is taken back to when it is rewritten into a failure: its
    /// own checkpoint, the counts its level opened with — after a creation's creator record — and
    /// the flag of the frame below as it stood then. Never where the frame's usage is measured
    /// from, which for a creation is before the creator's record.
    pub(crate) fn frame_start_mark(&self) -> Option<Mark> {
        let level = self.levels.last()?;
        let frame = level.frame?;
        let below = self.levels.len() - 1;
        let caller = self.levels[..below].iter().rev().any(|level| level.frame.is_some());
        Some(Mark {
            position: level.opened_at,
            counts: level.at_open,
            flag: caller.then_some(level.flag_at_open),
            seq: frame.seq,
            levels: below,
        })
    }

    /// Takes the ledger back to `mark`, which revm's journal is being reverted to: the counts, the
    /// running frame's flag, and whether the last frame level closed is kept.
    pub(crate) fn revert_to(&mut self, mark: &Mark) {
        ledger_assert!(
            self.levels.len() <= mark.levels,
            "a revert to a mark finds every level opened since closed"
        );
        self.counts = mark.counts;
        if let (Some(flag), Some(level)) = (mark.flag, self.frame_level_mut()) {
            level.recorded = flag;
        }
        self.take_back_closed_from(mark.seq);
    }
}

/// Satin's side of one frame start: what its caller paid for the records it makes, what the
/// ledger saw it count, and, for a frame revm built, the mark it is taken back to when it is
/// rewritten into a failure. One per frame result revm will return, as the lanes are.
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
    /// Whether the records the lanes counted for the start are those its charge paid for.
    #[cfg(feature = "ledger-shadow")]
    comparable: bool,
}

impl StartSide {
    /// The side of a start at `depth` whose caller paid `on_level_charge` and `caller_charge`.
    pub(crate) const fn new(depth: usize, on_level_charge: u64, caller_charge: u64) -> Self {
        Self {
            depth,
            on_level_charge,
            caller_charge,
            report: StartReport {
                seq: None,
                on_level: 0,
                caller_on_level: false,
                creator: false,
                transfer_log: false,
                refused: false,
                verdict: None,
            },
            mark: None,
            #[cfg(feature = "ledger-shadow")]
            comparable: true,
        }
    }

    /// Attaches what the ledger saw the start count and the built frame's mark.
    pub(crate) const fn attach(&mut self, report: StartReport, mark: Option<Mark>) {
        self.report = report;
        self.mark = mark;
    }

    /// Notes whether the records the lanes counted are those the charge paid for.
    #[cfg(feature = "ledger-shadow")]
    pub(crate) const fn set_comparable(&mut self, comparable: bool) {
        self.comparable = comparable;
    }

    /// Whether the shadow compares the refund: the records the lanes counted are those the charge
    /// paid for.
    #[cfg(feature = "ledger-shadow")]
    pub(crate) const fn comparable(&self) -> bool {
        self.comparable
    }

    /// The frame's depth.
    pub(crate) const fn depth(&self) -> usize {
        self.depth
    }

    /// The number of the frame level the start opened.
    pub(crate) const fn seq(&self) -> Option<u64> {
        self.report.seq
    }

    /// The mark of a frame revm built.
    pub(crate) const fn mark(&self) -> Option<&Mark> {
        self.mark.as_ref()
    }

    /// The history gas the caller gets back once the frame returned: what it paid less the price
    /// of the start's records `ledger` holds.
    ///
    /// The records on the start's own level are held when that level was kept — committed, and not
    /// taken back since; a creator's nonce record, counted below the creation's level, whenever it
    /// was counted. So a start answered before revm decided it, or refused before its writes, gets
    /// everything back; a call answered with a success keeps its records; a failed precompile, and
    /// a frame that fails, give theirs back, a creator's surviving record aside.
    #[cfg_attr(not(feature = "ledger-shadow"), allow(dead_code))]
    pub(crate) fn refund(&self, ledger: &Ledger) -> u64 {
        let kept = self.report.seq.is_some_and(|seq| ledger.kept(seq));
        let on_level = kept && self.report.on_level > 0;
        let caller = (kept && self.report.caller_on_level) || self.report.creator;
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
