//! The per-transaction state of the common execution layer.

use alloy_primitives::{Address, Bytes};
use revm::{
    context_interface::{
        context::CodeDeposit,
        host::{CompletedWrite, FrameStart, FrameStartKind},
    },
    handler::FrameResult,
    interpreter::{CallInputs, CallScheme, FrameInput, InstructionResult},
};

use super::{
    frame_limit::{FrameLimitTracker, Lane},
    record::{write_effect, write_moves_value, HistoryBytes, RecordEffect},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, TRANSFER_LOG, WRITE_RECORD, WRITE_RECORD_SIZE,
};
use crate::storage_call_stipend;

/// What the common execution layer tracks for the running transaction: the per-frame lanes of
/// data-size bytes and write records, the stops its frames return, and whether it is exempt from
/// every limit.
///
/// It lives on the [`MegaContext`](crate::MegaContext), is reset before each transaction and
/// system call, and is driven by the Host's instruction hooks (the records of completed writes and
/// the charge of a frame's start) and the frame lifecycle (lanes, stops).
///
/// # The stop
///
/// A limit crossed stops the frame that crossed it with a revert, not a halt: the frame reverts
/// with [`MegaLimitExceeded`](crate::MegaLimitExceeded) as its output, whether it crossed the
/// transaction's limit or its own frame budget, and its caller resumes as after any revert. What
/// the frame counted goes with it, so a caller that resumes is back within the limits, and a
/// transaction never keeps more than its limit: a caller that carries on is held to what is left,
/// and a charge that crosses again stops the frame that makes it. A crossing the transaction's
/// own frame makes is the transaction's result, which settles like any EIP-8037 revert: its
/// unspent regular gas and the reservoir go back to the sender, who pays only for what ran.
/// Exceptional halts (a real out-of-gas, an invalid opcode) are not limit stops and still burn the
/// frame's gas.
///
/// # The exemption
///
/// The protocol's own work is held to none of the per-transaction limits: a system-originated
/// transaction and a system call are marked exempt before their body is counted, and the mark is
/// sticky for the transaction ([`is_exempt`](Self::is_exempt)). Every stop the layer hands out
/// comes from one place ([`crossed`](Self::crossed)), which answers an exempt transaction with
/// [`LimitCheck::WithinLimit`] whatever it crossed — a transaction limit or a frame budget, in
/// any dimension.
/// What the transaction uses is counted all the same, so its usage is reported as any other
/// transaction's is.
#[derive(Clone, Debug, Default)]
pub struct AdditionalLimit {
    pub(crate) tracker: FrameLimitTracker,
    limits: EvmTxRuntimeLimits,
    /// Whether the transaction is exempt from every per-transaction limit, sticky for the
    /// transaction. See [`exempt`](Self::exempt).
    exempt: bool,
    /// The stop the frame about to run must return instead of running: the one a frame revm built
    /// returns on its first run when its start crossed a limit
    /// ([`hold_built_start`](Self::hold_built_start)), or the one a frame a child returned into
    /// returns when what the child left it put it over a limit
    /// ([`on_frame_return`](Self::on_frame_return)).
    pending_stop: Option<LimitCheck>,
    /// The crossing the result of the frame returning next was made of, noted where the stop
    /// became that result ([`note_frame_stop`](Self::note_frame_stop)) and taken when the frame
    /// returns.
    frame_stop: Option<LimitCheck>,
    /// The crossing that produced the result of the transaction's own frame, if one did. See
    /// [`limit_exceeded`](Self::limit_exceeded).
    limit_exceeded: Option<LimitCheck>,
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
    /// The write records the transaction's own frame makes, as the charge of their history before
    /// execution found them, for [`push_lane`](Self::push_lane) to check its own count against.
    /// See [`stage_top_level_records`](Self::stage_top_level_records).
    pending_top_level_records: Option<u64>,
    /// The history gas the running opcode charged its own frame for the records the frame it is
    /// starting will make, waiting for that frame's lane to be pushed.
    pending_frame_charge: FrameCharge,
    /// The history gas the settled transaction spent.
    history_gas_spent: u64,
    /// The history bytes the settled transaction appended.
    history_bytes: u64,
    /// The EIP-8037 reservoir the settled transaction left unspent.
    reservoir_remaining: u64,
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
        self.exempt = false;
        self.pending_stop = None;
        self.frame_stop = None;
        self.limit_exceeded = None;
        self.target_is_authority = false;
        self.sender = Address::ZERO;
        self.frame_began = false;
        self.body_bytes = 0;
        self.intrinsic_history_gas = 0;
        self.intrinsic_history_bytes = 0;
        self.top_level_write_record_gas = 0;
        self.pending_top_level_records = None;
        self.pending_frame_charge = FrameCharge::NONE;
        self.history_gas_spent = 0;
        self.history_bytes = 0;
        self.reservoir_remaining = 0;
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
    /// the frame returns before any other does: so the stop is the returning frame's when
    /// [`on_frame_return`](Self::on_frame_return) takes it, and the transaction's when that frame
    /// is the transaction's own ([`limit_exceeded`](Self::limit_exceeded)).
    pub(crate) fn note_frame_stop(&mut self, stop: &LimitCheck) -> Bytes {
        debug_assert!(stop.exceeded_limit(), "only a crossing stops a frame");
        self.frame_stop = Some(*stop);
        stop.revert_data()
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

    /// The verdict on `used` crossing `kind`'s `limit`, a frame budget when `frame_local` and a
    /// transaction-level limit otherwise: the stop the running frame reverts with, alone, its
    /// [`LimitCheck::revert_data`] naming the limit crossed.
    ///
    /// Every stop the layer hands out comes from here, gas detention's included, so this is where
    /// the exemption applies: an exempt transaction's verdict is [`LimitCheck::WithinLimit`],
    /// whatever it crossed.
    pub(crate) const fn crossed(
        &self,
        kind: LimitKind,
        limit: u64,
        used: u64,
        frame_local: bool,
    ) -> LimitCheck {
        if self.exempt {
            return LimitCheck::WithinLimit;
        }
        LimitCheck::ExceedsLimit { kind, limit, used, frame_local }
    }

    /// Checks the data-size limit after the running frame counted something: what the
    /// transaction keeps against its limit, then what the running frame keeps against its budget.
    /// A record that crosses both names the transaction's limit.
    fn check(&self) -> LimitCheck {
        let (kept, limit) = (self.tracker.net(), self.limits.tx_data_size_limit);
        if kept.exceeds_data_size(limit) {
            return self.crossed(LimitKind::DataSize, limit, kept.data_size, false);
        }
        if let Some(lane) = self.tracker.current() {
            let (kept, budget) = (lane.net(), lane.budget);
            if kept.exceeds_data_size(budget) {
                return self.crossed(LimitKind::DataSize, budget, kept.data_size, true);
            }
        }
        LimitCheck::WithinLimit
    }

    /// The stop the running frame returns instead of running another instruction: the one its own
    /// start left it ([`hold_built_start`](Self::hold_built_start)), or the one
    /// [`on_frame_return`](Self::on_frame_return) left for a caller its child put over a limit.
    /// Taking it clears it, which stops one frame.
    pub(crate) const fn stop_before_run(&mut self) -> Option<LimitCheck> {
        self.pending_stop.take()
    }

    /// Whether the running frame has a stop to return before it runs another instruction
    /// ([`stop_before_run`](Self::stop_before_run)).
    pub(crate) const fn has_stop_before_run(&self) -> bool {
        self.pending_stop.is_some()
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

    /// The EIP-8037 reservoir the last settled transaction or system call left unspent, which
    /// its sender gets back, as a transaction reports it
    /// ([`MegaGasUsage::reservoir_remaining`](crate::MegaGasUsage::reservoir_remaining)). It is
    /// read before the EIP-7623 floor absorbs it, and a failed deposit leaves none.
    pub const fn reservoir_remaining(&self) -> u64 {
        self.reservoir_remaining
    }

    /// Records the reservoir the settled transaction or system call left unspent.
    pub(crate) const fn set_reservoir_remaining(&mut self, reservoir: u64) {
        self.reservoir_remaining = reservoir;
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

    /// The history bytes the last settled transaction appended, whoever paid for them, which a
    /// transaction reports as [`MegaGasUsage::history_bytes`](crate::MegaGasUsage::history_bytes);
    /// [`history_gas_spent`](Self::history_gas_spent) is what the transaction's own gas paid of
    /// their price.
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

    /// Leaves the write records the transaction's own frame makes, as the charge of their history
    /// before execution found them, for [`push_lane`](Self::push_lane) to check its own count
    /// against when it pushes that frame's lane
    /// ([`check_top_level_records`](Self::check_top_level_records)).
    ///
    /// Only a transaction that pays history is charged, and so leaves one. A first frame answered
    /// or refused before its lane is pushed leaves it for the next transaction's reset to clear.
    pub(crate) const fn stage_top_level_records(&mut self, records: u64) {
        self.pending_top_level_records = Some(records);
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

    /// Leaves the frame about to start unchecked against the answer its caller's charge was
    /// computed from ([`records_the_caller_paid_for`](Self::records_the_caller_paid_for)): an
    /// inspector that may rewrite the frame's input ran between the charge and the count.
    pub(crate) const fn waive_start_check(&mut self) {
        self.pending_frame_charge.records = None;
    }

    /// Records the history gas the settled transaction spent.
    pub(crate) const fn set_history_gas_spent(&mut self, history_gas_spent: u64) {
        self.history_gas_spent = history_gas_spent;
    }

    /* Completed writes */

    /// Commits the record of `write`, which the running opcode completed, to the running frame's
    /// lane, and reports the history bytes it appends or takes back. Called by the Host's
    /// completed-write hook, which revm calls only once the opcode made every charge; a crossed
    /// limit in the verdict stops the opcode's frame, and the hook charges the history.
    ///
    /// A `SELFDESTRUCT` that moved value to another account also counts the EIP-7708 transfer log
    /// revm journaled for the move, with the beneficiary's record and in the same check. The log
    /// is the running frame's, as the move is: the frame's own journal checkpoint takes both back
    /// when the frame fails, and its lane the bytes. It is data size and nothing else — no write
    /// record, and no history: the bytes reported are the record's alone.
    #[inline]
    pub(crate) fn commit_write(
        &mut self,
        write: &CompletedWrite<'_>,
    ) -> (LimitCheck, HistoryBytes) {
        let effect = write_effect(write, self.sender);
        let history = effect.history_bytes();
        let counted = match effect {
            RecordEffect::None => LimitUsage::ZERO,
            RecordEffect::Record(usage) => usage,
            RecordEffect::Refund(usage) => {
                self.tracker.refund(usage);
                return (LimitCheck::WithinLimit, history);
            }
        };
        match write {
            // A log's bytes are history beside the write records; a record's are counted as one.
            CompletedWrite::Log { .. } => self.tracker.record_log_and_code_bytes(counted.data_size),
            CompletedWrite::Sstore { .. } | CompletedWrite::SelfDestruct { .. } => {}
        }
        let counted = if self.transfer_logs && write_moves_value(write) {
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

    /* Transaction-level records */

    /// Counts the transaction's body: its envelope, the writes its inclusion makes, its calldata,
    /// its authorizations and its access list.
    ///
    /// The bytes are the transaction's, recorded before any frame, so a revert does not take them
    /// back, and neither does an out-of-gas before the first frame. The count is all it does:
    /// validation holds the body to the limit ([`admits_tx_body`](Self::admits_tx_body)) on this
    /// same count, and refuses a transaction it does not admit, so a transaction that runs never
    /// crosses the limit with its body.
    pub(crate) fn record_tx_body(&mut self, bytes: u64) {
        self.body_bytes = bytes;
        if bytes != 0 {
            self.tracker.record_tx(LimitUsage { data_size: bytes, write_records: 0 });
        }
    }

    /// The data-size bytes of the running transaction's body, as
    /// [`record_tx_body`](Self::record_tx_body) counted them.
    pub(crate) const fn body_bytes(&self) -> u64 {
        self.body_bytes
    }

    /// Whether the running transaction's body, with one write record for each of its
    /// `authorizations`, fits its data-size limit: what validation holds a transaction to.
    ///
    /// Every entry of the authorization list counts, whether it will apply or not, so the figure is
    /// an upper bound on what the transaction counts before its first frame — the body, then one
    /// record for each distinct authority that applies — and a transaction validation admits
    /// cannot cross the limit there. An exempt transaction is held to nothing.
    pub(crate) fn admits_tx_body(&self, authorizations: u64) -> bool {
        self.exempt ||
            !LimitUsage { data_size: self.body_bytes, write_records: 0 }
                .saturating_add(WRITE_RECORD.times(authorizations))
                .exceeds_data_size(self.limits.tx_data_size_limit)
    }

    /// Counts the `bytes` of an Oracle hint, the payload a `sendHint` call hands to the node's
    /// oracle service.
    ///
    /// The bytes are counted before the payload is decoded, so a caller cannot make the node
    /// materialise a payload for free by appending bytes an ABI decoder ignores. They are the
    /// transaction's, not the calling frame's, and no frame's budget holds them. Nothing the frame
    /// or the transaction does afterwards takes them back, not even for a payload that fails to
    /// decode and is never forwarded, for two separate reasons: a payload forwarded to the oracle
    /// service cannot be taken back, and admitting a payload and decoding it is work done whether
    /// or not it decodes.
    ///
    /// The limit is checked before the bytes are counted. A hint that would cross it is the stop,
    /// with the usage it would have reached, and is neither forwarded nor counted: its payload
    /// never left the machine, so the transaction keeps none of it, as it keeps nothing of a log
    /// that crossed the limit.
    pub(crate) fn record_hint_bytes(&mut self, bytes: u64) -> LimitCheck {
        let hint = LimitUsage { data_size: bytes, write_records: 0 };
        let (reached, limit) =
            (self.tracker.net().saturating_add(hint), self.limits.tx_data_size_limit);
        if reached.exceeds_data_size(limit) {
            let stop = self.crossed(LimitKind::DataSize, limit, reached.data_size, false);
            if stop.exceeded_limit() {
                return stop;
            }
        }
        self.tracker.record_tx(hint);
        LimitCheck::WithinLimit
    }

    /// Records the account writes of the applied EIP-7702 authorities other than the sender:
    /// `authorities` distinct accounts, `target_is_authority` if the transaction's call target is
    /// one of them, from the transaction's `sender`.
    ///
    /// The records cannot cross the data-size limit: validation admitted the body with a record for
    /// every entry of the authorization list ([`admits_tx_body`](Self::admits_tx_body)), and only
    /// an entry can apply. A debug build asserts it.
    pub(crate) fn record_applied_authorities(
        &mut self,
        sender: Address,
        authorities: u64,
        target_is_authority: bool,
    ) {
        self.sender = sender;
        self.target_is_authority = target_is_authority;
        self.tracker.record(WRITE_RECORD.times(authorities));
        debug_assert!(
            self.exempt || !self.tracker.net().exceeds_data_size(self.limits.tx_data_size_limit),
            "validation admits no body whose authority records cross the data-size limit"
        );
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
    /// revm journals for the move, on the frame's own lane. The lane follows the frame's journal
    /// checkpoint: a frame that fails discards both the log and its bytes.
    ///
    /// The count is made as the frame starts, before revm decides the start, and nothing is held
    /// to a limit until revm has: a start revm builds the frame for is held to the limits before
    /// the frame runs ([`hold_built_start`](Self::hold_built_start)), and one revm answers once
    /// the answer is known ([`check_start`](Self::check_start)); a start revm refuses on its
    /// caller's account made nothing, and its lane is popped with its failure, or stands for
    /// nothing ([`withdraw_refused_start`](Self::withdraw_refused_start)). A start past the
    /// call-stack limit, which the depth guard answers before it reaches here, gets an empty
    /// lane ([`push_empty_frame`](Self::push_empty_frame)).
    pub(crate) fn on_frame_init(&mut self, input: &FrameInput, depth: usize) {
        self.frame_began = true;
        self.push_lane(input, depth);
        if self.frame_start_transfer_log(input) {
            self.tracker.record(TRANSFER_LOG);
        }
    }

    /// Holds the start of the frame revm just built to the limits: a crossing is the stop the
    /// frame returns on its first run, before any instruction ([`stop_before_run`]). revm moved
    /// the value, journaled the transfer log and, for a creation, bumped its creator's nonce; the
    /// stop is a revert of the frame, so its journal checkpoint takes the move and the log back,
    /// and the nonce, bumped before the checkpoint, stays, with its record on the creator.
    ///
    /// [`stop_before_run`]: Self::stop_before_run
    #[inline]
    pub(crate) fn hold_built_start(&mut self) {
        let check = self.check();
        if check.exceeded_limit() {
            debug_assert!(self.pending_stop.is_none(), "a frame starts from a caller that ran");
            self.pending_stop = Some(check);
        }
    }

    /// The verdict on the start of the frame revm just answered without building it, the
    /// writes its start made counted: a crossing means the answer must give way to the stop.
    pub(crate) fn check_start(&self) -> LimitCheck {
        self.check()
    }

    /// Withdraws the count of the start of the frame revm just answered, a creation revm refused
    /// on its creator's account before it wrote anything: an empty lane stands in for the
    /// frame's, so the creation counts no record, its creator's account is no more recorded than
    /// before, and its caller gets back, when the lane is popped, all it paid for the records.
    pub(crate) fn withdraw_refused_start(&mut self) {
        if let Some(lane) = self.tracker.current_mut() {
            lane.creator_record = false;
        }
        self.tracker.discard_running_lane();
    }

    /// Holds the code a creation deposits to the data-size limit, and names the stop when it
    /// crosses it: the bytes are counted on the creation's own lane.
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
    /// The same bytes are history beside the write records, counted here on the same lane, so the
    /// history a transaction reports it appended and the data size it kept move together: a
    /// creation that deposits nothing, or whose deposit fails, appends neither.
    pub(crate) fn on_code_deposit(&mut self, deposit: &CodeDeposit<'_>) -> Result<(), Bytes> {
        let bytes = deposit.code.len() as u64;
        if bytes == 0 {
            return Ok(());
        }
        debug_assert!(self.tracker.current().is_some(), "a creation returns on its own lane");
        self.tracker.record(LimitUsage { data_size: bytes, write_records: 0 });
        self.tracker.record_log_and_code_bytes(bytes);
        let check = self.check();
        if check.exceeded_limit() {
            return Err(self.note_frame_stop(&check));
        }
        Ok(())
    }

    /// The data-size budget of the frame about to start.
    ///
    /// The transaction's own frame gets what the transaction has left, and never more than the
    /// frame cap ([`frame_data_size_limit`](EvmTxRuntimeLimits::frame_data_size_limit)). A child
    /// gets [`FRAME_DATA_SHARE_NUMERATOR`] / [`FRAME_DATA_SHARE_DENOMINATOR`] of what its parent
    /// has left, under the same cap. What the parent has left is its budget minus what it has
    /// already kept, so a parent that has spent part of its budget forwards a smaller share.
    fn frame_budget(&self) -> u64 {
        let forwarded = match self.tracker.current() {
            Some(caller) => share_of_remaining(caller.remaining_budget()),
            None => self.limits.tx_data_size_limit.saturating_sub(self.tracker.net().data_size),
        };
        forwarded.min(self.limits.frame_data_size_limit)
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
                    let records = u64::from(transfers_value && !written_outside);
                    self.check_top_level_records(records);
                    self.tracker.push(Lane::new(
                        Some(target),
                        written_outside || transfers_value,
                        budget,
                        0,
                    ));
                    self.tracker.record(WRITE_RECORD.times(records));
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
                    self.check_top_level_records(records.on_lane);
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
    /// opcode, on the facts revm's instruction hands the frame-start hook, or at a `keylessDeploy`
    /// call's first run, on the creation it starts; and the count here, on the input that survived
    /// interception. They agree because the input revm builds carries the caller, target and value
    /// the hook was handed, and nothing between the two changes it: an interceptor answers a frame
    /// or lets it start as it is, and a creation reaches none. A divergence trips here in every
    /// debug build rather than mis-charging the caller and mis-splitting the refund its failure
    /// gets back.
    ///
    /// An inspector that may rewrite the input runs between the two, and can split them: under one
    /// the frame is not checked ([`waive_start_check`](Self::waive_start_check)).
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

    /// Checks the write records the transaction's own frame makes on its lane, `counted` here,
    /// against the ones its history was charged for before execution
    /// ([`stage_top_level_records`](Self::stage_top_level_records)).
    ///
    /// The two are separate answers to the same question: the charge, on the transaction, and the
    /// count here, on the frame input revm built from it. They agree because that input carries
    /// the transaction's own caller, target and value, and both read the same answer to whether
    /// the target is an applied authority. An inspector that rewrites the input so that the answer
    /// changes splits them, as it does for a frame an opcode starts
    /// ([`records_the_caller_paid_for`](Self::records_the_caller_paid_for)). A divergence trips
    /// here in every debug build rather than charging history for a record the data size does not
    /// count, or counting one no history paid for.
    fn check_top_level_records(&mut self, counted: u64) {
        if let Some(charged) = self.pending_top_level_records.take() {
            debug_assert_eq!(
                charged, counted,
                "the transaction's own frame makes other write records than its history was \
                 charged for",
            );
        }
    }

    /// The write records starting `input` makes at a depth above the transaction's own frame,
    /// counted as the frame starts ([`start_records`](Self::start_records)).
    pub(crate) fn frame_start_records(&self, input: &FrameInput) -> FrameStartRecords {
        match input {
            FrameInput::Call(inputs) => self.start_records(
                inputs.caller,
                Some(inputs.target_address),
                inputs.transfers_value(),
            ),
            FrameInput::Create(inputs) => self.start_records(inputs.caller(), None, true),
            FrameInput::Empty => FrameStartRecords::NONE,
        }
    }

    /// The write records the frame `start` describes makes, for the opcode about to start it,
    /// which pays for them ([`start_records`](Self::start_records)).
    pub(crate) fn opcode_start_records(&self, start: &FrameStart) -> FrameStartRecords {
        let transfers_value = !start.value.is_zero();
        match start.kind {
            FrameStartKind::Create | FrameStartKind::Create2 => {
                self.start_records(start.caller, None, true)
            }
            FrameStartKind::Call |
            FrameStartKind::CallCode |
            FrameStartKind::DelegateCall |
            FrameStartKind::StaticCall => {
                self.start_records(start.caller, start.target, transfers_value)
            }
        }
    }

    /// The write records a frame's start makes at a depth above the transaction's own frame: a
    /// call from `caller` to `target` that transfers value or not, or a creation (`target` is
    /// `None`). `on_lane` are the records the frame's failure discards, `caller` whether its start
    /// also writes the caller's own account.
    ///
    /// A call that moves no value writes nothing. A value call writes its recipient, unless that is
    /// the caller itself or the transaction's sender, whose account the body counts. A creation
    /// writes the account it creates. Either writes the caller's account — a transfer's sender, a
    /// creation's creator nonce — unless the caller's frame already recorded it.
    ///
    /// The opcode that starts the frame pays for all of them, after the gas it forwards was taken,
    /// so the frame's own budget carries none of them — which is what lets a value transfer's
    /// allowance pay for what the recipient does with it. The answer is asked twice, at the opcode
    /// ([`opcode_start_records`](Self::opcode_start_records)) and as the frame starts
    /// ([`frame_start_records`](Self::frame_start_records)), and the two are checked against each
    /// other ([`records_the_caller_paid_for`](Self::records_the_caller_paid_for)).
    fn start_records(
        &self,
        caller: Address,
        target: Option<Address>,
        transfers_value: bool,
    ) -> FrameStartRecords {
        let caller_recorded = self.tracker.current().is_some_and(|lane| lane.account_recorded);
        let Some(target) = target else {
            return FrameStartRecords { on_lane: 1, caller: !caller_recorded };
        };
        if !transfers_value {
            return FrameStartRecords::NONE;
        }
        let writes_target = target != caller && target != self.sender;
        FrameStartRecords { on_lane: u64::from(writes_target), caller: !caller_recorded }
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
    pub(crate) fn set_frame_creator(&mut self, creator: Address) {
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
    /// A stop the record left the frame, by putting it over a limit, goes with it: the frame is
    /// held to its limits again without the record. Returns whether a record was taken back.
    pub(crate) fn take_back_creator_record(&mut self) -> bool {
        let sender = self.sender;
        if self.tracker.current().is_none_or(|lane| lane.address == Some(sender)) ||
            !self.tracker.take_back_own_record()
        {
            return false;
        }
        let check = self.check();
        self.pending_stop = check.exceeded_limit().then_some(check);
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
    /// a failure discards it, a stop's included, so the caller resumes within its limits.
    ///
    /// Two returns would add to a caller what no check has held it to, and after them the caller is
    /// held to its limits with what it now holds; a crossing is the stop it returns before it runs
    /// on ([`stop_before_run`](Self::stop_before_run)), naming its budget or the transaction's
    /// limit.
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
    /// The crossing the result is made of, the stop noted for the frame
    /// ([`note_frame_stop`](Self::note_frame_stop)), is the transaction's
    /// [`limit_exceeded`](Self::limit_exceeded) when the frame is the transaction's own and its
    /// result is still the stop's revert.
    ///
    /// Returns the history gas the caller paid for records this frame did not keep, which the
    /// caller gets back once the frame has merged into it.
    #[must_use = "the history of the records the frame did not keep goes back to its caller"]
    pub(crate) fn on_frame_return(&mut self, result: &FrameResult) -> u64 {
        let noted = self.frame_stop.take();
        if self.tracker.depth() == 1 {
            self.limit_exceeded = noted.filter(|stop| is_the_revert_of(result, stop));
        }
        // A charge an opcode of this frame made for a child that never started died with that
        // opcode, which failed after making it.
        self.pending_frame_charge = FrameCharge::NONE;
        let success = result.instruction_result().is_ok();
        let lane = self.tracker.pop(success);
        let refund = lane.as_ref().map_or(0, |lane| lane.history_refund(success));
        let unchecked = lane.is_some_and(|lane| self.tracker.hands_unchecked(&lane, success));
        self.pending_stop = if unchecked {
            let check = self.check();
            check.exceeded_limit().then_some(check)
        } else {
            debug_assert!(
                self.check().within_limit(),
                "a frame's return put its caller over a limit no check held it to"
            );
            None
        };
        refund
    }

    /// Settles the transaction's outermost frame: pops its lane unless the frame already
    /// returned through [`on_frame_return`](Self::on_frame_return), which is the case whenever
    /// it ran.
    ///
    /// A transaction whose first frame never reached frame init ran out of gas in the runtime
    /// phase before it: the out-of-gas took back the authorizations applied before it, so their
    /// records go too, and the halt stays a halt. The body is not one of those records. It is put
    /// back after the reset. Nothing counted before the first frame can have crossed a limit,
    /// validation having held the body and the authorities' records to it, so the halt is what the
    /// transaction reports. An exemption stays: it is the transaction's, not its frames'.
    pub(crate) fn on_last_frame_return(&mut self, result: &FrameResult) {
        if !self.frame_began {
            let body = self.body_bytes;
            self.tracker.reset();
            self.tracker.record_tx(LimitUsage { data_size: body, write_records: 0 });
            return;
        }
        debug_assert!(self.tracker.depth() <= 1, "only the outermost lane can be left");
        if self.tracker.depth() == 1 {
            // The outermost frame has no caller that paid for its records: the transaction did,
            // before execution, and the settlement gives that charge back.
            let refund = self.on_frame_return(result);
            debug_assert_eq!(refund, 0, "the outermost frame's caller is the transaction");
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

/// Whether `result` is the revert `stop` makes a frame return: a revert whose output is the
/// stop's [`MegaLimitExceeded`](crate::MegaLimitExceeded).
fn is_the_revert_of(result: &FrameResult, stop: &LimitCheck) -> bool {
    let result = result.interpreter_result();
    result.result == InstructionResult::Revert && result.output == stop.revert_data()
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
    use revm::{
        context_interface::context::SelfDestructResult,
        interpreter::{CallInput, CallValue},
    };

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

    /// A hint is held to the transaction's data-size limit before it is counted: one that fits is
    /// counted, one that would cross is the stop at the usage it would have reached and leaves the
    /// count as it was, and a later one that fits is counted, the crossing having stopped nothing
    /// but its own call. An exempt transaction's is counted whatever it crosses.
    #[test]
    fn test_a_hint_that_would_cross_the_limit_is_not_counted() {
        let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(100);
        let kept = |limit: &AdditionalLimit| limit.usage().data_size;

        let mut limit = AdditionalLimit::new(limits);
        limit.record_tx_body(40);
        assert_eq!(limit.record_hint_bytes(59), LimitCheck::WithinLimit);
        assert_eq!(kept(&limit), 99);
        let stop = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 100,
            used: 101,
            frame_local: false,
        };
        assert_eq!(limit.record_hint_bytes(2), stop, "two more bytes cross");
        assert_eq!(kept(&limit), 99, "the hint that crossed is not counted");
        assert_eq!(limit.record_hint_bytes(1), LimitCheck::WithinLimit, "one more fits exactly");
        assert_eq!(kept(&limit), 100, "and is counted");

        let mut exempt = AdditionalLimit::new(limits);
        exempt.exempt();
        exempt.record_tx_body(40);
        assert_eq!(exempt.record_hint_bytes(61), LimitCheck::WithinLimit);
        assert_eq!(kept(&exempt), 101, "an exempt transaction's hint is counted");
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

    /// A creation revm refused on its creator's account counts nothing once its count is
    /// withdrawn: whatever revm answered — `OutOfFunds`, or the success with no address it gives a
    /// creator whose nonce cannot be bumped — its records are not kept, its creator's account is
    /// not recorded, and its caller gets back all it paid, the nonce record's charge included.
    #[test]
    fn test_a_refused_start_counts_nothing_and_gives_its_charge_back() {
        for answered in [InstructionResult::OutOfFunds, InstructionResult::Return] {
            let mut limit = with_the_transactions_frame();
            let input = creation(CALLEE, U256::from(1));
            let records = limit.frame_start_records(&input);
            assert_eq!(records, FrameStartRecords { on_lane: 1, caller: true });
            limit.stage_frame_charge(records, 3, 4);
            limit.on_frame_init(&input, 1);
            limit.withdraw_refused_start();
            assert_eq!(limit.usage(), LimitUsage::ZERO, "{answered:?}");
            let result = crate::synthetic_frame_result(&input, answered, Bytes::new());
            assert_eq!(limit.on_frame_return(&result), 3 + 4, "{answered:?}: the whole charge");
            assert!(!limit.tracker.current().unwrap().account_recorded, "{answered:?}");
            assert_eq!(limit.usage(), LimitUsage::ZERO, "{answered:?}");
        }
    }

    /// A frame answered without running — by the depth guard, a stop, an interceptor or an
    /// inspector — keeps none of the records its caller paid for, whatever its answer: its lane is
    /// empty, the whole charge comes back, and the transaction's history bytes are its body alone.
    ///
    /// The depth guard's answer is reached here rather than through a transaction: under the
    /// execution cap no call chain reaches the call-stack limit, because each call forwards at most
    /// sixty-three sixty-fourths of what its caller has left, and 200,000,000 gas leaves about
    /// twenty at depth 1,024. A stop's and an interceptor's answers are also reached through
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

            let result = crate::synthetic_frame_result(&inner, answer, Bytes::new());
            assert_eq!(limit.on_frame_return(&result), 3_000, "{answer:?}: all of it back");
            assert_eq!(limit.usage(), LimitUsage::ZERO, "{answer:?}: nothing kept");

            let outer = call_from_to(SENDER, CALLEE, U256::ZERO);
            let result =
                crate::synthetic_frame_result(&outer, InstructionResult::Stop, Bytes::new());
            limit.on_last_frame_return(&result);
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

    /// The transaction's own frame counts on its lane the write records its history was charged
    /// for before execution: its value's recipient, unless that is the sender or an applied
    /// authority, and the account a creation makes.
    #[test]
    fn test_the_transactions_own_frame_counts_the_records_its_history_paid_for() {
        let one = U256::from(1);
        let rows = [
            ("a value call", call_from_to(SENDER, CALLEE, one), false, 1),
            ("a call without value", call_from_to(SENDER, CALLEE, U256::ZERO), false, 0),
            ("a value call to the sender", call_from_to(SENDER, SENDER, one), false, 0),
            ("a value call to an applied authority", call_from_to(SENDER, CALLEE, one), true, 0),
            ("a creation", creation(SENDER, U256::ZERO), false, 1),
            ("an endowed creation", creation(SENDER, one), false, 1),
        ];
        for (name, input, target_is_authority, records) in rows {
            let authority = u64::from(target_is_authority);
            let mut limit = AdditionalLimit::default();
            limit.record_applied_authorities(SENDER, authority, target_is_authority);
            limit.stage_top_level_records(records);
            limit.on_frame_init(&input, 0);
            assert_eq!(limit.usage().write_records, authority + records, "{name}");
            assert_eq!(limit.pending_top_level_records, None, "{name}: taken once");
        }
    }

    /// A transaction's own frame that would count a record its history did not pay for trips the
    /// check, rather than counting data size no history matches.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "other write records than its history was charged for")]
    fn test_a_top_level_record_its_history_did_not_pay_for_trips() {
        let mut limit = AdditionalLimit::default();
        limit.stage_top_level_records(0);
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::from(1)), 0);
    }

    /// A transaction's own frame that makes no record its history was charged for trips the check,
    /// rather than keeping history no data size matches.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "other write records than its history was charged for")]
    fn test_a_top_level_charge_for_a_record_the_frame_does_not_make_trips() {
        let mut limit = AdditionalLimit::default();
        limit.stage_top_level_records(1);
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
    }

    /// A creation as the transaction's own frame is checked too: one whose history paid for no
    /// record of the account it creates trips.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "other write records than its history was charged for")]
    fn test_a_top_level_creation_its_history_did_not_pay_for_trips() {
        let mut limit = AdditionalLimit::default();
        limit.stage_top_level_records(0);
        limit.on_frame_init(&creation(SENDER, U256::ZERO), 0);
    }

    /// A child frame's budget is 98% of what its parent has left, three frames down, and the
    /// transaction's own frame gets what the transaction has left.
    #[test]
    fn test_a_child_frame_gets_98_percent_of_what_its_parent_has_left() {
        let mut limit =
            AdditionalLimit::new(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(10_000));
        limit.tracker.record_tx(LimitUsage { data_size: 310, write_records: 0 });

        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let first = 10_000 - 310;
        assert_eq!(limit.tracker.current().unwrap().budget, first);

        limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::ZERO), 1);
        let child = share_of_remaining(first);
        assert_eq!(child, 9_496);
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

    /// A layer with a frame cap of 100 bytes and the transaction's own frame started, which is
    /// what the cap gives it; `CALLEE`'s call to `TARGET`, the child, is started below it with
    /// 98 of them.
    fn with_a_child_of_a_capped_frame() -> (AdditionalLimit, FrameInput) {
        let mut limit =
            AdditionalLimit::new(EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(100));
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let child = call_from_to(CALLEE, TARGET, U256::ZERO);
        limit.on_frame_init(&child, 1);
        assert_eq!(limit.tracker.current().unwrap().budget, 98);
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
            let result =
                crate::synthetic_frame_result(&child, InstructionResult::Stop, Bytes::new());
            let _ = limit.on_frame_return(&result);
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
        limit.tracker.current_mut().unwrap().budget = u64::MAX;
        limit.tracker.record(LimitUsage { data_size: 101, write_records: 0 });
        let result = crate::synthetic_frame_result(&child, InstructionResult::Stop, Bytes::new());
        let _ = limit.on_frame_return(&result);
    }

    /// Limits every dimension holds at zero: anything a transaction counts crosses one.
    fn zero_limits() -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(0).with_frame_data_size_limit(0)
    }

    /// Counts a body, then starts the transaction's frame and a value call's, and reports the
    /// verdicts of the two starts.
    fn count_a_transaction(limit: &mut AdditionalLimit) -> [LimitCheck; 2] {
        limit.record_tx_body(crate::TX_BODY_SIZE);
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let first = limit.check_start();
        limit.on_frame_init(&call_from_to(CALLEE, TARGET, U256::from(1)), 1);
        [first, limit.check_start()]
    }

    /// A transaction that is not exempt is stopped wherever it crosses a limit: counting the
    /// body checks nothing, the first check, at its frame's start, is the stop, and a later start
    /// that crosses again is a stop of its own, with the usage it reached, not the first one's.
    #[test]
    fn test_a_transaction_that_is_not_exempt_is_stopped() {
        let mut limit = AdditionalLimit::new(zero_limits());
        let [first, records] = count_a_transaction(&mut limit);
        let stop = |used| LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 0,
            used,
            frame_local: false,
        };
        assert_eq!(first, stop(crate::TX_BODY_SIZE), "the body crosses at the first check");
        assert_eq!(records, stop(crate::TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE));
        assert!(!limit.is_exempt());
    }

    /// Validation admits a body whose bytes, with one record for each entry of the authorization
    /// list, are within the transaction's limit, and nothing over it; an exempt transaction
    /// whatever it carries.
    #[test]
    fn test_validation_admits_a_body_and_its_authority_records_within_the_limit() {
        let body = crate::TX_BODY_SIZE + 2 * crate::AUTHORIZATION_SIZE;
        let at = body + 2 * WRITE_RECORD_SIZE;
        let layer = |limit: u64, exempt: bool| {
            let mut layer = AdditionalLimit::new(
                EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
            );
            if exempt {
                layer.exempt();
            }
            layer.record_tx_body(body);
            layer
        };
        assert!(layer(at, false).admits_tx_body(2), "at the limit");
        assert!(!layer(at - 1, false).admits_tx_body(2), "one byte over it");
        assert!(layer(at - 1, false).admits_tx_body(1), "one record fewer");
        assert!(!layer(body - 1, false).admits_tx_body(0), "the body alone over it");
        assert!(layer(0, true).admits_tx_body(2), "an exempt transaction");
        assert_eq!(layer(at, false).body_bytes(), body);
        assert_eq!(layer(at, false).usage().data_size, body, "the body is counted");
    }

    /// An exempt transaction is stopped by no limit, in any dimension, at the transaction or at a
    /// frame: every verdict is within the limits and no caller is stopped. What it uses is counted
    /// all the same.
    #[test]
    fn test_an_exempt_transaction_is_stopped_by_no_limit() {
        let mut limit = AdditionalLimit::new(zero_limits());
        limit.exempt();
        assert_eq!(count_a_transaction(&mut limit), [LimitCheck::WithinLimit; 2]);
        for kind in [LimitKind::DataSize, LimitKind::ComputeGas] {
            for frame_local in [false, true] {
                let verdict = limit.crossed(kind, 0, 1, frame_local);
                assert_eq!(verdict, LimitCheck::WithinLimit, "{kind:?}, frame-local {frame_local}");
            }
        }
        assert_eq!(limit.stop_before_run(), None);
        assert_eq!(
            limit.usage(),
            LimitUsage { data_size: crate::TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE, write_records: 2 },
        );

        let result = crate::synthetic_frame_result(
            &call_from_to(CALLEE, TARGET, U256::from(1)),
            InstructionResult::Stop,
            Bytes::new(),
        );
        let _ = limit.on_frame_return(&result);
        assert_eq!(limit.stop_before_run(), None, "the caller runs on");
    }

    /// Validation admits no body whose authority records cross the data-size limit, so a debug
    /// build trips on records that do: under a limit lowered to the body and one record, two
    /// applied authorities cross it.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "validation admits no body whose authority records cross")]
    fn test_authority_records_that_cross_the_limit_trip() {
        let mut limit = AdditionalLimit::new(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_data_size_limit(crate::TX_BODY_SIZE + WRITE_RECORD_SIZE),
        );
        limit.record_tx_body(crate::TX_BODY_SIZE);
        limit.record_applied_authorities(SENDER, 2, false);
    }

    /// Applied authorities are recorded for an exempt transaction, whatever they cross.
    #[test]
    fn test_an_exempt_transaction_records_its_authorities() {
        let mut limit = AdditionalLimit::new(zero_limits());
        limit.exempt();
        limit.record_applied_authorities(SENDER, 2, false);
        assert_eq!(limit.usage(), WRITE_RECORD.times(2));
    }

    /// The exemption is the transaction's: a reset clears it, and the next transaction is held to
    /// its limits again. An out-of-gas before the first frame keeps it.
    #[test]
    fn test_the_exemption_lasts_one_transaction() {
        let mut limit = AdditionalLimit::new(zero_limits());
        limit.exempt();
        let result = crate::synthetic_frame_result(
            &call_from_to(SENDER, CALLEE, U256::ZERO),
            InstructionResult::OutOfGas,
            Bytes::new(),
        );
        limit.on_last_frame_return(&result);
        assert!(limit.is_exempt(), "an out-of-gas before the first frame keeps the exemption");

        limit.reset();
        assert!(!limit.is_exempt());
        let [first, ..] = count_a_transaction(&mut limit);
        assert!(first.exceeded_limit(), "after a reset the transaction is held to its limits");
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

        let result = crate::synthetic_frame_result(&inner, InstructionResult::Stop, Bytes::new());
        let _ = limit.on_frame_return(&result);
        let result = crate::synthetic_frame_result(&outer, InstructionResult::Stop, Bytes::new());
        limit.on_last_frame_return(&result);
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

        let result = crate::synthetic_frame_result(&inner, InstructionResult::Revert, Bytes::new());
        let _ = limit.on_frame_return(&result);
        assert_eq!(limit.usage(), LimitUsage::ZERO);
    }

    /// A frame start whose transfer log crosses a limit is stopped there, before revm moves the
    /// value, with a stop that names the limit it crossed, its own budget or the transaction's
    /// limit. The records alone
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
            limit.on_frame_init(&inner, 1);
            let check = limit.check_start();
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
            }
        }

        // The transaction's own value: one record, the recipient's, and the log.
        let first = call_from_to(SENDER, CALLEE, U256::from(1));
        let needs = WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE;
        for (tx_limit, fits) in [(needs, true), (needs - 1, false), (WRITE_RECORD_SIZE, false)] {
            let mut limit =
                logging(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(tx_limit));
            limit.on_frame_init(&first, 0);
            let check = limit.check_start();
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
        }
    }

    /// A `SELFDESTRUCT` that moved value to another account counts its transfer log with the
    /// beneficiary's record, in one check, and reports the record's history alone. To the sender,
    /// whose account the body counts, it is the log alone, and the log alone can cross.
    #[test]
    fn test_a_destruction_counts_its_transfer_log_with_its_beneficiary() {
        let moved = SelfDestructResult { had_value: true, ..Default::default() };
        let destruction =
            |target| CompletedWrite::SelfDestruct { address: CALLEE, target, result: &moved };
        let mut limit = logging(EvmTxRuntimeLimits::no_limits());
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let (check, history) = limit.commit_write(&destruction(TARGET));
        assert_eq!(check, LimitCheck::WithinLimit);
        assert_eq!(history, HistoryBytes::Appended(WRITE_RECORD_SIZE), "the record's alone");
        assert_eq!(
            limit.usage(),
            LimitUsage { data_size: WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE, write_records: 1 },
        );

        let mut limit =
            logging(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TRANSFER_LOG_SIZE - 1));
        limit.on_frame_init(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let (check, history) = limit.commit_write(&destruction(SENDER));
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
        let _ = limit.commit_write(&destruction(TARGET));
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
