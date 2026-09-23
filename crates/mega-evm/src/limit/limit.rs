//! The per-transaction state of the common execution layer.

use alloy_primitives::Address;
use revm::{
    handler::FrameResult,
    interpreter::{CallInputs, CallScheme, FrameInput, InstructionResult, InterpreterResult},
};

use super::{
    frame_limit::{FrameLimitTracker, Lane},
    record::{HistoryBytes, RecordEffect, StagedRecord},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, WRITE_RECORD,
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
    /// The stop the frame a child returned into must return instead of running on, when what the
    /// child left it put it over a limit. See [`on_frame_return`](Self::on_frame_return).
    resume_stop: Option<LimitCheck>,
    /// Whether the transaction's call target is an applied EIP-7702 authority, whose account
    /// write the transaction's lane already counts.
    target_is_authority: bool,
    /// The transaction's sender, whose account write is part of the transaction body: a frame
    /// running as the sender never records it.
    sender: Address,
    /// Whether the transaction's first frame reached frame init. When it did not, the runtime
    /// phase before it ran out of gas and took back the authorizations applied before it. The
    /// body stays; see [`on_last_frame_return`](Self::on_last_frame_return).
    frame_began: bool,
    /// Data-size bytes of the transaction's body. Recorded before any frame and kept on every
    /// path, including an out-of-gas before the first frame, which clears the rest of the tracker.
    body_bytes: u64,
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
        self.resume_stop = None;
        self.target_is_authority = false;
        self.sender = Address::ZERO;
        self.frame_began = false;
        self.body_bytes = 0;
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

    /// The stop the running frame returns instead of running another instruction: the latched
    /// one, or the one [`on_frame_return`](Self::on_frame_return) left for a caller its child put
    /// over its budget. Taking it clears the latter, which stops one frame.
    pub(crate) fn stop_before_run(&mut self) -> Option<LimitCheck> {
        let resume_stop = self.resume_stop.take();
        self.latched.or(resume_stop)
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
    /// the record of the caller's own account, and `records`, the answer the charge was computed
    /// from, for [`push_lane`](Self::push_lane) to check its own against.
    ///
    /// The next lane pushed takes it, whether that is the frame's own or the empty one of a frame
    /// answered without running.
    #[inline]
    pub(crate) const fn stage_frame_charge(
        &mut self,
        records: FrameStartRecords,
        on_lane: u64,
        caller: u64,
    ) {
        self.pending_frame_charge = FrameCharge { records: Some(records), on_lane, caller };
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

    /// Counts the transaction's body: its envelope, the writes its inclusion makes, its calldata,
    /// its authorizations and its access list.
    ///
    /// The bytes are the transaction's, recorded before any frame, so a revert does not take them
    /// back, and neither does an out-of-gas before the first frame. A body that crosses the
    /// transaction limit latches it, and the first frame is then answered with the stop.
    pub(crate) fn record_tx_body(&mut self, bytes: u64) -> LimitCheck {
        self.body_bytes = bytes;
        if bytes == 0 {
            return LimitCheck::WithinLimit;
        }
        self.tracker.record_tx(LimitUsage { data_size: bytes, write_records: 0 });
        self.check()
    }

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

    /// Counts the code a creation is about to deposit, on the creation's own lane, and turns the
    /// return into the stop when that crosses a limit.
    ///
    /// Called from the frame run, on a successful return, before revm's `return_create` commits
    /// the creation's journal checkpoint. A rewrite after that commit would leave the code
    /// deployed: the checkpoint is already gone, and flipping the frame result does not reopen
    /// it. A stop here makes `return_create` revert the checkpoint instead, so the code is not
    /// written. The bytes stay on the lane until the frame returns; a success merges them into
    /// the caller, and the failure — the stop included — discards them.
    ///
    /// A return that is already a revert or a halt deposits nothing, and its output is the
    /// revert data, not code. Empty code deposits nothing either.
    pub(crate) fn on_create_return(&mut self, result: &mut InterpreterResult) {
        if !result.result.is_ok() {
            return;
        }
        let bytes = result.output.len() as u64;
        if bytes == 0 {
            return;
        }
        debug_assert!(self.tracker.current().is_some(), "a creation returns on its own lane");
        self.tracker.record(LimitUsage { data_size: bytes, write_records: 0 });
        let check = self.check();
        if !check.exceeded_limit() {
            return;
        }
        result.result = InstructionResult::Revert;
        result.output = check.revert_data();
    }

    /// The data-size budget of the frame about to start.
    ///
    /// The transaction's own frame gets what the transaction has left, and never more than
    /// [`frame_data_size_limit`](EvmTxRuntimeLimits::frame_data_size_limit). A child gets
    /// [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`] of what its parent has
    /// left, under the same cap. What the parent has left is its budget minus what it has already
    /// kept, so a parent that has spent part of its budget forwards a smaller share.
    fn frame_budget(&self) -> u64 {
        let forwarded = match self.tracker.current() {
            Some(caller) => share_of_remaining(caller.remaining_budget()),
            None => self.limits.tx_data_size_limit.saturating_sub(self.tracker.net().data_size),
        };
        forwarded.min(self.limits.frame_data_size_limit)
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
        let budget = self.frame_budget();
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
                let records = self.records_the_caller_paid_for(input, charge.records);
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
                self.tracker.record(WRITE_RECORD.times(records.on_lane));
                if grants_stipend(inputs) {
                    self.tracker.grant_stipend(storage_call_stipend());
                }
            }
            FrameInput::Create(inputs) => {
                let records = self.records_the_caller_paid_for(input, charge.records);
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

    /// The records starting `input` makes, checked against the answer the caller's charge was
    /// computed from.
    ///
    /// The two are separate answers to the same question, asked at two points: the charge at the
    /// opcode, on the input revm's instruction built, and the count here, on the input that
    /// survived interception and the keyless rewrite. They agree because the rewrite is the
    /// identity today. The mechanism that makes it rewrite a call into a creation — native
    /// keyless deployment — changes which records a frame's start makes, and must reconcile the
    /// charge with them; until it does, a divergence trips here in every debug build rather than
    /// mis-charging the caller and mis-splitting the refund its failure gets back.
    fn records_the_caller_paid_for(
        &self,
        input: &FrameInput,
        charged: Option<FrameStartRecords>,
    ) -> FrameStartRecords {
        let records = self.frame_start_records(input);
        if let Some(charged) = charged {
            debug_assert_eq!(
                charged, records,
                "the records a frame's start makes changed after its caller was charged for them",
            );
        }
        records
    }

    /// The write records starting `input` makes at a depth above the transaction's own frame, for
    /// the opcode that starts it: `on_lane` are the records the frame's failure discards, `caller`
    /// whether its start also writes the caller's own account.
    ///
    /// The caller pays for both at the opcode, before it forwards gas, so the frame's own budget
    /// carries none of them — which is what lets a value transfer's allowance pay for what the
    /// recipient does with it.
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
    /// Then the caller is held to its limits with what it now holds, and a crossing is the stop
    /// it returns before it runs on ([`stop_before_run`](Self::stop_before_run)): its own
    /// frame-local revert for its budget, the latch for the transaction's limit. One return adds
    /// to a caller what no check has held it to: the nonce record a failed creation leaves its
    /// creator. The creation counted that record on its own lane, against its own share, and a
    /// creation stopped for crossing that share still bumps the nonce — so a creator with fewer
    /// bytes left than a record would keep one it may not. Any other return leaves the caller
    /// within its limits: a failure hands it nothing, and a success hands it no more than the
    /// share it gave.
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
        let refund = self.tracker.pop(success).map_or(0, |lane| lane.history_refund(success));
        let check = self.check();
        self.resume_stop = check.exceeded_limit().then_some(check);
        refund
    }

    /// Settles the transaction's outermost frame: pops its lane unless the frame already
    /// returned through [`on_frame_return`](Self::on_frame_return), which is the case whenever
    /// it ran, and rewrites the result to the latched stop.
    ///
    /// A transaction whose first frame never reached frame init ran out of gas in the runtime
    /// phase before it: the out-of-gas took back the authorizations applied before it, so their
    /// records go too, and the halt stays a halt. The body is not one of those records. It is put
    /// back after the reset.
    ///
    /// A latch goes with the reset. Only the body's can be set by then: authorities whose records
    /// cross the limit are taken back before anything else is charged, and a latched transaction's
    /// runtime phase charges nothing but the account a deposit-like transaction creates for its
    /// caller. That account exists whatever the transaction does, so a transaction that cannot pay
    /// for it is out of gas with or without a stop, and it reports the halt.
    pub(crate) fn on_last_frame_return(&mut self, result: &mut FrameResult) {
        if !self.frame_began {
            let body = self.body_bytes;
            self.tracker.reset();
            self.latched = None;
            self.tracker.record_tx(LimitUsage { data_size: body, write_records: 0 });
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

/// `remaining` × [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`].
///
/// The product is taken in `u128`, so a remaining budget near `u64::MAX` does not wrap.
const fn share_of_remaining(remaining: u64) -> u64 {
    let remaining = remaining as u128;
    let numerator = FRAME_DATA_SHARE_NUMERATOR as u128;
    let denominator = FRAME_DATA_SHARE_DENOMINATOR as u128;
    ((remaining * numerator) / denominator) as u64
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
}

/// The history gas a caller paid for the records the frame it starts makes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FrameCharge {
    /// The records the charge was computed from, when an opcode made one. The transaction's own
    /// frame has no such opcode, and neither has a frame of a transaction that prices no history.
    records: Option<FrameStartRecords>,
    /// What the records on the frame's own lane cost.
    on_lane: u64,
    /// What the record of the caller's own account cost.
    caller: u64,
}

impl FrameCharge {
    /// Nothing charged.
    const NONE: Self = Self { records: None, on_lane: 0, caller: 0 };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WRITE_RECORD_SIZE;
    use alloy_primitives::{address, Address, Bytes, U256};
    use revm::interpreter::{CallInput, CallValue};

    const SENDER: Address = address!("00000000000000000000000000000000000f0001");
    const CALLEE: Address = address!("00000000000000000000000000000000000f0002");
    const TARGET: Address = address!("00000000000000000000000000000000000f0003");

    fn call_inputs(scheme: CallScheme, value: U256) -> CallInputs {
        CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            bytecode_address: Address::ZERO,
            known_bytecode: Default::default(),
            target_address: Address::ZERO,
            caller: Address::ZERO,
            value: CallValue::Transfer(value),
            scheme,
            is_static: false,
            reservoir: 0,
            charged_new_account_state_gas: false,
        }
    }

    /// A `CALL` from `caller` to `target` carrying `value`.
    fn call_from_to(caller: Address, target: Address, value: U256) -> FrameInput {
        FrameInput::Call(Box::new(CallInputs {
            caller,
            target_address: target,
            ..call_inputs(CallScheme::Call, value)
        }))
    }

    /// A layer with the transaction's own frame started: `SENDER` calling `CALLEE`, which has
    /// recorded nothing of its own yet.
    fn with_the_transactions_frame() -> AdditionalLimit {
        let mut limit = AdditionalLimit::default();
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        limit
    }

    /// The records a frame's start makes are counted from the answer its caller was charged for:
    /// a value transfer records the caller's account and the recipient's, the two the charge was
    /// computed from.
    #[test]
    fn test_the_records_counted_are_the_records_the_caller_was_charged_for() {
        let mut limit = with_the_transactions_frame();
        let inner = call_from_to(CALLEE, TARGET, U256::from(1));
        let records = limit.frame_start_records(&inner);
        assert_eq!(records, FrameStartRecords { on_lane: 1, caller: true });

        limit.stage_frame_charge(records, 1, 1);
        limit.on_frame_init(&inner, 1);
        assert_eq!(
            limit.usage(),
            LimitUsage { data_size: 2 * WRITE_RECORD_SIZE, write_records: 2 },
        );
    }

    /// A charge computed from a different answer than the count is what a rewrite that turns one
    /// kind of frame into another would make. It trips rather than passing silently, because the
    /// charge and the refund its failure gets back would both be wrong.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "changed after its caller was charged for them")]
    fn test_a_charge_computed_from_another_answer_trips() {
        let mut limit = with_the_transactions_frame();
        limit.stage_frame_charge(FrameStartRecords::NONE, 0, 0);
        limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::from(1)), 1);
    }

    /// A child frame's budget is 98% of what its parent has left, three frames down, and the
    /// transaction's own frame gets what the transaction has left. A configured frame cap binds
    /// when it is the smaller of the two.
    #[test]
    fn test_a_child_frame_gets_98_percent_of_what_its_parent_has_left() {
        let tx_limit = 10_000;
        let mut limit =
            AdditionalLimit::new(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(tx_limit));
        limit.tracker.record_tx(LimitUsage { data_size: 310, write_records: 0 });

        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        assert_eq!(limit.tracker.current().unwrap().budget, tx_limit - 310);

        limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::ZERO), 1);
        let child = share_of_remaining(tx_limit - 310);
        assert_eq!(limit.tracker.current().unwrap().budget, child);

        limit.on_frame_init(&call_from_to(TARGET, SENDER, U256::ZERO), 2);
        let grandchild = share_of_remaining(child);
        assert_eq!(limit.tracker.current().unwrap().budget, grandchild);

        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 3);
        assert_eq!(
            limit.tracker.current().unwrap().budget,
            share_of_remaining(grandchild),
            "the fourth frame, at depth 3, still takes 98% of what is left"
        );
    }

    /// The frame cap binds when it is tighter than the share of what the parent has left.
    #[test]
    fn test_the_frame_cap_binds_when_it_is_tighter_than_the_share() {
        let mut limit = AdditionalLimit::new(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_data_size_limit(u64::MAX)
                .with_frame_data_size_limit(100),
        );
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        assert_eq!(limit.tracker.current().unwrap().budget, 100);
        limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::ZERO), 1);
        assert_eq!(limit.tracker.current().unwrap().budget, 98);
    }

    /// The allowance follows the transfer, and only the two schemes that can carry one: a `CALL`
    /// or a `CALLCODE` that moves value is granted one, and nothing else is — a valueless call of
    /// either scheme, and `DELEGATECALL` and `STATICCALL`, which take no value word at all.
    #[test]
    fn test_only_a_value_call_or_callcode_is_granted_an_allowance() {
        for scheme in [
            CallScheme::Call,
            CallScheme::CallCode,
            CallScheme::DelegateCall,
            CallScheme::StaticCall,
        ] {
            let carries_value = matches!(scheme, CallScheme::Call | CallScheme::CallCode);
            assert_eq!(
                grants_stipend(&call_inputs(scheme, U256::from(1))),
                carries_value,
                "{scheme:?} with value",
            );
            assert!(!grants_stipend(&call_inputs(scheme, U256::ZERO)), "{scheme:?} without value");
        }
    }
}
