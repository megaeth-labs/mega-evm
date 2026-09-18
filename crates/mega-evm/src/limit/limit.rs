//! The per-transaction state of the common execution layer.

use alloy_primitives::Address;
use revm::{handler::FrameResult, interpreter::FrameInput};

use super::{
    frame_limit::{FrameLimitTracker, Lane},
    record::{RecordEffect, StagedRecord},
    LimitUsage, WRITE_RECORD,
};

/// What the common execution layer tracks for the running transaction: the per-frame lanes of
/// data-size bytes and write records, and the record the Host staged for the running opcode.
///
/// It lives on the [`MegaContext`](crate::MegaContext), is reset before each transaction and
/// system call, and is driven by the Host (staging), the opcode wrappers (commit and discard)
/// and the frame lifecycle (lanes).
#[derive(Clone, Debug, Default)]
pub struct AdditionalLimit {
    pub(crate) tracker: FrameLimitTracker,
    staged: Option<StagedRecord>,
    /// Whether the transaction's call target is an applied EIP-7702 authority, whose account
    /// write the transaction's lane already counts.
    target_is_authority: bool,
    /// The history gas the settled transaction spent.
    history_gas_spent: u64,
}

impl AdditionalLimit {
    /// Clears the state for a new transaction, keeping allocations.
    pub(crate) fn reset(&mut self) {
        self.tracker.reset();
        self.staged = None;
        self.target_is_authority = false;
        self.history_gas_spent = 0;
    }

    /// What the transaction keeps so far: its data-size bytes and write records, with every
    /// running frame counted as if it succeeds.
    pub fn usage(&self) -> LimitUsage {
        self.tracker.net()
    }

    /// The history gas the last settled transaction spent.
    pub const fn history_gas_spent(&self) -> u64 {
        self.history_gas_spent
    }

    /// Records the history gas the settled transaction spent.
    pub(crate) const fn set_history_gas_spent(&mut self, history_gas_spent: u64) {
        self.history_gas_spent = history_gas_spent;
    }

    /* Observe, stage, commit */

    /// The record the Host staged for the running opcode, if any.
    pub const fn staged_record(&self) -> Option<&StagedRecord> {
        self.staged.as_ref()
    }

    /// Stages the facts the Host observed. Called by the Host, which records nothing itself.
    #[inline]
    pub(crate) fn stage_record(&mut self, record: StagedRecord) {
        self.staged = Some(record);
    }

    /// Commits the staged record to the running frame's lane. Called by an opcode's wrapper once
    /// the opcode completed.
    #[inline]
    pub(crate) fn commit_staged_record(&mut self) {
        let Some(record) = self.staged.take() else { return };
        match record.effect() {
            RecordEffect::None => {}
            RecordEffect::Record(usage) => self.tracker.record(usage),
            RecordEffect::Refund(usage) => self.tracker.refund(usage),
        }
    }

    /// Discards the staged record. Called by an opcode's wrapper when the opcode failed, which
    /// takes the observed write back with it.
    #[inline]
    pub(crate) fn discard_staged_record(&mut self) {
        self.staged = None;
    }

    /// Discards a record staged outside an opcode's wrapper. Called by each wrapper on entry, so
    /// a record nobody committed cannot be committed for the wrong opcode.
    #[inline]
    pub(crate) fn discard_stale_record(&mut self) {
        self.staged = None;
    }

    /* Transaction-level records */

    /// Records the account writes of the applied EIP-7702 authorities other than the sender:
    /// `authorities` distinct accounts, `target_is_authority` if the transaction's call target is
    /// one of them.
    pub(crate) fn record_applied_authorities(
        &mut self,
        authorities: u64,
        target_is_authority: bool,
    ) {
        self.target_is_authority = target_is_authority;
        self.tracker.record(WRITE_RECORD.times(authorities));
    }

    /* Frame lanes */

    /// Pushes the lane of a frame about to start and counts the writes its start makes:
    ///
    /// - a call that transfers value: the sender's account (once per sender frame) and the
    ///   recipient's, one record when they are the same account;
    /// - a creation: the created account, and the creator's nonce (once per creator frame);
    /// - the transaction's own frame: the value recipient or the created account. The sender's
    ///   account is part of the transaction body, not a record here.
    pub(crate) fn on_frame_init(&mut self, input: &FrameInput, depth: usize) {
        match input {
            FrameInput::Call(inputs) => {
                let target = inputs.target_address;
                let transfers_value = inputs.transfers_value();
                if depth == 0 {
                    let written_outside = target == inputs.caller || self.target_is_authority;
                    self.tracker.push(Lane::new(
                        Some(target),
                        written_outside || transfers_value,
                        u64::MAX,
                    ));
                    if transfers_value && !written_outside {
                        self.tracker.record(WRITE_RECORD);
                    }
                    return;
                }
                let inherited = self.tracker.current().is_some_and(|caller| {
                    caller.address == Some(target) && caller.account_recorded
                });
                self.tracker.push(Lane::new(Some(target), inherited || transfers_value, u64::MAX));
                if transfers_value {
                    self.tracker.record_caller(false);
                    if target != inputs.caller {
                        self.tracker.record(WRITE_RECORD);
                    }
                }
            }
            FrameInput::Create(_) => {
                self.tracker.push(Lane::new(None, true, u64::MAX));
                self.tracker.record(WRITE_RECORD);
                if depth > 0 {
                    self.tracker.record_caller(true);
                }
            }
            FrameInput::Empty => self.push_empty_frame(),
        }
    }

    /// Sets the account the running frame runs as, once known (a creation's address).
    pub(crate) fn set_frame_address(&mut self, address: Address) {
        if let Some(lane) = self.tracker.current_mut() {
            lane.address = Some(address);
        }
    }

    /// Takes a creation's creator record back: the creation failed before bumping the nonce.
    pub(crate) fn creation_did_not_bump_nonce(&mut self) {
        self.tracker.drop_caller_record();
    }

    /// Pushes the lane of a frame answered without running: a result built without an
    /// interpreter keeps the lanes aligned with the frames revm returns.
    pub(crate) fn push_empty_frame(&mut self) {
        self.tracker.push(Lane::empty());
    }

    /// Pops the lane of the frame `result` returns from: a success merges it into the caller's,
    /// a failure discards it.
    pub(crate) fn on_frame_return(&mut self, result: &FrameResult) {
        self.tracker.pop(result.instruction_result().is_ok());
    }

    /// Settles the transaction's outermost frame: pops its lane unless the frame already
    /// returned through [`on_frame_return`](Self::on_frame_return), which is the case whenever
    /// it ran.
    pub(crate) fn on_last_frame_return(&mut self, result: &FrameResult) {
        debug_assert!(self.tracker.depth() <= 1, "only the outermost lane can be left");
        if self.tracker.depth() == 1 {
            self.on_frame_return(result);
        }
    }
}
