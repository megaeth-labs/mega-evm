//! The per-transaction state of the common execution layer.

use alloy_primitives::Bytes;
use revm::{
    context_interface::cfg::gas::GasTracker, handler::FrameResult, interpreter::InstructionResult,
};

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use super::{
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE,
};

/// What the common execution layer tracks for the running transaction: what it keeps of the
/// data-size limit and the frame budgets, the stops its frames return, whether it is exempt from
/// every limit, and the figures the settled transaction reports.
///
/// It lives on the [`MegaContext`](crate::MegaContext), is begun before each transaction and
/// system call, and is driven by the Host's hooks (a completed write, a frame's start, a code
/// deposit) and by the frame lifecycle (a start revm decided, a frame's return, the stops).
///
/// # The data size
///
/// A frame's data-size budget is the byte pool on its gas tracker, which the revm fork forwards
/// and settles as it does the reservoir: the transaction's own frame gets what the transaction has
/// left once its body and the records made before it are counted
/// ([`first_frame_pool`](Self::first_frame_pool)), every child the schedule's share of what its
/// caller has left (98/100, `GasId::bytes_forward_percent`) under the frame cap, a failing frame's
/// draws go back to its pool and a successful one hands what it kept to its caller. A byte is drawn
/// where it is made, on the pool of the frame that makes it ([`draw`](Self::draw)): a completed
/// write's record or log, a frame start's records and transfer log on the started frame, deployed
/// code on the creating frame. A creator's nonce record is the creating frame's own, drawn on its
/// pool before the creation's share is forwarded
/// ([`count_creator_record`](Self::count_creator_record)).
///
/// The transaction's limit is held beside the pools, on what the transaction keeps
/// ([`usage`](Self::usage)): its body, its Oracle hints, the records of its applied authorities,
/// and what its frames keep ([`Kept`]), counted with every draw and restored to what it was at a
/// start when that start's frame kept nothing ([`close_start`](Self::close_start)). A pool is one
/// figure, the bytes; the write-record count and the transfer logs the transaction reports apart
/// from the bytes are kept beside them, and restored with them.
///
/// # The stop
///
/// A limit crossed stops the frame that crossed it with a revert, not a halt: the frame reverts
/// with [`MegaLimitExceeded`](crate::MegaLimitExceeded) as its output, whether it crossed the
/// transaction's limit or its own frame budget, and its caller resumes as after any revert. What
/// the frame counted goes with it, so a caller that resumes is back within the limits, and a
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
    /// Whether the transaction is exempt from every per-transaction limit, sticky for the
    /// transaction. See [`is_exempt`](Self::is_exempt).
    exempt: bool,
    /// Whether a value movement journals an EIP-7708 transfer log.
    transfer_logs: bool,
    /// The stop the frame about to run must return instead of running: the one a frame revm built
    /// returns on its first run when its start crossed a limit ([`hold_start`](Self::hold_start)),
    /// or the one a frame a child returned into returns when what the child left it put it over a
    /// limit ([`hold_caller`](Self::hold_caller)).
    pending_stop: Option<LimitCheck>,
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
    /// What the transaction's frames keep, every running frame counted as if it succeeds.
    kept: Kept,
    /// One per start revm decided whose result has not returned, the latest last.
    decided: Vec<Decided>,
    /// The record of its own nonce the last creation's start hook charged its creator for, until
    /// a frame start takes it.
    pending_creator: Option<CreatorRecord>,
    /// Whether the result returning next kept nothing, whatever it says: an inspector revived a
    /// frame that failed.
    kept_nothing: bool,
    /// The history gas of the transaction's body, part of
    /// [`history_gas_spent`](Self::history_gas_spent).
    intrinsic_history_gas: u64,
    /// The bytes of the body that [`intrinsic_history_gas`](Self::intrinsic_history_gas) paid
    /// for, part of [`history_bytes`](Self::history_bytes).
    intrinsic_history_bytes: u64,
    /// The history gas charged before the first frame for the write record that frame makes,
    /// which the frame carries as its start charge.
    first_frame_charge: u64,
    /// The history gas the settled transaction spent.
    history_gas_spent: u64,
    /// The history bytes the settled transaction appended.
    history_bytes: u64,
    /// The EIP-8037 reservoir the settled transaction left unspent.
    reservoir_remaining: u64,
}

/// What a transaction's frames keep: the data-size bytes they drew from their pools, and, among
/// them, the write records and the EIP-7708 transfer logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Kept {
    /// Data-size bytes.
    bytes: u64,
    /// Write records.
    records: u64,
    /// EIP-7708 transfer logs.
    transfer_logs: u64,
}

/// What one write, or one frame start, adds to what a frame keeps and takes back from it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counted {
    /// Data-size bytes added: records, a log's bytes, deployed code, a transfer log.
    pub(crate) added: u64,
    /// Data-size bytes taken back: the records of accounts and slots returned to what the
    /// transaction loaded.
    pub(crate) taken: u64,
    /// Write records added.
    pub(crate) records_added: u64,
    /// Write records taken back.
    pub(crate) records_taken: u64,
    /// Whether an EIP-7708 transfer log is among the bytes added.
    pub(crate) transfer_log: bool,
}

impl Counted {
    /// Nothing counted.
    pub(crate) const NONE: Self =
        Self { added: 0, taken: 0, records_added: 0, records_taken: 0, transfer_log: false };

    /// `added` write records made and `taken` taken back, with an EIP-7708 transfer log when
    /// `transfer_log`.
    pub(crate) const fn records(added: u64, taken: u64, transfer_log: bool) -> Self {
        let log = if transfer_log { TRANSFER_LOG_SIZE } else { 0 };
        Self {
            added: WRITE_RECORD_SIZE.saturating_mul(added).saturating_add(log),
            taken: WRITE_RECORD_SIZE.saturating_mul(taken),
            records_added: added,
            records_taken: taken,
            transfer_log,
        }
    }

    /// `bytes` bytes that are no write record: a log's, or deployed code.
    pub(crate) const fn bytes(bytes: u64) -> Self {
        Self { added: bytes, ..Self::NONE }
    }

    /// Whether it adds nothing and takes nothing back.
    const fn is_none(&self) -> bool {
        self.added == 0 && self.taken == 0
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
    /// The bytes of a creator's nonce record the frame's own pool could not pay when the frame
    /// created, which it owes until the creation returns into it.
    overdraft: u64,
}

/// The record of its own nonce a creation's start hook charged its creator for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CreatorRecord {
    /// The history gas it cost.
    pub(crate) history: u64,
    /// The bytes it drew from the creator's pool.
    pub(crate) drawn: u64,
    /// The bytes the creator's pool could not pay, which the creator owes.
    pub(crate) overdraft: u64,
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
    /// exempt from every limit or not, its value movements journaling transfer logs or not, its
    /// body weighing `body` bytes.
    ///
    /// The exemption is decided before the body is counted: the protocol's own work is exempt
    /// whatever it carries.
    pub(crate) fn begin_transaction(&mut self, exempt: bool, transfer_logs: bool, body: u64) {
        self.exempt = exempt;
        self.transfer_logs = transfer_logs;
        self.pending_stop = None;
        self.frame_stop = None;
        self.limit_exceeded = None;
        self.body = body;
        self.hints = 0;
        self.authorities = 0;
        self.kept = Kept::default();
        self.decided.clear();
        self.pending_creator = None;
        self.kept_nothing = false;
        self.intrinsic_history_gas = 0;
        self.intrinsic_history_bytes = 0;
        self.first_frame_charge = 0;
        self.history_gas_spent = 0;
        self.history_bytes = 0;
        self.reservoir_remaining = 0;
    }

    /* What the transaction keeps */

    /// What the transaction keeps so far: its data-size bytes and write records, every running
    /// frame counted as if it succeeds.
    pub(crate) const fn usage(&self) -> LimitUsage {
        LimitUsage {
            data_size: self
                .body
                .saturating_add(self.hints)
                .saturating_add(WRITE_RECORD_SIZE.saturating_mul(self.authorities))
                .saturating_add(self.kept.bytes),
            write_records: self.authorities.saturating_add(self.kept.records),
        }
    }

    /// The data-size bytes of the transaction's body, which it counts before any frame and no
    /// revert takes back.
    pub(crate) const fn body(&self) -> u64 {
        self.body
    }

    /// Whether the transaction's value movements journal EIP-7708 transfer logs.
    pub(crate) const fn counts_transfer_logs(&self) -> bool {
        self.transfer_logs
    }

    /// Whether the transaction's body, with one write record for each of its `authorizations`,
    /// fits its data-size limit: what validation holds a transaction to.
    ///
    /// Every entry of the authorization list counts, whether it will apply or not, so the figure is
    /// an upper bound on what the transaction counts before its first frame — the body, then one
    /// record for each distinct authority that applies — and a transaction validation admits
    /// cannot cross the limit there. An exempt transaction is held to nothing.
    pub(crate) const fn admits_tx_body(&self, authorizations: u64) -> bool {
        self.exempt ||
            self.body.saturating_add(WRITE_RECORD_SIZE.saturating_mul(authorizations)) <=
                self.limits.tx_data_size_limit
    }

    /// Counts the records of the applied EIP-7702 authorities, `authorities` distinct accounts
    /// other than the transaction's sender, made before the first frame. An out-of-gas before the
    /// first frame takes them back ([`take_back_authorities`](Self::take_back_authorities)).
    ///
    /// The records cannot cross the data-size limit: validation admitted the body with a record for
    /// every entry of the authorization list ([`admits_tx_body`](Self::admits_tx_body)), and only
    /// an entry can apply. A debug build asserts it.
    pub(crate) fn count_authorities(&mut self, authorities: u64) {
        self.authorities = authorities;
        debug_assert!(
            self.exempt || !self.usage().exceeds_data_size(self.limits.tx_data_size_limit),
            "validation admits no body whose authority records cross the data-size limit"
        );
    }

    /// Takes back the authorities' records, which an out-of-gas before the first frame takes back
    /// with the authorizations.
    pub(crate) const fn take_back_authorities(&mut self) {
        self.authorities = 0;
    }

    /// The pool of the transaction's own frame: what the transaction has left of its data-size
    /// limit once its body and the records made before the frame are counted, under the frame
    /// cap; unlimited for an exempt transaction.
    pub(crate) const fn first_frame_pool(&self) -> u64 {
        let left = self.limits.tx_data_size_limit.saturating_sub(self.usage().data_size);
        let cap = self.frame_cap();
        if self.exempt {
            u64::MAX
        } else if left < cap {
            left
        } else {
            cap
        }
    }

    /// The most a frame's pool may start with: the frame cap, and nothing for an exempt
    /// transaction.
    pub(crate) const fn frame_cap(&self) -> u64 {
        if self.exempt {
            u64::MAX
        } else {
            self.limits.frame_data_size_limit
        }
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
        if self.exempt {
            return LimitCheck::WithinLimit;
        }
        LimitCheck::ExceedsLimit { kind, limit, used, frame_local }
    }

    /// The verdict on the transaction keeping `counted` more: its data-size limit crossed, or
    /// within.
    const fn check_transaction(&self, counted: &Counted) -> LimitCheck {
        let used =
            self.usage().data_size.saturating_sub(counted.taken).saturating_add(counted.added);
        if used > self.limits.tx_data_size_limit {
            return self.crossed(LimitKind::DataSize, self.limits.tx_data_size_limit, used, false);
        }
        LimitCheck::WithinLimit
    }

    /// The verdict on a frame budget crossed by `added` bytes the pool `gas` holds could not pay:
    /// the budget is the pool the frame started with, which its pool and its net draw add up to,
    /// and what it used is its net draw with `added` in.
    fn budget_crossed(&self, gas: &GasTracker, added: u64) -> LimitCheck {
        let budget = gas.bytes_remaining().saturating_add_signed(gas.bytes_spent());
        let used = gas.bytes_spent().saturating_add_unsigned(added).max(0).unsigned_abs();
        self.crossed(LimitKind::DataSize, budget, used, true)
    }

    /// Counts what the frame whose pool `gas` holds keeps of `counted`, held to the transaction's
    /// limit, then to the frame's budget: the bytes `counted` takes back go back to the pool
    /// first, and the bytes it adds are drawn; a draw the pool cannot pay is the frame's budget
    /// crossed. A crossing counts nothing and draws nothing, and is the stop the frame reverts
    /// with. A count that adds nothing holds nothing.
    pub(crate) fn draw(&mut self, gas: &mut GasTracker, counted: &Counted) -> LimitCheck {
        if counted.is_none() {
            return LimitCheck::WithinLimit;
        }
        if counted.added != 0 {
            let check = self.check_transaction(counted);
            if check.exceeded_limit() {
                return check;
            }
        }
        gas.refill_bytes(counted.taken);
        if !gas.record_bytes(counted.added) {
            gas.clear_bytes_crossing();
            let check = self.budget_crossed(gas, counted.added);
            let drew = gas.record_bytes(counted.taken);
            debug_assert!(drew, "what was given back can be drawn again");
            return check;
        }
        self.count(counted);
        LimitCheck::WithinLimit
    }

    /// Adds `counted` to what the frames keep.
    fn count(&mut self, counted: &Counted) {
        let kept = &mut self.kept;
        debug_assert!(
            kept.bytes >= counted.taken && kept.records >= counted.records_taken,
            "a frame takes back only what the transaction's frames keep"
        );
        kept.bytes = kept.bytes.saturating_sub(counted.taken).saturating_add(counted.added);
        kept.records = kept
            .records
            .saturating_sub(counted.records_taken)
            .saturating_add(counted.records_added);
        kept.transfer_logs += u64::from(counted.transfer_log);
    }

    /// The verdict on a start revm is not asked to decide — a call gas detention answers without
    /// running the precompile it targets — had revm made it: `counted`, the records and transfer
    /// log its value transfer would make, held to the transaction's limit and to `budget`, the
    /// pool its frame would start with. Nothing is counted.
    pub(crate) const fn hypothetical_start(&self, counted: &Counted, budget: u64) -> LimitCheck {
        let check = self.check_transaction(counted);
        if check.exceeded_limit() {
            return check;
        }
        let used = counted.added.saturating_sub(counted.taken);
        if used > budget {
            return self.crossed(LimitKind::DataSize, budget, used, true);
        }
        LimitCheck::WithinLimit
    }

    /// Admits an Oracle hint of `bytes`: held to the transaction's limit before it is counted, and
    /// never taken back once counted. A hint is on no frame's budget.
    pub(crate) fn admit_hint(&mut self, bytes: u64) -> LimitCheck {
        let check = self.check_transaction(&Counted::bytes(bytes));
        if check.within_limit() {
            self.hints = self.hints.saturating_add(bytes);
        }
        check
    }

    /* A frame's start */

    /// Counts the record of a creator's own nonce, which a creation's start makes on the creating
    /// frame, whose pool `gas` holds, before the frame's share is forwarded: the creation's budget
    /// is taken from what the creator has left once the record is counted. It lands on the creator
    /// unchecked, and the creator is held to its limits with it once the creation returns
    /// ([`hold_caller`](Self::hold_caller)); a pool that cannot pay it is drained, and the creator
    /// owes the rest until then. `history` is what the creator paid for it.
    pub(crate) fn count_creator_record(&mut self, gas: &mut GasTracker, history: u64) {
        self.count(&Counted::records(1, 0, false));
        let drawn = WRITE_RECORD_SIZE.min(gas.bytes_remaining());
        let drew = gas.record_bytes(drawn);
        debug_assert!(drew, "the draw is what the pool holds");
        let overdraft = WRITE_RECORD_SIZE - drawn;
        if overdraft != 0 {
            if let Some(running) = self.decided.last_mut() {
                running.overdraft += overdraft;
            }
        }
        self.pending_creator = Some(CreatorRecord { history, drawn, overdraft });
    }

    /// The record of its own nonce the last creation's start hook charged its creator for, which
    /// the start's decision takes: revm deciding it keeps it, an inspector answering in its place
    /// gives it back ([`give_back_creator_record`](Self::give_back_creator_record)).
    pub(crate) const fn take_creator_record(&mut self) -> Option<CreatorRecord> {
        self.pending_creator.take()
    }

    /// Takes back the record of its own nonce a creation's start counted on its creator, for a
    /// creation an inspector answered in revm's place: the start never bumped the nonce. What the
    /// creator paid for it goes back through the creation's answer.
    pub(crate) fn give_back_creator_record(&mut self, record: CreatorRecord) {
        self.count(&Counted::records(0, 1, false));
        if let Some(running) = self.decided.last_mut() {
            running.overdraft -= record.overdraft;
        }
    }

    /// Opens the start of the frame at `depth` that revm just decided, before its records are
    /// counted: what the frames keep now is what the frame's failure restores
    /// ([`close_start`](Self::close_start)).
    pub(crate) fn open_start(&mut self, depth: usize) {
        self.decided.push(Decided { depth, at_start: self.kept, credit: 0, overdraft: 0 });
    }

    /// Holds the start revm just decided, whose frame's pool `gas` holds, to the limits, and counts
    /// `counted`, the records and transfer log it made, on that pool ([`draw`](Self::draw)): the
    /// verdict is the stop the frame returns before it runs, or the one its answer gives way to.
    /// `credit` is what the start's caller gets back once the frame returned with its start kept.
    pub(crate) fn count_start(
        &mut self,
        gas: &mut GasTracker,
        counted: &Counted,
        credit: u64,
    ) -> LimitCheck {
        let check = self.draw(gas, counted);
        if check.within_limit() {
            if let Some(start) = self.decided.last_mut() {
                start.credit = credit;
            }
        }
        check
    }

    /// Notes that the result returning next kept nothing, whatever it says: an inspector revived
    /// a frame that failed.
    pub(crate) const fn kept_nothing(&mut self) {
        self.kept_nothing = true;
    }

    /// Closes the start of the frame whose result returns at `depth`, `kept` or not, and answers
    /// the history its caller gets back for it: a frame that kept nothing — it failed, or an
    /// inspector revived it — has what the frames keep restored to what they kept when its start
    /// began, and its caller gets nothing back; one that kept its start, the start's credit.
    ///
    /// A result revm did not decide the start of — the depth guard's, an interceptor's, an
    /// inspector's — has no start to close: what its caller paid comes back with its start charge.
    pub(crate) fn close_start(&mut self, depth: usize, kept: bool) -> u64 {
        let kept_nothing = core::mem::take(&mut self.kept_nothing);
        let kept = kept && !kept_nothing;
        let Some(&start) = self.decided.last().filter(|start| start.depth == depth) else {
            return 0;
        };
        self.decided.pop();
        if kept {
            start.credit
        } else {
            self.kept = start.at_start;
            0
        }
    }

    /// Holds the frame a child just returned into, whose pool `gas` holds, to its limits with what
    /// it now keeps: a creation's nonce record, which outlives the creation, lands on its creator
    /// unchecked ([`count_creator_record`](Self::count_creator_record)), and a crossing is the stop
    /// the creator returns before it runs on ([`stop_before_run`](Self::stop_before_run)). The
    /// transaction's limit is held first. A record the creator's pool could not pay is drawn now if
    /// the pool can pay it; a creator still owing it crossed its budget.
    ///
    /// Any other return leaves the caller within its limits — a failure hands it nothing, and a
    /// success no more than the share it gave — and clears a stop it had.
    pub(crate) fn hold_caller(&mut self, gas: &mut GasTracker) {
        let check = self.check_transaction(&Counted::NONE);
        self.pending_stop = check.exceeded_limit().then_some(check);
        if self.pending_stop.is_some() {
            return;
        }
        let Some(running) = self.decided.last_mut() else { return };
        let overdraft = running.overdraft;
        if overdraft == 0 {
            return;
        }
        if gas.record_bytes(overdraft) {
            running.overdraft = 0;
            return;
        }
        gas.clear_bytes_crossing();
        let stop = self.budget_crossed(gas, overdraft);
        self.pending_stop = stop.exceeded_limit().then_some(stop);
    }

    /// Takes back one write record of the running frame, whose pool `gas` holds, for an account a
    /// write outside the hooks returned to what the transaction loaded: a `keylessDeploy` call's
    /// signer, whose nonce the settlement sets back. A record the frame still owes is forgiven
    /// first.
    pub(crate) fn take_back_record(&mut self, gas: &mut GasTracker) {
        let forgiven = self.decided.last_mut().map_or(0, |running| {
            let forgiven = running.overdraft.min(WRITE_RECORD_SIZE);
            running.overdraft -= forgiven;
            forgiven
        });
        gas.refill_bytes(WRITE_RECORD_SIZE - forgiven);
        self.count(&Counted::records(0, 1, false));
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

    /// Whether the running transaction is exempt from every per-transaction limit.
    pub const fn is_exempt(&self) -> bool {
        self.exempt
    }

    /// The stop the running frame returns instead of running another instruction: the one its own
    /// start left it ([`hold_start`](Self::hold_start)), or the one
    /// [`hold_caller`](Self::hold_caller) left for a caller its child put over a limit. Taking it
    /// clears it, which stops one frame.
    pub(crate) const fn stop_before_run(&mut self) -> Option<LimitCheck> {
        self.pending_stop.take()
    }

    /// Holds the start of the frame revm just built to `verdict`: a crossing is the stop the frame
    /// returns on its first run, before any instruction
    /// ([`stop_before_run`](Self::stop_before_run)). revm moved the value, journaled the transfer
    /// log and, for a creation, bumped its creator's nonce; the stop is a revert of the frame, so
    /// its journal checkpoint takes the move and the log back, and the nonce, bumped before the
    /// checkpoint, stays, with its record on the creator.
    #[inline]
    pub(crate) fn hold_start(&mut self, verdict: LimitCheck) {
        if verdict.exceeded_limit() {
            debug_assert!(self.pending_stop.is_none(), "a frame starts from a caller that ran");
            self.pending_stop = Some(verdict);
        }
    }

    /// Whether the running frame has a stop to return before it runs another instruction
    /// ([`stop_before_run`](Self::stop_before_run)).
    pub(crate) const fn has_stop_before_run(&self) -> bool {
        self.pending_stop.is_some()
    }

    /* What the transaction pays before its first frame */

    /// The history gas of the transaction's body: the bytes the transaction carries and the write
    /// records its inclusion makes, which are known before it runs. The transaction's gas pays it
    /// before the first frame, reservoir first, as intrinsic gas.
    pub const fn intrinsic_history_gas(&self) -> u64 {
        self.intrinsic_history_gas
    }

    /// Records the history gas the transaction's body costs, and the `bytes` of the body it pays
    /// for.
    pub(crate) const fn set_intrinsic_history(&mut self, gas: u64, bytes: u64) {
        self.intrinsic_history_gas = gas;
        self.intrinsic_history_bytes = bytes;
    }

    /// Records the history gas charged before execution for the write record the transaction's own
    /// frame makes: the recipient of its value, or the account it creates. The frame carries it as
    /// its start charge, which its failure gives back.
    pub(crate) const fn set_first_frame_charge(&mut self, gas: u64) {
        self.first_frame_charge = gas;
    }

    /// The history gas charged before execution for the write record the transaction's own frame
    /// makes.
    pub(crate) const fn first_frame_charge(&self) -> u64 {
        self.first_frame_charge
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
        self.reservoir_remaining
    }

    /// Records the reservoir the settled transaction or system call left unspent.
    pub(crate) const fn set_reservoir_remaining(&mut self, reservoir: u64) {
        self.reservoir_remaining = reservoir;
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
                .saturating_add(self.kept.bytes)
                .saturating_sub(TRANSFER_LOG_SIZE.saturating_mul(self.kept.transfer_logs))
        } else {
            0
        };
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

    /// An exempt layer answers every crossing as within the limits, in any dimension and at the
    /// transaction or at a frame; the next transaction begins held to its limits again.
    #[test]
    fn test_the_exemption_answers_every_crossing_and_lasts_one_transaction() {
        let mut limit = AdditionalLimit::default();
        limit.begin_transaction(true, false, 0);
        assert!(limit.is_exempt());
        for kind in [LimitKind::DataSize, LimitKind::ComputeGas] {
            for frame_local in [false, true] {
                let verdict = limit.crossed(kind, 0, 1, frame_local);
                assert_eq!(verdict, LimitCheck::WithinLimit, "{kind:?}, frame-local {frame_local}");
            }
        }
        limit.begin_transaction(false, false, 0);
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
}
