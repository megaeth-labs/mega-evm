//! The per-transaction state of the common execution layer.

use alloy_primitives::Address;
use revm::{
    handler::FrameResult,
    interpreter::{CallInputs, CallScheme, FrameInput, InstructionResult},
};

use super::{
    frame_limit::{FrameLimitTracker, Lane},
    record::{HistoryBytes, RecordEffect, StagedRecord},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, WRITE_RECORD,
};
use crate::storage_call_stipend;

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
    /// Whether the transaction's first frame reached frame init. When it did not, the runtime
    /// phase before it ran out of gas and took back everything counted before it.
    frame_began: bool,
    /// The history gas validation charged for the transaction's body, part of
    /// [`history_gas_spent`](Self::history_gas_spent).
    intrinsic_history_gas: u64,
    /// The history gas of the write record the transaction's own frame makes — its value's
    /// recipient, or the account it creates — charged before the first frame and given back when
    /// that frame fails.
    top_level_write_record_gas: u64,
    /// The history gas the running opcode charged its own frame for the records the frame it is
    /// starting will make, waiting for that frame's lane to be pushed.
    pending_frame_charge: FrameCharge,
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
        self.frame_began = false;
        self.intrinsic_history_gas = 0;
        self.top_level_write_record_gas = 0;
        self.pending_frame_charge = FrameCharge::NONE;
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

    /// The history gas validation charged for the transaction's body: the bytes the transaction
    /// carries and the write records its inclusion makes, which are known before it runs.
    ///
    /// It rides in the EIP-8037 intrinsic state-gas slot, so the reservoir pays it first, and the
    /// settled result takes it back out of the state gas it reports: it is history, not state.
    pub const fn intrinsic_history_gas(&self) -> u64 {
        self.intrinsic_history_gas
    }

    /// Records the history gas validation charged for the transaction's body.
    pub(crate) const fn set_intrinsic_history_gas(&mut self, gas: u64) {
        self.intrinsic_history_gas = gas;
    }

    /// The history gas charged before the first frame for the write record that frame makes: the
    /// recipient of the transaction's value, or the account it creates.
    ///
    /// A first frame that fails keeps no such write, so the charge is given back with the rest of
    /// what the failure discards.
    pub(crate) const fn top_level_write_record_gas(&self) -> u64 {
        self.top_level_write_record_gas
    }

    /// Records the history gas charged for the transaction's own write record.
    pub(crate) const fn set_top_level_write_record_gas(&mut self, gas: u64) {
        self.top_level_write_record_gas = gas;
    }

    /// Draws up to `amount` from the running frame's history allowance and reports what it gave;
    /// the caller pays the rest out of the frame's own gas.
    ///
    /// Only a history charge may draw, and only the one a log makes: a write record, a state
    /// charge and a unit of computation are all paid for by the frame's gas alone. See
    /// [`storage_call_stipend`](crate::storage_call_stipend).
    #[inline]
    pub(crate) fn try_consume_stipend(&mut self, amount: u64) -> u64 {
        self.tracker.consume_stipend(amount)
    }

    /// Whether the transaction's call target is an applied EIP-7702 authority, whose account
    /// write the transaction's own lane already counts, so its first frame writes no recipient.
    pub(crate) const fn target_is_authority(&self) -> bool {
        self.target_is_authority
    }

    /// Leaves the history gas the running opcode just charged its frame for the records the frame
    /// it starts will make: `on_lane` for the records that frame's failure discards, `caller` for
    /// the record of the caller's own account.
    ///
    /// The next lane pushed takes it, whether that is the frame's own or the empty one of a frame
    /// answered without running.
    #[inline]
    pub(crate) const fn stage_frame_charge(&mut self, on_lane: u64, caller: u64) {
        self.pending_frame_charge = FrameCharge { on_lane, caller };
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

    /// Commits the staged record to the running frame's lane, and reports the history bytes it
    /// appends or takes back. Called by an opcode's wrapper once the opcode completed; a crossed
    /// limit in the verdict stops the opcode's frame, and the wrapper charges the history.
    #[inline]
    pub(crate) fn commit_staged_record(&mut self) -> (LimitCheck, HistoryBytes) {
        let Some(record) = self.staged.take() else {
            return (LimitCheck::WithinLimit, HistoryBytes::None);
        };
        let effect = record.effect(self.sender);
        let history = effect.history_bytes();
        let check = match effect {
            RecordEffect::None => LimitCheck::WithinLimit,
            RecordEffect::Record(usage) => {
                self.tracker.record(usage);
                self.check()
            }
            RecordEffect::Refund(usage) => {
                self.tracker.refund(usage);
                LimitCheck::WithinLimit
            }
        };
        (check, history)
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

    /// Counts the `bytes` of an Oracle hint, the payload a `sendHint` call hands to the node's
    /// oracle service.
    ///
    /// The bytes are counted before the payload is decoded, so a caller cannot make the node
    /// materialise a payload for free by appending bytes an ABI decoder ignores. They are the
    /// transaction's, not the calling frame's: the hint has left the machine by the time the
    /// frame could fail, so nothing takes it back. A crossing latches the transaction, and the
    /// hint is not forwarded.
    pub(crate) fn record_hint_bytes(&mut self, bytes: u64) -> LimitCheck {
        self.tracker.record_tx(LimitUsage { data_size: bytes, write_records: 0 });
        self.check()
    }

    /// Records the account writes of the applied EIP-7702 authorities other than the sender:
    /// `authorities` distinct accounts, `target_is_authority` if the transaction's call target is
    /// one of them, from the transaction's `sender`.
    ///
    /// The limit is checked before the records are made: a crossing latches the transaction and
    /// records nothing, and the caller takes the authorizations back, so the writes the limit
    /// guards never happen. The first frame is then answered with the stop without running.
    pub(crate) fn record_applied_authorities(
        &mut self,
        sender: Address,
        authorities: u64,
        target_is_authority: bool,
    ) -> LimitCheck {
        self.sender = sender;
        let records = WRITE_RECORD.times(authorities);
        let used = self.tracker.net().saturating_add(records).data_size;
        if used > self.limits.tx_data_size_limit {
            return self.latch(LimitKind::DataSize, self.limits.tx_data_size_limit, used);
        }
        self.target_is_authority = target_is_authority;
        self.tracker.record(records);
        LimitCheck::WithinLimit
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
        self.frame_began = true;
        if let Some(latched) = self.latched {
            self.push_empty_frame();
            return latched;
        }
        self.push_lane(input, depth);
        self.check()
    }

    /// Takes a creation's creator record back: the creation failed before bumping the nonce.
    ///
    /// The record is gone, so the caller gets its history back when the frame returns: the lane no
    /// longer holds a creator's record, and a lane that holds none gives its caller's charge back
    /// with the rest.
    pub(crate) fn creation_did_not_bump_nonce(&mut self) {
        self.tracker.drop_caller_record();
    }

    fn push_lane(&mut self, input: &FrameInput, depth: usize) {
        let budget = match self.tracker.current() {
            Some(caller) => caller.remaining_budget().min(self.limits.frame_data_size_limit),
            None => self.limits.frame_data_size_limit,
        };
        // What the caller paid for these records at its opcode. The transaction's own frame has
        // no such caller: its record is charged before execution and given back by the settlement
        // ([`top_level_write_record_gas`](Self::top_level_write_record_gas)).
        let charge = core::mem::replace(&mut self.pending_frame_charge, FrameCharge::NONE);
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
                        0,
                    ));
                    if transfers_value && !written_outside {
                        self.tracker.record(WRITE_RECORD);
                    }
                    return;
                }
                let records = self.frame_start_records(input);
                let inherited = self.tracker.current().is_some_and(|caller| {
                    caller.address == Some(target) && caller.account_recorded
                });
                let is_sender = target == self.sender;
                self.tracker.push(Lane::new(
                    Some(target),
                    inherited || is_sender || transfers_value,
                    budget,
                    charge.on_lane,
                ));
                if records.caller {
                    self.tracker.record_caller(false, charge.caller);
                }
                if records.on_lane > 0 {
                    self.tracker.record(WRITE_RECORD.times(records.on_lane));
                }
                if grants_stipend(inputs) {
                    self.tracker.grant_stipend(storage_call_stipend());
                }
            }
            FrameInput::Create(inputs) => {
                let records = self.frame_start_records(input);
                self.tracker.push(Lane::new(None, true, budget, charge.on_lane));
                self.tracker.record(WRITE_RECORD.times(records.on_lane));
                if depth == 0 {
                    self.sender = inputs.caller();
                } else if records.caller {
                    self.tracker.record_caller(true, charge.caller);
                }
            }
            FrameInput::Empty => self.push_empty_frame(),
        }
    }

    /// The write records starting `input` makes at a depth above the transaction's own frame, for
    /// the opcode that starts it: `on_lane` are the records the frame's failure discards, `caller`
    /// whether its start also writes the caller's own account.
    ///
    /// The caller pays for both at the opcode, before it forwards gas, so the frame's own budget
    /// carries none of them — which is what lets a value transfer's allowance pay for what the
    /// recipient does with it. [`push_lane`](Self::push_lane) makes exactly these records, from
    /// this same answer, so the charge and the count cannot disagree.
    pub(crate) fn frame_start_records(&self, input: &FrameInput) -> FrameStartRecords {
        let caller_recorded = self.tracker.current().is_some_and(|lane| lane.account_recorded);
        match input {
            FrameInput::Call(inputs) => {
                if !inputs.transfers_value() {
                    return FrameStartRecords::NONE;
                }
                let target = inputs.target_address;
                let writes_target = target != inputs.caller && target != self.sender;
                FrameStartRecords { on_lane: u64::from(writes_target), caller: !caller_recorded }
            }
            FrameInput::Create(_) => FrameStartRecords { on_lane: 1, caller: !caller_recorded },
            FrameInput::Empty => FrameStartRecords::NONE,
        }
    }

    /// Sets the account the running frame runs as, once known (a creation's address).
    pub(crate) fn set_frame_address(&mut self, address: Address) {
        if let Some(lane) = self.tracker.current_mut() {
            lane.address = Some(address);
        }
    }

    /// Pushes the lane of a frame answered without running: a result built without an
    /// interpreter keeps the lanes aligned with the frames revm returns.
    ///
    /// Such a frame records none of the writes its caller paid for, so the whole charge sits on
    /// the lane and comes back when it is popped, whatever the answer was.
    pub(crate) fn push_empty_frame(&mut self) {
        self.frame_began = true;
        let charge = core::mem::replace(&mut self.pending_frame_charge, FrameCharge::NONE);
        self.tracker.push(Lane::empty(charge.on_lane.saturating_add(charge.caller)));
    }

    /// Pops the lane of the frame `result` returns from: a success merges it into the caller's,
    /// a failure discards it. Under a latch the result is first rewritten to the latched stop,
    /// whatever produced it (an interceptor, an inspector's rewrite), so no success passes it.
    ///
    /// Returns the history gas the caller paid for records this frame did not keep, which the
    /// caller gets back once the frame has merged into it.
    #[must_use = "the history of the records the frame did not keep goes back to its caller"]
    pub(crate) fn on_frame_return(&mut self, result: &mut FrameResult) -> u64 {
        self.apply_latch(result);
        // A charge an opcode of this frame made for a child that never started died with that
        // opcode, which failed after making it.
        self.pending_frame_charge = FrameCharge::NONE;
        let success = result.instruction_result().is_ok();
        self.tracker.pop(success).map_or(0, |lane| lane.history_refund(success))
    }

    /// Settles the transaction's outermost frame: pops its lane unless the frame already
    /// returned through [`on_frame_return`](Self::on_frame_return), which is the case whenever
    /// it ran, and rewrites the result to the latched stop.
    ///
    /// A transaction whose first frame never reached frame init ran out of gas in the runtime
    /// phase before it: the out-of-gas took back the authorizations applied before it, so their
    /// records and any latch they set go too, and the halt stays a halt.
    pub(crate) fn on_last_frame_return(&mut self, result: &mut FrameResult) {
        if !self.frame_began {
            self.tracker.reset();
            self.latched = None;
            return;
        }
        debug_assert!(self.tracker.depth() <= 1, "only the outermost lane can be left");
        if self.tracker.depth() == 1 {
            // The outermost frame has no caller that paid for its records: the transaction did,
            // before execution, and the settlement gives that charge back.
            let refund = self.on_frame_return(result);
            debug_assert_eq!(refund, 0, "the outermost frame's caller is the transaction");
        } else {
            self.apply_latch(result);
        }
    }
}

/// Whether the frame `inputs` starts is granted a history allowance: a value-transferring `CALL`
/// or `CALLCODE` below the transaction's own frame.
///
/// `DELEGATECALL` and `STATICCALL` carry no value, and the transaction's own frame is not a call
/// anybody made — its sender chose the gas limit.
fn grants_stipend(inputs: &CallInputs) -> bool {
    matches!(inputs.scheme, CallScheme::Call | CallScheme::CallCode) && inputs.transfers_value()
}

/// The write records a frame's start makes, split by whose failure takes them back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameStartRecords {
    /// Records on the frame's own lane, which its failure discards.
    pub(crate) on_lane: u64,
    /// Whether the frame's start also writes its caller's account.
    pub(crate) caller: bool,
}

impl FrameStartRecords {
    /// No record at all.
    pub(crate) const NONE: Self = Self { on_lane: 0, caller: false };

    /// The number of records, whoever keeps them.
    pub(crate) const fn total(self) -> u64 {
        self.on_lane.saturating_add(self.caller as u64)
    }
}

/// The history gas a caller paid for the records the frame it starts makes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FrameCharge {
    /// What the records on the frame's own lane cost.
    on_lane: u64,
    /// What the record of the caller's own account cost.
    caller: u64,
}

impl FrameCharge {
    /// Nothing charged.
    const NONE: Self = Self { on_lane: 0, caller: 0 };
}
