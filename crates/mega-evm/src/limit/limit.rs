//! The per-transaction state of the common execution layer.

use alloy_primitives::{Address, Bytes};
use revm::{
    context_interface::context::CodeDeposit,
    handler::FrameResult,
    interpreter::{CallInputs, CallScheme, FrameInput, InstructionResult},
};

use super::{
    frame_limit::{FrameLimitTracker, Lane},
    record::{HistoryBytes, RecordEffect, StagedRecord},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, TRANSFER_LOG, WRITE_RECORD, WRITE_RECORD_SIZE,
};
use crate::storage_call_stipend;

/// What the common execution layer tracks for the running transaction: the per-frame lanes of
/// data-size bytes and write records, the state gas the transaction holds outside each frame, the
/// record the Host staged for the running opcode, and where the transaction stands with its
/// limits — within them or latched by one — and whether it is exempt from all of them.
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
///
/// # The exemption
///
/// The protocol's own work is held to none of the per-transaction limits: a system-originated
/// transaction and a system call are marked exempt before their body is counted, and the mark is
/// sticky for the transaction ([`is_exempt`](Self::is_exempt)). Every stop the layer hands out
/// comes from one place, which answers an exempt transaction with [`LimitCheck::WithinLimit`]
/// whatever it crossed — a transaction limit or a frame budget, in any dimension — and never
/// latches it.
/// What the transaction uses is counted all the same, so its usage is reported as any other
/// transaction's is.
#[derive(Clone, Debug, Default)]
pub struct AdditionalLimit {
    pub(crate) tracker: FrameLimitTracker,
    staged: Option<StagedRecord>,
    limits: EvmTxRuntimeLimits,
    /// Where the transaction stands with its limits: within them, or stopped by the
    /// transaction-level limit it latched, which is sticky for the transaction.
    standing: LimitCheck,
    /// Whether the transaction is exempt from every per-transaction limit, sticky for the
    /// transaction. See [`exempt`](Self::exempt).
    exempt: bool,
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
    /// The bytes of the transaction's body that
    /// [`intrinsic_history_gas`](Self::intrinsic_history_gas) paid for, part of
    /// [`history_bytes`](Self::history_bytes).
    intrinsic_history_bytes: u64,
    /// The history gas of the write record the transaction's own frame makes — its value's
    /// recipient, or the account it creates — charged before the first frame and given back when
    /// that frame fails.
    top_level_write_record_gas: u64,
    /// The history gas the running opcode charged its own frame for the records the frame it is
    /// starting will make, waiting for that frame's lane to be pushed.
    pending_frame_charge: FrameCharge,
    /// Whether revm refuses, on its caller's account, the start of the frame the last
    /// frame-starting opcode suspended on — or, before the first frame, the transaction's own
    /// frame — as that opcode, or the charge of the transaction's record, found it. See
    /// [`stage_start_refused`](Self::stage_start_refused).
    pending_start_refused: Option<bool>,
    /// The history gas the settled transaction spent.
    history_gas_spent: u64,
    /// The history bytes the settled transaction appended.
    history_bytes: u64,
    /// Whether the running transaction's value movements journal EIP-7708 transfer logs, which
    /// revm emits itself and the data size counts where the value moves.
    transfer_logs: bool,
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
        self.standing = LimitCheck::WithinLimit;
        self.exempt = false;
        self.resume_stop = None;
        self.target_is_authority = false;
        self.sender = Address::ZERO;
        self.frame_began = false;
        self.body_bytes = 0;
        self.intrinsic_history_gas = 0;
        self.intrinsic_history_bytes = 0;
        self.top_level_write_record_gas = 0;
        self.pending_frame_charge = FrameCharge::NONE;
        self.pending_start_refused = None;
        self.history_gas_spent = 0;
        self.history_bytes = 0;
        self.transfer_logs = false;
    }

    /// Records whether the running transaction's value movements journal EIP-7708 transfer logs.
    /// Called for every transaction, after the reset, from the configuration revm emits them by.
    pub(crate) const fn set_transfer_logs(&mut self, emitted: bool) {
        self.transfer_logs = emitted;
    }

    /// Whether starting `input` journals an EIP-7708 transfer log in the running transaction.
    /// See [`moves_value`].
    pub(crate) fn frame_start_transfer_log(&self, input: &FrameInput) -> bool {
        self.transfer_logs && moves_value(input)
    }

    /* The latch and the exemption */

    /// The stop a transaction-level limit latched, if any.
    pub const fn latched(&self) -> Option<&LimitCheck> {
        match &self.standing {
            latched @ LimitCheck::ExceedsLimit { .. } => Some(latched),
            LimitCheck::WithinLimit => None,
        }
    }

    /// Whether the running transaction is exempt from every per-transaction limit.
    pub const fn is_exempt(&self) -> bool {
        self.exempt
    }

    /// Exempts the running transaction from every per-transaction limit, until the next
    /// transaction resets the layer.
    ///
    /// Called for the protocol's own work — a system-originated transaction and a system call —
    /// before its body is counted. See the type's documentation.
    pub(crate) const fn exempt(&mut self) {
        self.exempt = true;
    }

    /// Latches the transaction: `kind`'s transaction-level `limit` was crossed at `used`. The
    /// running frame must stop with [`LimitCheck::revert_data`]; the frame lifecycle stops every
    /// frame above it. A later latch does not replace the first, and an exempt transaction is not
    /// latched at all: the verdict is then [`LimitCheck::WithinLimit`].
    pub fn latch(&mut self, kind: LimitKind, limit: u64, used: u64) -> LimitCheck {
        self.crossed(kind, limit, used, false)
    }

    /// The verdict on `used` crossing `kind`'s `limit`: a frame budget's stop when `frame_local`,
    /// which reverts the running frame alone, and the transaction's latch otherwise.
    ///
    /// Every stop the layer hands out comes from here, so this is where the exemption applies: an
    /// exempt transaction's verdict is [`LimitCheck::WithinLimit`], whatever it crossed.
    fn crossed(&mut self, kind: LimitKind, limit: u64, used: u64, frame_local: bool) -> LimitCheck {
        if self.exempt {
            return LimitCheck::WithinLimit;
        }
        let crossed = LimitCheck::ExceedsLimit { kind, limit, used, frame_local };
        if frame_local {
            return crossed;
        }
        if !self.standing.exceeded_limit() {
            self.standing = crossed;
        }
        self.standing
    }

    /// Checks the limits after the running frame counted something: what the transaction keeps
    /// against its limits (latching on a crossing), then what the running frame keeps against its
    /// budget. In each, data size comes before the write records.
    fn check(&mut self) -> LimitCheck {
        if let Some((kind, limit, used)) = self.tracker.net().crossing(self.limits.tx_usage_limit())
        {
            return self.latch(kind, limit, used);
        }
        if let Some(lane) = self.tracker.current() {
            if let Some((kind, limit, used)) = lane.net().crossing(lane.budget) {
                return self.crossed(kind, limit, used, true);
            }
        }
        LimitCheck::WithinLimit
    }

    /// Whether [`check`](Self::check) would find nothing to stop the running frame for, asked
    /// without latching anything: the transaction keeps no more than its limits, and the running
    /// frame no more than its budget. An exempt transaction is stopped by nothing, and a latched
    /// one by nothing but the latch, which [`stop_before_run`](Self::stop_before_run) takes
    /// first.
    fn check_finds_nothing(&self) -> bool {
        if self.is_exempt() || self.latched().is_some() {
            return true;
        }
        self.tracker.net().crossing(self.limits.tx_usage_limit()).is_none() &&
            self.tracker.current().is_none_or(|lane| lane.net().crossing(lane.budget).is_none())
    }

    /// The stop the running frame returns instead of running another instruction: the latched
    /// one, or the one [`on_frame_return`](Self::on_frame_return) left for a caller its child put
    /// over its budget. Taking it clears the latter, which stops one frame.
    pub(crate) fn stop_before_run(&mut self) -> Option<LimitCheck> {
        let resume_stop = self.resume_stop.take();
        self.latched().copied().or(resume_stop)
    }

    /// Rewrites `result` to the latched stop, when the transaction is latched: a success, a revert
    /// or a halt becomes the latched revert, on the gas the result carries.
    ///
    /// No frame runs an instruction once the transaction is latched, so no out-of-gas can happen
    /// after the latch: the frame that crossed returns the stop, and every frame above it returns
    /// it again without running. A halt that reaches here under the latch was made by an
    /// inspector's rewrite, which cannot turn the stop into a halt any more than into a success.
    /// A real halt before the latch is left alone: it was returned before the latch was set.
    pub(crate) fn apply_latch(&self, result: &mut FrameResult) {
        let Some(latched) = self.latched() else { return };
        let interpreter_result = result.interpreter_result_mut();
        interpreter_result.result = InstructionResult::Revert;
        interpreter_result.output = latched.revert_data();
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

    /// Records the history gas validation charged for the transaction's body, and the `bytes` of
    /// the body it paid for.
    pub(crate) const fn set_intrinsic_history(&mut self, gas: u64, bytes: u64) {
        self.intrinsic_history_gas = gas;
        self.intrinsic_history_bytes = bytes;
    }

    /// The history bytes the last settled transaction appended, whoever paid for them: its body,
    /// one write record per account or storage write it kept, the logs it emitted and kept and
    /// the code it deposited. The EIP-7708 transfer logs of its value movements are data size and
    /// not history, and are not among them.
    ///
    /// Every one of them is priced at the cost per history byte, and
    /// [`history_gas_spent`](Self::history_gas_spent) is what the transaction's own gas paid of
    /// that price. The two part by what the history allowances of its value transfers paid, which
    /// no gas ledger carries. They are the bytes the schedule prices, not the chain's physical
    /// growth: a transaction exempt from history gas appended none that anybody priced, and
    /// reports none, and the body counts its fixed write records whichever fee accounts are
    /// written.
    pub const fn history_bytes(&self) -> u64 {
        self.history_bytes
    }

    /// Settles [`history_bytes`](Self::history_bytes) from what the transaction kept: `priced` is
    /// whether it pays history at all. Called once the outermost frame's lane is popped, when
    /// everything the transaction kept sits on its own lane.
    pub(crate) fn settle_history_bytes(&mut self, priced: bool) {
        self.history_bytes = if priced {
            self.intrinsic_history_bytes
                .saturating_add(WRITE_RECORD_SIZE.saturating_mul(self.tracker.net().write_records))
                .saturating_add(self.tracker.log_and_code_bytes())
        } else {
            0
        };
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

    /// Leaves whether revm refuses the start of the frame about to start on its caller's account,
    /// for the frame's init to take ([`take_start_refused`](Self::take_start_refused)) rather than
    /// read the caller's account again: found by the opcode starting the frame, or, for the
    /// transaction's own frame, by the charge of its record before execution.
    ///
    /// Only a start revm can refuse there — a call that transfers value, a creation — takes it.
    /// One an opcode makes finds the answer that opcode staged, which stages one for every frame
    /// it starts and so replaces whatever an earlier start left. The transaction's own frame finds
    /// the one its record's charge staged, or none: the reset cleared what the transaction before
    /// it left. A start answered before its init leaves its answer for no other start to take.
    /// The one start past the first frame no opcode makes, a keyless deployment's creation, is
    /// cleared for ([`set_frame_creator`](Self::set_frame_creator)), and so is every start on the
    /// inspected path, whose inspector may rewrite the input.
    #[inline]
    pub(crate) const fn stage_start_refused(&mut self, refused: bool) {
        self.pending_start_refused = Some(refused);
    }

    /// Takes what [`stage_start_refused`](Self::stage_start_refused) left for the frame about to
    /// start, if anything.
    #[inline]
    pub(crate) const fn take_start_refused(&mut self) -> Option<bool> {
        self.pending_start_refused.take()
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
    ///
    /// A `SELFDESTRUCT` that moved value to another account also counts the EIP-7708 transfer log
    /// revm journaled for the move, with the beneficiary's record and in the same check. The log
    /// is the running frame's, as the move is: the frame's own journal checkpoint takes both back
    /// when the frame fails, and its lane the bytes. It is data size and nothing else — no write
    /// record, and no history: the bytes reported are the record's alone.
    #[inline]
    pub(crate) fn commit_staged_record(&mut self) -> (LimitCheck, HistoryBytes) {
        let Some(record) = self.staged.take() else {
            return (LimitCheck::WithinLimit, HistoryBytes::None);
        };
        let effect = record.effect(self.sender);
        let history = effect.history_bytes();
        // A log's bytes are history beside the write records; a record's are counted as one.
        if let (StagedRecord::Log { .. }, HistoryBytes::Appended(bytes)) = (&record, history) {
            self.tracker.record_log_and_code_bytes(bytes);
        }
        let counted = match effect {
            RecordEffect::None => LimitUsage::ZERO,
            RecordEffect::Record(usage) => usage,
            RecordEffect::Refund(usage) => {
                self.tracker.refund(usage);
                return (LimitCheck::WithinLimit, history);
            }
        };
        let counted = if self.transfer_logs && record.moves_value() {
            counted.saturating_add(TRANSFER_LOG)
        } else {
            counted
        };
        if counted == LimitUsage::ZERO {
            return (LimitCheck::WithinLimit, history);
        }
        self.tracker.record(counted);
        (self.check(), history)
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
    /// one of them, from the transaction's `sender`, which has been charged `state_gas` so far.
    ///
    /// The limits are checked before the records are made — the state gas the authorities were
    /// charged first, then their records: a crossing latches the transaction and records nothing,
    /// and the caller takes the authorizations back with the gas they charged, so the writes the
    /// limit guards never happen. The first frame is then answered with the stop without running.
    pub(crate) fn record_applied_authorities(
        &mut self,
        sender: Address,
        authorities: u64,
        target_is_authority: bool,
        state_gas: i64,
    ) -> LimitCheck {
        self.sender = sender;
        let check = self.check_state_gas(state_gas);
        if check.exceeded_limit() {
            return check;
        }
        let records = WRITE_RECORD.times(authorities);
        let used = self.tracker.net().saturating_add(records);
        if let Some((kind, limit, used)) = used.crossing(self.limits.tx_usage_limit()) {
            let check = self.latch(kind, limit, used);
            if check.exceeded_limit() {
                return check;
            }
        }
        self.target_is_authority = target_is_authority;
        self.tracker.record(records);
        LimitCheck::WithinLimit
    }

    /* The state-gas limit */

    /// Holds the state gas the transaction holds to its limit, the running frame — or, outside
    /// any frame, the transaction — holding `running`, and latches the transaction when it is
    /// crossed.
    ///
    /// Called where state gas has just been charged, so a charge the frame could not pay is an
    /// out-of-gas whatever the limit: the limit holds what was paid. What a frame refilled, and
    /// what a failed frame rolled back, is out of what it holds, so a write taken back gives its
    /// room back.
    pub(crate) fn check_state_gas(&mut self, running: i64) -> LimitCheck {
        let used = self.tracker.state_gas_held(running);
        let limit = self.limits.tx_state_gas_limit;
        if used > limit {
            return self.latch(LimitKind::StateGrowth, limit, used);
        }
        LimitCheck::WithinLimit
    }

    /// Records the state gas the transaction was charged before its first frame, `spent`, and
    /// holds it to the limit. A crossing latches the transaction, and its first frame is answered
    /// with the stop before it is built.
    pub(crate) fn on_state_gas_before_frames(&mut self, spent: i64) {
        self.tracker.set_state_gas_before_frames(spent);
        self.check_state_gas(spent);
    }

    /// Records the state gas `held` by the running frame as it suspends on a child, which the
    /// child's lane counts as held outside it.
    #[inline]
    pub(crate) fn on_frame_suspend(&mut self, held: i64) {
        self.tracker.set_state_gas_at_suspension(held);
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
    /// A frame whose start moves value to another account also counts the EIP-7708 transfer log
    /// revm journals for the move, on the frame's own lane and in the same check as its records.
    /// revm moves the value, and emits the log, only once it builds the frame, inside the journal
    /// checkpoint it makes for it; the count comes before that, so a crossing answers the frame
    /// with the stop before any value moves. After the checkpoint it could not: a frame revm
    /// answers without running — a call to an account with no code, a precompile — has committed
    /// its checkpoint by the time its answer returns, and rewriting the answer would not take the
    /// move back. The lane follows the checkpoint from then on: a frame that fails discards both
    /// the log and its bytes.
    ///
    /// So the count is a prediction of what revm will do, and it is called only for a start revm
    /// makes as far as the caller's account decides it. A start revm refuses there — a value its
    /// caller cannot fund, a creation whose creator's nonce cannot be bumped — moves and writes
    /// nothing, and gets an empty lane instead ([`push_empty_frame`](Self::push_empty_frame)); so
    /// does a start past the call-stack limit, which the depth guard answers before it is counted.
    /// The one refusal decided after the count is a creation onto an occupied address: revm reads
    /// the created address's account only once it builds the frame. Its records and transfer log
    /// are counted, a crossing they cause stops the creation before revm could refuse it, and
    /// without a crossing revm's refusal fails the creation and discards them.
    ///
    /// A crossed limit in the verdict means the frame must not run: it is answered with the stop.
    /// So is every frame of a latched transaction, whose lane stays empty.
    pub(crate) fn on_frame_init(&mut self, input: &FrameInput, depth: usize) -> LimitCheck {
        self.frame_began = true;
        if let Some(latched) = self.latched().copied() {
            self.push_empty_frame();
            return latched;
        }
        self.push_lane(input, depth);
        if self.frame_start_transfer_log(input) {
            self.tracker.record(TRANSFER_LOG);
        }
        self.check()
    }

    /// Holds the code a creation deposits to the limits, and names the stop when it crosses one:
    /// first the state gas `return_create` charged for the bytes, then the bytes themselves,
    /// counted on the creation's own lane.
    ///
    /// Called through
    /// [`ContextTr::admit_code_deposit`](revm::context::ContextTr::admit_code_deposit), which
    /// `return_create` asks once it decided to deposit the code — the code passed every check
    /// and every deposit charge is recorded — and before it commits the creation's journal
    /// checkpoint. A stop refuses the deposit: `return_create` reverts the checkpoint, so the code
    /// is not written, puts the frame's gas back to what it had before the deposit's charges, and
    /// ends the creation as a revert carrying the stop. The bytes stay on the lane until the frame
    /// returns; a success merges them into the caller, and the failure — the stop included —
    /// discards them.
    ///
    /// A creation `return_create` fails itself is never offered: a revert or a halt of the init
    /// code, code it refuses, and a creation that cannot pay one of the deposit's charges, which
    /// runs out of gas whatever the limit. Empty code deposits nothing and is held to nothing.
    ///
    /// The state gas is held with what the creation already holds, and before the bytes, as it is
    /// wherever both cross at one site. A deposit the schedule charges no state gas for adds none,
    /// and holding what the creation already held finds nothing its own charges did not. The same
    /// bytes are history beside the write records, counted here on the same lane, so the
    /// history a transaction reports it appended and the data size it kept move together: a
    /// creation that deposits nothing, or whose deposit fails, appends neither.
    pub(crate) fn on_code_deposit(&mut self, deposit: &CodeDeposit<'_>) -> Result<(), Bytes> {
        let bytes = deposit.code.len() as u64;
        if bytes == 0 {
            return Ok(());
        }
        let check = self.check_state_gas(deposit.gas_after.state_gas_spent());
        if check.exceeded_limit() {
            return Err(check.revert_data());
        }
        debug_assert!(self.tracker.current().is_some(), "a creation returns on its own lane");
        self.tracker.record(LimitUsage { data_size: bytes, write_records: 0 });
        self.tracker.record_log_and_code_bytes(bytes);
        let check = self.check();
        if check.exceeded_limit() {
            return Err(check.revert_data());
        }
        Ok(())
    }

    /// The budget of the frame about to start, in data-size bytes and in write records.
    ///
    /// In each dimension the transaction's own frame gets what the transaction has left, and never
    /// more than the frame cap
    /// ([`frame_data_size_limit`](EvmTxRuntimeLimits::frame_data_size_limit),
    /// [`frame_kv_update_limit`](EvmTxRuntimeLimits::frame_kv_update_limit)). A child gets
    /// [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`] of what its parent has
    /// left, under the same cap. What the parent has left is its budget minus what it has already
    /// kept, so a parent that has spent part of its budget forwards a smaller share.
    fn frame_budget(&self) -> LimitUsage {
        let forwarded = match self.tracker.current() {
            Some(caller) => {
                let remaining = caller.remaining_budget();
                LimitUsage {
                    data_size: share_of_remaining(remaining.data_size),
                    write_records: share_of_remaining(remaining.write_records),
                }
            }
            None => self.limits.tx_usage_limit().saturating_sub(self.tracker.net()),
        };
        forwarded.min(self.limits.frame_usage_limit())
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
            // No frame starts from an empty input: revm's own frame init is `unreachable!` on
            // one.
            FrameInput::Empty => unreachable!(
                "a frame input always names a call or a creation, as revm's frame init asserts"
            ),
        }
    }

    /// The records starting `input` makes, checked against the answer the caller's charge was
    /// computed from.
    ///
    /// The two are separate answers to the same question, asked at two points: the charge at the
    /// opcode, on the input revm's instruction built, or at a `keylessDeploy` call's first run, on
    /// the creation it starts; and the count here, on the input that survived interception. They
    /// agree because nothing between the two changes the input: an interceptor answers a frame or
    /// lets it start as it is, and a creation reaches none. A divergence trips here in every debug
    /// build rather than mis-charging the caller and mis-splitting the refund its failure gets
    /// back.
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

    /// Makes the running frame start its creation as `creator`: the frame of a `keylessDeploy`
    /// call, which runs no code of its own and starts the creation its signer signed.
    ///
    /// The creation's start writes the creator's nonce, so the frame's lane runs as the creator:
    /// the creation records that write as its creator's, on the frame's lane once the creation
    /// fails, unless the creator is the transaction's sender, whose account the body counts.
    ///
    /// No opcode starts the creation, so nothing staged the answer to whether its creator's
    /// account refuses it. Clearing the staged answer is defensive: the only answer staged for the
    /// call's own start is that of a call carrying value, which is answered before its frame is
    /// built and so never starts a creation. Nothing staged can reach the creation; the clear is
    /// kept so that none ever does.
    pub(crate) fn set_frame_creator(&mut self, creator: Address) {
        self.pending_start_refused = None;
        let sender = self.sender;
        if let Some(lane) = self.tracker.current_mut() {
            lane.address = Some(creator);
            lane.account_recorded = creator == sender;
        }
    }

    /// Takes back the nonce record a failed creation left its creator, when the nonce bump it
    /// stands for is taken back: the running frame is a `keylessDeploy` call whose lane runs as
    /// the signer ([`set_frame_creator`](Self::set_frame_creator)). A signer that is the
    /// transaction's sender has no such record, its account being the body's.
    ///
    /// A stop the record left the frame, by putting it over its budget, goes with it: the frame is
    /// held to its limits again without the record. Returns whether a record was taken back.
    pub(crate) fn take_back_creator_record(&mut self) -> bool {
        let sender = self.sender;
        if self.tracker.current().is_none_or(|lane| lane.address == Some(sender)) ||
            !self.tracker.take_back_own_record()
        {
            return false;
        }
        let check = self.check();
        self.resume_stop = check.exceeded_limit().then_some(check);
        true
    }

    /// Discards the lane of the frame about to return as its failure would, and stands an empty
    /// lane in for it, which gives the caller back, when it is popped, the history the failure
    /// gives back.
    ///
    /// For a frame whose journal checkpoint was reverted and whose result an inspector then
    /// rewrote into a success: nothing the frame counted was kept, so none of it reaches its
    /// caller, whatever the result says.
    pub(crate) fn discard_returning_lane(&mut self) {
        self.tracker.discard_running_lane();
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
    /// whatever produced it (an interceptor, an inspector's rewrite), so neither a success nor a
    /// halt passes it.
    ///
    /// Two returns would add to a caller what no check has held it to, and after them the caller is
    /// held to its limits with what it now holds; a crossing is the stop it returns before it runs
    /// on ([`stop_before_run`](Self::stop_before_run)): its own frame-local revert for its budget,
    /// the latch for the transaction's limit.
    ///
    /// - A failed creation leaves its creator the nonce record. The creation counted that record on
    ///   its own lane, against its own share, and a creation stopped for crossing that share still
    ///   bumps the nonce — so a creator with fewer bytes left than a record would keep one it may
    ///   not.
    /// - A success past the frame's own budget would hand the caller more than the share it gave.
    ///   No frame returns one: a frame past its budget failed, and a failure an inspector rewrites
    ///   into a success has its lane discarded before it returns
    ///   ([`discard_returning_lane`](Self::discard_returning_lane)). It is held all the same.
    ///
    /// Any other return leaves the caller within its limits, and is not checked: a failure hands
    /// it nothing, and a success hands it no more than the share it gave. Neither is the return of
    /// the transaction's own frame, which has no caller to hold. A debug build asserts that the
    /// check would have found nothing to stop.
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
        let lane = self.tracker.pop(success);
        let refund = lane.as_ref().map_or(0, |lane| lane.history_refund(success));
        let unchecked = lane.is_some_and(|lane| self.tracker.hands_unchecked(&lane, success));
        self.resume_stop = if unchecked {
            let check = self.check();
            check.exceeded_limit().then_some(check)
        } else {
            debug_assert!(
                self.check_finds_nothing(),
                "a frame's return put its caller over a limit no check held it to"
            );
            None
        };
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
    /// for it is out of gas with or without a stop, and it reports the halt. An exemption stays:
    /// it is the transaction's, not its frames'.
    pub(crate) fn on_last_frame_return(&mut self, result: &mut FrameResult) {
        if !self.frame_began {
            let body = self.body_bytes;
            self.tracker.reset();
            self.standing = LimitCheck::WithinLimit;
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

/// Whether starting `input` moves value from one account to another, which is what revm journals
/// an EIP-7708 transfer log for: a call that transfers value to an account other than its caller —
/// the transaction's own value, a value `CALL` — and a creation with an endowment, the
/// transaction's own or a `CREATE`'s or a `CREATE2`'s.
///
/// A `CALLCODE` transfers its value from the calling account to itself, as a `CALL` to itself does,
/// and revm moves nothing and emits nothing for either. `DELEGATECALL` and `STATICCALL` transfer
/// no value at all.
fn moves_value(input: &FrameInput) -> bool {
    match input {
        FrameInput::Call(inputs) => {
            inputs.transfers_value() && inputs.target_address != inputs.caller
        }
        FrameInput::Create(inputs) => !inputs.value().is_zero(),
        FrameInput::Empty => false,
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
    use crate::{TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE};
    use alloy_primitives::{address, Address, Bytes, U256};
    use revm::interpreter::{CallInput, CallValue};

    const SENDER: Address = address!("00000000000000000000000000000000000f0001");
    const CALLEE: Address = address!("00000000000000000000000000000000000f0002");
    const TARGET: Address = address!("00000000000000000000000000000000000f0003");

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

    /// A frame answered without running — by the depth guard, the latch, an interceptor or an
    /// inspector — keeps none of the records its caller paid for, whatever its answer: its lane is
    /// empty, the whole charge comes back, and the transaction's history bytes are its body alone.
    ///
    /// The depth guard's answer is reached here rather than through a transaction: under the
    /// execution cap no call chain reaches the call-stack limit, because each call forwards at most
    /// sixty-three sixty-fourths of what its caller has left, and 200,000,000 gas leaves about
    /// twenty at depth 1,024. The latch's and an interceptor's answers are also reached through
    /// transactions, in the history-byte tests.
    #[test]
    fn test_a_frame_answered_without_running_keeps_nothing_whatever_the_answer() {
        const BODY: u64 = crate::TX_BODY_SIZE;
        for answer in
            [InstructionResult::CallTooDeep, InstructionResult::Revert, InstructionResult::Stop]
        {
            let mut limit = with_the_transactions_frame();
            limit.set_intrinsic_history(1, BODY);
            let inner = call_from_to(CALLEE, TARGET, U256::from(1));
            limit.stage_frame_charge(limit.frame_start_records(&inner), 1_000, 2_000);
            limit.push_empty_frame();

            let mut result = crate::synthetic_frame_result(&inner, answer, Bytes::new());
            assert_eq!(limit.on_frame_return(&mut result), 3_000, "{answer:?}: all of it back");
            assert_eq!(limit.usage(), LimitUsage::ZERO, "{answer:?}: nothing kept");

            let outer = call_from_to(SENDER, CALLEE, U256::ZERO);
            let mut result =
                crate::synthetic_frame_result(&outer, InstructionResult::Stop, Bytes::new());
            limit.on_last_frame_return(&mut result);
            limit.settle_history_bytes(true);
            assert_eq!(limit.history_bytes(), BODY, "{answer:?}: the body alone");
        }
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
    /// transaction's own frame gets what the transaction has left — in data-size bytes and in
    /// write records alike, each from its own limit.
    #[test]
    fn test_a_child_frame_gets_98_percent_of_what_its_parent_has_left() {
        let tx_limit = LimitUsage { data_size: 10_000, write_records: 1_000 };
        let mut limit = AdditionalLimit::new(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_data_size_limit(tx_limit.data_size)
                .with_tx_kv_update_limit(tx_limit.write_records),
        );
        limit.tracker.record_tx(LimitUsage { data_size: 310, write_records: 0 });
        let share = |usage: LimitUsage| LimitUsage {
            data_size: share_of_remaining(usage.data_size),
            write_records: share_of_remaining(usage.write_records),
        };

        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let first = LimitUsage { data_size: 10_000 - 310, write_records: 1_000 };
        assert_eq!(limit.tracker.current().unwrap().budget, first);

        limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::ZERO), 1);
        let child = share(first);
        assert_eq!(child, LimitUsage { data_size: 9_496, write_records: 980 });
        assert_eq!(limit.tracker.current().unwrap().budget, child);

        limit.on_frame_init(&call_from_to(TARGET, SENDER, U256::ZERO), 2);
        let grandchild = share(child);
        assert_eq!(limit.tracker.current().unwrap().budget, grandchild);

        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 3);
        assert_eq!(
            limit.tracker.current().unwrap().budget,
            share(grandchild),
            "the fourth frame, at depth 3, still takes 98% of what is left"
        );
    }

    /// Each frame cap binds when it is tighter than the share of what the parent has left, in its
    /// own dimension and in no other.
    #[test]
    fn test_the_frame_cap_binds_when_it_is_tighter_than_the_share() {
        let mut limit = AdditionalLimit::new(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_data_size_limit(u64::MAX)
                .with_frame_data_size_limit(100)
                .with_tx_kv_update_limit(1_000),
        );
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        assert_eq!(
            limit.tracker.current().unwrap().budget,
            LimitUsage { data_size: 100, write_records: 1_000 }
        );
        limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::ZERO), 1);
        assert_eq!(
            limit.tracker.current().unwrap().budget,
            LimitUsage { data_size: 98, write_records: 980 }
        );

        let mut limit = AdditionalLimit::new(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_data_size_limit(1_000)
                .with_frame_kv_update_limit(10),
        );
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        assert_eq!(
            limit.tracker.current().unwrap().budget,
            LimitUsage { data_size: 1_000, write_records: 10 }
        );
        limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::ZERO), 1);
        assert_eq!(
            limit.tracker.current().unwrap().budget,
            LimitUsage { data_size: 980, write_records: 9 }
        );
    }

    /// A layer with a frame cap of 100 bytes and the transaction's own frame started, which is
    /// what the cap gives it; `CALLEE`'s call to `TARGET`, the child, is started below it with
    /// 98 of them.
    fn with_a_child_of_a_capped_frame() -> (AdditionalLimit, FrameInput) {
        let mut limit =
            AdditionalLimit::new(EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(100));
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let child = call_from_to(CALLEE, TARGET, U256::ZERO);
        limit.on_frame_init(&child, 1);
        assert_eq!(limit.tracker.current().unwrap().budget.data_size, 98);
        (limit, child)
    }

    /// A success is checked on its return only when it kept more than its own budget, which no
    /// frame returns and the check guards against all the same. Within its budget it hands the
    /// caller no more than the caller's share, and the caller runs on unchecked; past it, the
    /// caller is held to its own budget with what it now keeps, and stops when that crosses it.
    #[test]
    fn test_a_success_past_its_budget_holds_its_caller_to_the_caller_budget() {
        for (kept, caller_stopped) in [(98, false), (99, false), (100, false), (101, true)] {
            let (mut limit, child) = with_a_child_of_a_capped_frame();
            limit.tracker.record(LimitUsage { data_size: kept, write_records: 0 });
            let mut result =
                crate::synthetic_frame_result(&child, InstructionResult::Stop, Bytes::new());
            let _ = limit.on_frame_return(&mut result);
            let stop = limit.stop_before_run();
            let expected = caller_stopped.then_some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit: 100,
                used: kept,
                frame_local: true,
            });
            assert_eq!(stop, expected, "a child that kept {kept} bytes");
            assert_eq!(limit.usage().data_size, kept, "the success merged what it kept");
        }
    }

    /// The premise the unchecked returns rest on is asserted in a debug build. A success within a
    /// budget wider than the share its caller gave it — what a share computed wrong would give —
    /// puts the caller over its own budget with no check to catch it, and trips.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "no check held it to")]
    fn test_a_success_that_hands_its_caller_more_than_its_share_trips() {
        let (mut limit, child) = with_a_child_of_a_capped_frame();
        limit.tracker.current_mut().unwrap().budget = crate::limit::UNLIMITED;
        limit.tracker.record(LimitUsage { data_size: 101, write_records: 0 });
        let mut result =
            crate::synthetic_frame_result(&child, InstructionResult::Stop, Bytes::new());
        let _ = limit.on_frame_return(&mut result);
    }

    /// Whether a start's caller refuses it is staged by the opcode that found it and taken once,
    /// by the frame's init. Nothing is left for a later start: a reset drops what the transaction
    /// before it left, and a keyless deployment's creation, which no opcode starts, drops what was
    /// staged for its call.
    #[test]
    fn test_a_staged_start_answer_is_taken_once() {
        let mut limit = with_the_transactions_frame();
        assert_eq!(limit.take_start_refused(), None, "nothing staged");
        for refused in [false, true] {
            limit.stage_start_refused(refused);
            assert_eq!(limit.take_start_refused(), Some(refused));
            assert_eq!(limit.take_start_refused(), None, "taken once");
        }

        limit.stage_start_refused(true);
        limit.set_frame_creator(TARGET);
        assert_eq!(limit.take_start_refused(), None, "the creation's is not the call's");

        limit.stage_start_refused(true);
        limit.reset();
        assert_eq!(limit.take_start_refused(), None, "the next transaction's is not this one's");
    }

    /// Limits every dimension holds at zero: anything a transaction counts crosses one.
    fn zero_limits() -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(0)
            .with_frame_data_size_limit(0)
            .with_tx_kv_update_limit(0)
            .with_frame_kv_update_limit(0)
            .with_tx_state_gas_limit(0)
    }

    /// Counts a body, a value call's two records and some state gas, and reports each verdict.
    fn count_a_transaction(limit: &mut AdditionalLimit) -> [LimitCheck; 3] {
        let body = limit.record_tx_body(crate::TX_BODY_SIZE);
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let records = limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::from(1)), 1);
        [body, records, limit.check_state_gas(1_000)]
    }

    /// A transaction that is not exempt is stopped by the first limit it crosses: its body latches
    /// it.
    #[test]
    fn test_a_transaction_that_is_not_exempt_is_stopped() {
        let mut limit = AdditionalLimit::new(zero_limits());
        let [body, ..] = count_a_transaction(&mut limit);
        assert!(body.exceeded_limit() && !body.is_frame_local());
        assert_eq!(limit.latched(), Some(&body));
        assert!(!limit.is_exempt());
    }

    /// An exempt transaction is stopped by no limit, in any dimension, at the transaction or at a
    /// frame: every verdict is within the limits, nothing is latched and no caller is stopped. What
    /// it uses is counted all the same.
    #[test]
    fn test_an_exempt_transaction_is_stopped_by_no_limit() {
        let mut limit = AdditionalLimit::new(zero_limits());
        limit.exempt();
        assert_eq!(count_a_transaction(&mut limit), [LimitCheck::WithinLimit; 3]);
        assert_eq!(limit.latch(LimitKind::DataSize, 0, 1), LimitCheck::WithinLimit);
        assert_eq!(limit.latched(), None);
        assert_eq!(limit.stop_before_run(), None);
        assert_eq!(
            limit.usage(),
            LimitUsage { data_size: crate::TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE, write_records: 2 },
        );

        let mut result = crate::synthetic_frame_result(
            &call_from_to(CALLEE, TARGET, U256::from(1)),
            InstructionResult::Stop,
            Bytes::new(),
        );
        let _ = limit.on_frame_return(&mut result);
        assert_eq!(result.instruction_result(), InstructionResult::Stop, "nothing is rewritten");
        assert_eq!(limit.stop_before_run(), None, "and the caller runs on");
    }

    /// Applied authorities are recorded for an exempt transaction, whatever they cross.
    #[test]
    fn test_an_exempt_transaction_records_its_authorities() {
        let mut limit = AdditionalLimit::new(zero_limits());
        limit.exempt();
        let check = limit.record_applied_authorities(SENDER, 2, false, 1_000);
        assert!(!check.exceeded_limit(), "{check:?}");
        assert_eq!(limit.usage(), WRITE_RECORD.times(2));
    }

    /// The exemption is the transaction's: a reset clears it, and the next transaction is held to
    /// its limits again. A latch cleared before the first frame keeps it.
    #[test]
    fn test_the_exemption_lasts_one_transaction() {
        let mut limit = AdditionalLimit::new(zero_limits());
        limit.exempt();
        let mut result = crate::synthetic_frame_result(
            &call_from_to(SENDER, CALLEE, U256::ZERO),
            InstructionResult::OutOfGas,
            Bytes::new(),
        );
        limit.on_last_frame_return(&mut result);
        assert!(limit.is_exempt(), "an out-of-gas before the first frame keeps the exemption");

        limit.reset();
        assert!(!limit.is_exempt());
        let [body, ..] = count_a_transaction(&mut limit);
        assert!(body.exceeded_limit(), "after a reset the body latches again");
    }

    /// A layer whose transaction's value movements journal transfer logs, under `limits`.
    fn logging(limits: EvmTxRuntimeLimits) -> AdditionalLimit {
        let mut limit = AdditionalLimit::new(limits);
        limit.set_transfer_logs(true);
        limit
    }

    /// A creation from `caller` endowing the account it creates with `value`.
    fn creation(caller: Address, value: U256) -> FrameInput {
        FrameInput::Create(Box::new(revm::interpreter::CreateInputs::new(
            caller,
            revm::interpreter::CreateScheme::Create,
            value,
            Bytes::new(),
            100_000,
            0,
        )))
    }

    /// Exactly the frame starts that move value to another account count a transfer log: a value
    /// call to another account and an endowed creation, at the transaction's own frame and below
    /// it. A call to itself, a `CALLCODE`, a valueless call of any scheme, a `DELEGATECALL`, a
    /// `STATICCALL` and a creation without an endowment count none; and nothing counts one when
    /// the transaction's value movements journal no log.
    #[test]
    fn test_a_frame_start_counts_a_transfer_log_where_value_moves() {
        let one = U256::from(1);
        let scheme = |scheme, value| {
            FrameInput::Call(Box::new(CallInputs {
                caller: CALLEE,
                target_address: if scheme == CallScheme::CallCode { CALLEE } else { TARGET },
                ..call_inputs(scheme, value)
            }))
        };
        for (input, moves) in [
            (call_from_to(CALLEE, TARGET, one), true),
            (call_from_to(CALLEE, CALLEE, one), false),
            (call_from_to(CALLEE, TARGET, U256::ZERO), false),
            (scheme(CallScheme::CallCode, one), false),
            (scheme(CallScheme::StaticCall, U256::ZERO), false),
            (creation(CALLEE, one), true),
            (creation(CALLEE, U256::ZERO), false),
        ] {
            assert_eq!(moves_value(&input), moves, "{input:?}");
            assert!(!with_the_transactions_frame().frame_start_transfer_log(&input), "off");
            let mut limit = logging(EvmTxRuntimeLimits::no_limits());
            limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
            assert_eq!(limit.frame_start_transfer_log(&input), moves, "{input:?}");
            let before = limit.usage();
            limit.on_frame_init(&input, 1);
            let records = limit.usage().write_records - before.write_records;
            assert_eq!(
                limit.usage().data_size - before.data_size,
                WRITE_RECORD_SIZE * records + if moves { TRANSFER_LOG_SIZE } else { 0 },
                "{input:?}",
            );
        }
        let delegate = FrameInput::Call(Box::new(CallInputs {
            value: CallValue::Apparent(one),
            ..call_inputs(CallScheme::DelegateCall, one)
        }));
        assert!(!moves_value(&delegate), "a delegate call carries its caller's value, no transfer");

        // The transaction's own frame: its value to another account, and its endowed creation.
        for (input, moves) in [
            (call_from_to(SENDER, CALLEE, one), true),
            (call_from_to(SENDER, SENDER, one), false),
            (creation(SENDER, one), true),
        ] {
            let mut limit = logging(EvmTxRuntimeLimits::no_limits());
            limit.on_frame_init(&input, 0);
            let log = limit.usage().data_size - WRITE_RECORD_SIZE * limit.usage().write_records;
            assert_eq!(log, if moves { TRANSFER_LOG_SIZE } else { 0 }, "{input:?}");
        }
    }

    /// A transfer log is data size and nothing else: no write record, and no history byte — the
    /// transaction's history is its body and its records, whatever it moved.
    #[test]
    fn test_a_transfer_log_is_data_size_and_never_history() {
        let mut limit = logging(EvmTxRuntimeLimits::no_limits());
        limit.set_intrinsic_history(1, crate::TX_BODY_SIZE);
        limit.record_tx_body(crate::TX_BODY_SIZE);
        let outer = call_from_to(SENDER, CALLEE, U256::from(1));
        limit.on_frame_init(&outer, 0);
        let inner = call_from_to(CALLEE, TARGET, U256::from(1));
        limit.stage_frame_charge(limit.frame_start_records(&inner), 0, 0);
        limit.on_frame_init(&inner, 1);
        assert_eq!(
            limit.usage(),
            LimitUsage {
                data_size: crate::TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE + 2 * TRANSFER_LOG_SIZE,
                write_records: 2,
            },
            "the recipient of each move — the inner one's caller is the outer one's — and two logs",
        );

        let mut result =
            crate::synthetic_frame_result(&inner, InstructionResult::Stop, Bytes::new());
        let _ = limit.on_frame_return(&mut result);
        let mut result =
            crate::synthetic_frame_result(&outer, InstructionResult::Stop, Bytes::new());
        limit.on_last_frame_return(&mut result);
        limit.settle_history_bytes(true);
        assert_eq!(limit.history_bytes(), crate::TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE);
    }

    /// A frame that fails takes its transfer log back with its records, as revm's checkpoint
    /// takes back the move and the log.
    #[test]
    fn test_a_failed_frame_gives_its_transfer_log_back() {
        let mut limit = logging(EvmTxRuntimeLimits::no_limits());
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let inner = call_from_to(CALLEE, TARGET, U256::from(1));
        limit.on_frame_init(&inner, 1);
        assert_eq!(limit.usage().data_size, 2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE);

        let mut result =
            crate::synthetic_frame_result(&inner, InstructionResult::Revert, Bytes::new());
        let _ = limit.on_frame_return(&mut result);
        assert_eq!(limit.usage(), LimitUsage::ZERO);
    }

    /// A frame start whose transfer log crosses a limit is stopped there, before revm moves the
    /// value: its own budget reverts it alone, the transaction's limit latches. The records alone
    /// fit, so it is the log that crosses; at exactly the limit it holds.
    #[test]
    fn test_a_frame_start_whose_transfer_log_crosses_is_stopped() {
        let records = 2 * WRITE_RECORD_SIZE;
        let needs = records + TRANSFER_LOG_SIZE;
        let inner = call_from_to(CALLEE, TARGET, U256::from(1));
        // The smallest frame cap whose share, the budget of a child of the first frame, holds it.
        let cap = (needs..).find(|cap| share_of_remaining(*cap) >= needs).unwrap();
        assert_eq!(share_of_remaining(cap), needs);
        for (cap, fits) in [(cap, true), (cap - 1, false)] {
            let mut limit =
                logging(EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(cap));
            limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
            let check = limit.on_frame_init(&inner, 1);
            if fits {
                assert_eq!(check, LimitCheck::WithinLimit, "exactly the budget");
            } else {
                assert_eq!(
                    check,
                    LimitCheck::ExceedsLimit {
                        kind: LimitKind::DataSize,
                        limit: needs - 1,
                        used: needs,
                        frame_local: true,
                    },
                );
                assert_eq!(limit.latched(), None, "a frame budget does not latch");
            }
        }

        // The transaction's own value: one record, the recipient's, and the log.
        let first = call_from_to(SENDER, CALLEE, U256::from(1));
        let needs = WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE;
        for (tx_limit, fits) in [(needs, true), (needs - 1, false), (WRITE_RECORD_SIZE, false)] {
            let mut limit =
                logging(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(tx_limit));
            let check = limit.on_frame_init(&first, 0);
            if fits {
                assert_eq!(check, LimitCheck::WithinLimit, "exactly the limit");
                continue;
            }
            assert_eq!(
                check,
                LimitCheck::ExceedsLimit {
                    kind: LimitKind::DataSize,
                    limit: tx_limit,
                    used: needs,
                    frame_local: false,
                },
            );
            assert_eq!(limit.latched(), Some(&check));
        }
    }

    /// A `SELFDESTRUCT` that moved value to another account counts its transfer log with the
    /// beneficiary's record, in one check, and reports the record's history alone. To the sender,
    /// whose account the body counts, it is the log alone, and the log alone can cross.
    #[test]
    fn test_a_destruction_counts_its_transfer_log_with_its_beneficiary() {
        let destruction = |beneficiary| StagedRecord::SelfDestruct {
            had_value: true,
            target_exists: true,
            to_other_account: true,
            beneficiary,
        };
        let mut limit = logging(EvmTxRuntimeLimits::no_limits());
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        limit.stage_record(destruction(TARGET));
        let (check, history) = limit.commit_staged_record();
        assert_eq!(check, LimitCheck::WithinLimit);
        assert_eq!(history, HistoryBytes::Appended(WRITE_RECORD_SIZE), "the record's alone");
        assert_eq!(
            limit.usage(),
            LimitUsage { data_size: WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE, write_records: 1 },
        );

        let mut limit =
            logging(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TRANSFER_LOG_SIZE - 1));
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        limit.stage_record(destruction(SENDER));
        let (check, history) = limit.commit_staged_record();
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

        let mut limit = AdditionalLimit::default();
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        limit.stage_record(destruction(TARGET));
        let _ = limit.commit_staged_record();
        assert_eq!(limit.usage(), WRITE_RECORD, "no log, no bytes");
    }

    /// The transfer logs are the transaction's to set: a reset turns them off until the next
    /// transaction sets them again.
    #[test]
    fn test_a_reset_turns_the_transfer_logs_off() {
        let mut limit = logging(EvmTxRuntimeLimits::no_limits());
        let input = call_from_to(SENDER, CALLEE, U256::from(1));
        assert!(limit.frame_start_transfer_log(&input));
        limit.reset();
        assert!(!limit.frame_start_transfer_log(&input));
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
