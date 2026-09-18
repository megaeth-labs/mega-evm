//! The per-transaction state of the common execution layer.

use alloy_primitives::Address;
use revm::{
    handler::FrameResult,
    interpreter::{FrameInput, InstructionResult},
};

use super::{
    frame_limit::{FrameLimitTracker, Lane},
    record::{RecordEffect, StagedRecord},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, WRITE_RECORD,
};

/// What the common execution layer tracks for the running transaction: the per-frame lanes of
/// data-size bytes and write records, the record the Host staged for the running opcode, and the
/// latch of a transaction a limit stopped.
///
/// It lives on the [`MegaContext`](crate::MegaContext), is reset before each transaction and
/// system call, and is driven by the Host (staging), the opcode wrappers (commit and discard)
/// and the frame lifecycle (lanes, the latch).
///
/// # The abort protocol
///
/// A transaction-level limit stops the transaction with a revert, not a halt. The frame that
/// crosses the limit reverts with [`MegaLimitExceeded`](crate::MegaLimitExceeded) as its output
/// and the transaction is latched ([`latch`](Self::latch)). From then on no frame runs another
/// instruction: a caller that gets the reverted result back returns the same revert instead of
/// resuming, a frame about to start is answered with it, and every result returned above is
/// rewritten to it. The outermost frame settles like any EIP-8037 revert: its unspent regular gas
/// and the reservoir go back to the sender, who pays only for what ran. A frame budget crossed
/// reverts that frame alone, without a latch, and its caller resumes. Exceptional halts (a real
/// out-of-gas, an invalid opcode) are not limit stops and still burn the frame's gas.
#[derive(Clone, Debug, Default)]
pub struct AdditionalLimit {
    pub(crate) tracker: FrameLimitTracker,
    staged: Option<StagedRecord>,
    limits: EvmTxRuntimeLimits,
    /// The transaction-level stop, once a limit latched one.
    latched: Option<LimitCheck>,
    /// Whether the transaction's call target is an applied EIP-7702 authority, whose account
    /// write the transaction's lane already counts.
    target_is_authority: bool,
    /// The transaction's sender, whose account write is part of the transaction body: a frame
    /// running as the sender never records it.
    sender: Address,
    /// The history gas the settled transaction spent.
    history_gas_spent: u64,
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

    /// Clears the state for a new transaction, keeping allocations and the limits.
    pub(crate) fn reset(&mut self) {
        self.tracker.reset();
        self.staged = None;
        self.latched = None;
        self.target_is_authority = false;
        self.sender = Address::ZERO;
        self.history_gas_spent = 0;
    }

    /* The latch */

    /// The stop a transaction-level limit latched, if any.
    pub const fn latched(&self) -> Option<&LimitCheck> {
        self.latched.as_ref()
    }

    /// Latches the transaction: `kind`'s transaction-level `limit` was crossed at `used`. The
    /// running frame must stop with [`LimitCheck::revert_data`]; the frame lifecycle stops every
    /// frame above it. A later latch does not replace the first.
    pub fn latch(&mut self, kind: LimitKind, limit: u64, used: u64) -> LimitCheck {
        *self.latched.get_or_insert(LimitCheck::ExceedsLimit {
            kind,
            limit,
            used,
            frame_local: false,
        })
    }

    /// Checks the limits after the running frame counted something: the transaction's data size
    /// against its limit (latching on a crossing), then the running frame's against its budget.
    fn check(&mut self) -> LimitCheck {
        let used = self.tracker.net().data_size;
        if used > self.limits.tx_data_size_limit {
            return self.latch(LimitKind::DataSize, self.limits.tx_data_size_limit, used);
        }
        if let Some(lane) = self.tracker.current() {
            let used = lane.net().data_size;
            if used > lane.budget {
                return LimitCheck::ExceedsLimit {
                    kind: LimitKind::DataSize,
                    limit: lane.budget,
                    used,
                    frame_local: true,
                };
            }
        }
        LimitCheck::WithinLimit
    }

    /// Rewrites `result` to the latched stop, when the transaction is latched: a success or a
    /// revert becomes the latched revert. A halt stays what it is.
    fn apply_latch(&self, result: &mut FrameResult) {
        let Some(latched) = &self.latched else { return };
        let interpreter_result = result.interpreter_result_mut();
        if interpreter_result.result.is_ok_or_revert() {
            interpreter_result.result = InstructionResult::Revert;
            interpreter_result.output = latched.revert_data();
        }
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
    /// the opcode completed; a crossed limit in the verdict stops the opcode's frame.
    #[inline]
    pub(crate) fn commit_staged_record(&mut self) -> LimitCheck {
        let Some(record) = self.staged.take() else { return LimitCheck::WithinLimit };
        match record.effect(self.sender) {
            RecordEffect::None => LimitCheck::WithinLimit,
            RecordEffect::Record(usage) => {
                self.tracker.record(usage);
                self.check()
            }
            RecordEffect::Refund(usage) => {
                self.tracker.refund(usage);
                LimitCheck::WithinLimit
            }
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
        // Crossing the limit here latches the transaction before its first frame, which is then
        // answered with the stop without running.
        self.check();
    }

    /* Frame lanes */

    /// Pushes the lane of a frame about to start and counts the writes its start makes:
    ///
    /// - a call that transfers value: the sender's account (once per sender frame) and the
    ///   recipient's, one record when they are the same account;
    /// - a creation: the created account, and the creator's nonce (once per creator frame);
    /// - the transaction's own frame: the value recipient or the created account.
    ///
    /// The sender's account is part of the transaction body and never a record here: a frame
    /// running as the sender (reached through an EIP-7702 delegation) counts it as recorded, and
    /// a value transfer to the sender records no recipient.
    ///
    /// A crossed limit in the verdict means the frame must not run: it is answered with the stop.
    /// So is every frame of a latched transaction, whose lane stays empty.
    pub(crate) fn on_frame_init(&mut self, input: &FrameInput, depth: usize) -> LimitCheck {
        if let Some(latched) = self.latched {
            self.push_empty_frame();
            return latched;
        }
        self.push_lane(input, depth);
        self.check()
    }

    fn push_lane(&mut self, input: &FrameInput, depth: usize) {
        let budget = match self.tracker.current() {
            Some(caller) => caller.remaining_budget().min(self.limits.frame_data_size_limit),
            None => self.limits.frame_data_size_limit,
        };
        match input {
            FrameInput::Call(inputs) => {
                let target = inputs.target_address;
                let transfers_value = inputs.transfers_value();
                if depth == 0 {
                    self.sender = inputs.caller;
                    let written_outside = target == inputs.caller || self.target_is_authority;
                    self.tracker.push(Lane::new(
                        Some(target),
                        written_outside || transfers_value,
                        budget,
                    ));
                    if transfers_value && !written_outside {
                        self.tracker.record(WRITE_RECORD);
                    }
                    return;
                }
                let inherited = self.tracker.current().is_some_and(|caller| {
                    caller.address == Some(target) && caller.account_recorded
                });
                let is_sender = target == self.sender;
                self.tracker.push(Lane::new(
                    Some(target),
                    inherited || is_sender || transfers_value,
                    budget,
                ));
                if transfers_value {
                    self.tracker.record_caller(false);
                    if target != inputs.caller && !is_sender {
                        self.tracker.record(WRITE_RECORD);
                    }
                }
            }
            FrameInput::Create(inputs) => {
                self.tracker.push(Lane::new(None, true, budget));
                self.tracker.record(WRITE_RECORD);
                if depth == 0 {
                    self.sender = inputs.caller();
                } else {
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
    /// a failure discards it. Under a latch the result is first rewritten to the latched stop,
    /// whatever produced it (an interceptor, an inspector's rewrite), so no success passes it.
    pub(crate) fn on_frame_return(&mut self, result: &mut FrameResult) {
        self.apply_latch(result);
        self.tracker.pop(result.instruction_result().is_ok());
    }

    /// Settles the transaction's outermost frame: pops its lane unless the frame already
    /// returned through [`on_frame_return`](Self::on_frame_return), which is the case whenever
    /// it ran, and rewrites the result to the latched stop.
    pub(crate) fn on_last_frame_return(&mut self, result: &mut FrameResult) {
        debug_assert!(self.tracker.depth() <= 1, "only the outermost lane can be left");
        if self.tracker.depth() == 1 {
            self.on_frame_return(result);
        } else {
            self.apply_latch(result);
        }
    }
}
