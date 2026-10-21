//! The per-transaction state of the common execution layer beside the journal's ledger.

use alloy_primitives::Bytes;
use revm::{
    handler::FrameResult,
    interpreter::{CallInputs, CallScheme, FrameInput, InstructionResult},
};

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use super::{
    ledger::{FrameStartRecords, Ledger, Rollback, StartId, StartSide},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, StartReport, WRITE_RECORD_SIZE,
};
use crate::storage_call_stipend;

/// What the common execution layer tracks for the running transaction beside the journal's
/// ledger ([`Ledger`]), which counts what the transaction keeps and holds it to the data-size
/// limit: Satin's side of each frame start ([`StartSide`]), the stops its frames return, whether
/// it is exempt from every limit, and the figures the settled transaction reports.
///
/// It lives on the [`MegaContext`](crate::MegaContext), is reset before each transaction and
/// system call, and is driven by the Host's frame-start hook (the charge of a frame's start) and
/// the frame lifecycle (the frame sides, the stops).
///
/// # The stop
///
/// A limit crossed stops the frame that crossed it with a revert, not a halt: the frame reverts
/// with [`MegaLimitExceeded`](crate::MegaLimitExceeded) as its output, whether it crossed the
/// transaction's limit or its own frame budget, and its caller resumes as after any revert. What
/// the frame counted goes with its level of the ledger, so a caller that resumes is back within
/// the limits, and a transaction never keeps more than its limit: a caller that carries on is
/// held to what is left, and a charge that crosses again stops the frame that makes it. A
/// crossing the transaction's own frame makes is the transaction's result, which settles like any
/// EIP-8037 revert: its unspent regular gas and the reservoir go back to the sender, who pays only
/// for what ran. Exceptional halts (a real out-of-gas, an invalid opcode) are not limit stops and
/// still burn the frame's gas.
///
/// # The exemption
///
/// The protocol's own work is held to none of the per-transaction limits: a system-originated
/// transaction and a system call are marked exempt before their body is counted, and the mark is
/// sticky for the transaction ([`is_exempt`](Self::is_exempt)). Every stop the layer hands out
/// comes from one of two places — the ledger's verdict for the data size,
/// [`crossed`](Self::crossed) for gas detention's compute stop — and both answer an exempt
/// transaction with [`LimitCheck::WithinLimit`] whatever it crossed. What the transaction uses is
/// counted all the same, so its usage is reported as any other transaction's is.
#[derive(Clone, Debug, Default)]
pub struct AdditionalLimit {
    limits: EvmTxRuntimeLimits,
    /// Whether the transaction is exempt from every per-transaction limit, sticky for the
    /// transaction. See [`exempt`](Self::exempt).
    exempt: bool,
    /// The stop the frame about to run must return instead of running: the one a frame revm built
    /// returns on its first run when its start crossed a limit ([`hold_start`](Self::hold_start)),
    /// or the one a frame a child returned into returns when what the child left it put it over a
    /// limit ([`on_start_return`](Self::on_start_return)).
    pending_stop: Option<LimitCheck>,
    /// The crossing the result of the frame returning next was made of, noted where the stop
    /// became that result ([`note_frame_stop`](Self::note_frame_stop)) and taken when the frame
    /// returns.
    frame_stop: Option<LimitCheck>,
    /// The crossing that produced the result of the transaction's own frame, if one did. See
    /// [`limit_exceeded`](Self::limit_exceeded).
    limit_exceeded: Option<LimitCheck>,
    /// The history gas validation charged for the transaction's body, part of
    /// [`history_gas_spent`](Self::history_gas_spent).
    intrinsic_history_gas: u64,
    /// The bytes of the transaction's body that
    /// [`intrinsic_history_gas`](Self::intrinsic_history_gas) paid for, part of
    /// [`history_bytes`](Self::history_bytes).
    intrinsic_history_bytes: u64,
    /// The history gas of the write record the transaction's own frame makes — its value's
    /// recipient, or the account it creates — charged before the first frame and given back when
    /// that frame does not keep the record.
    top_level_write_record_gas: u64,
    /// The write records the transaction's own frame makes, as the charge of their history before
    /// execution found them, for the frame's side to hold the ledger's count to. See
    /// [`stage_top_level_records`](Self::stage_top_level_records).
    pending_top_level_records: Option<FrameStartRecords>,
    /// The history gas the running opcode charged its own frame for the records the frame it is
    /// starting will make, waiting for that frame's side to be pushed.
    pending_frame_charge: FrameCharge,
    /// The history gas the transaction's own frame's start gets back, kept once that frame
    /// returned for the settlement to give back
    /// ([`take_top_level_refund`](Self::take_top_level_refund)).
    top_level_refund: u64,
    /// The history gas the settled transaction spent.
    history_gas_spent: u64,
    /// The history bytes the settled transaction appended.
    history_bytes: u64,
    /// The EIP-8037 reservoir the settled transaction left unspent.
    reservoir_remaining: u64,
    /// Satin's side of every frame start, one per frame result revm will return: what the start's
    /// caller paid for its records, what the journal's ledger saw the start count, the history
    /// allowance the frame was granted, and the mark a frame revm built is taken back to when it
    /// is rewritten into a failure.
    starts: Vec<StartSide>,
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
        self.exempt = false;
        self.pending_stop = None;
        self.frame_stop = None;
        self.limit_exceeded = None;
        self.intrinsic_history_gas = 0;
        self.intrinsic_history_bytes = 0;
        self.top_level_write_record_gas = 0;
        self.pending_top_level_records = None;
        self.pending_frame_charge = FrameCharge::NONE;
        self.top_level_refund = 0;
        self.history_gas_spent = 0;
        self.history_bytes = 0;
        self.reservoir_remaining = 0;
        self.starts.clear();
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
    /// [`on_start_return`](Self::on_start_return) takes it, and the transaction's when that frame
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
    /// before its body is counted; the journal's ledger begins the transaction exempt alongside.
    /// See the type's documentation.
    pub(crate) const fn exempt(&mut self) {
        self.exempt = true;
    }

    /// The verdict on `used` crossing `kind`'s `limit`, a frame budget when `frame_local` and a
    /// transaction-level limit otherwise: the stop the running frame reverts with, alone, its
    /// [`LimitCheck::revert_data`] naming the limit crossed.
    ///
    /// Gas detention's compute stop comes from here, as the data-size stops come from the
    /// ledger's own verdict, so this is where the exemption applies to it: an exempt
    /// transaction's verdict is [`LimitCheck::WithinLimit`], whatever it crossed.
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

    /// The stop the running frame returns instead of running another instruction: the one its own
    /// start left it ([`hold_start`](Self::hold_start)), or the one
    /// [`on_start_return`](Self::on_start_return) left for a caller its child put over a limit.
    /// Taking it clears it, which stops one frame.
    pub(crate) const fn stop_before_run(&mut self) -> Option<LimitCheck> {
        self.pending_stop.take()
    }

    /// Holds the start of the frame revm just built to `verdict`, the journal's ledger's on what
    /// the start counted while the frame's level was open: a crossing is the stop the frame
    /// returns on its first run, before any instruction
    /// ([`stop_before_run`](Self::stop_before_run)). revm moved the value, journaled the transfer
    /// log and, for a creation, bumped its creator's nonce; the stop is a revert of the frame, so
    /// its journal checkpoint takes the move and the log back, and the nonce, bumped before the
    /// checkpoint, stays, with its record on the creator.
    #[inline]
    pub(crate) fn hold_start(&mut self, verdict: Option<LimitCheck>) {
        if let Some(stop) = verdict.filter(LimitCheck::exceeded_limit) {
            debug_assert!(self.pending_stop.is_none(), "a frame starts from a caller that ran");
            self.pending_stop = Some(stop);
        }
    }

    /// Holds the running frame to `check`, the journal's ledger's, once what it keeps changed
    /// outside its own instructions — a child returned into it, or a record a child left it was
    /// taken back: a crossing is the stop it returns before it runs on, and anything else clears a
    /// stop it had.
    pub(crate) fn hold_running_frame(&mut self, check: LimitCheck) {
        self.pending_stop = check.exceeded_limit().then_some(check);
    }

    /// Whether the running frame has a stop to return before it runs another instruction
    /// ([`stop_before_run`](Self::stop_before_run)).
    pub(crate) const fn has_stop_before_run(&self) -> bool {
        self.pending_stop.is_some()
    }

    /* What the settled transaction reports */

    /// The history gas the last settled transaction spent.
    pub const fn history_gas_spent(&self) -> u64 {
        self.history_gas_spent
    }

    /// Records the history gas the settled transaction spent.
    pub(crate) const fn set_history_gas_spent(&mut self, history_gas_spent: u64) {
        self.history_gas_spent = history_gas_spent;
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

    /// Settles [`history_bytes`](Self::history_bytes) from what the transaction kept — its
    /// `write_records` and its `log_and_code_bytes`, as the journal's ledger holds them once the
    /// transaction's own frame returned: `priced` is whether it pays history at all.
    pub(crate) const fn settle_history_bytes(
        &mut self,
        priced: bool,
        write_records: u64,
        log_and_code_bytes: u64,
    ) {
        self.history_bytes = if priced {
            self.intrinsic_history_bytes
                .saturating_add(WRITE_RECORD_SIZE.saturating_mul(write_records))
                .saturating_add(log_and_code_bytes)
        } else {
            0
        };
    }

    /* The charges a frame start is paid for */

    /// Records the history gas charged before the first frame for the write record that frame
    /// makes: the recipient of the transaction's value, or the account it creates. A first frame
    /// that does not keep the record gets it back at the settlement, with the rest of what the
    /// frame did not keep.
    pub(crate) const fn set_top_level_write_record_gas(&mut self, gas: u64) {
        self.top_level_write_record_gas = gas;
    }

    /// Leaves the write records the transaction's own frame makes, as the charge of their history
    /// before execution found them, for that frame's side to hold the ledger's count to once revm
    /// decided the start ([`attach_start`](Self::attach_start)).
    ///
    /// Only a transaction that pays history is charged, and so leaves one.
    pub(crate) const fn stage_top_level_records(&mut self, records: FrameStartRecords) {
        self.pending_top_level_records = Some(records);
    }

    /// Leaves the history gas the running opcode just charged its frame for the records the frame
    /// it starts will make: `added` for the records that frame's failure takes back, `creator` for
    /// the record of the creator's own nonce, and `records`, the answer the charge was computed
    /// from, for the frame's side to hold the ledger's count to; `priced` says whether the
    /// transaction prices history at all, so that a record the start takes back is history the
    /// caller gets back.
    ///
    /// The next side pushed takes it, whether that is the frame's own or that of a frame answered
    /// without running.
    #[inline]
    pub(crate) const fn stage_frame_charge(
        &mut self,
        records: FrameStartRecords,
        added: u64,
        creator: u64,
        priced: bool,
    ) {
        self.pending_frame_charge = FrameCharge { records: Some(records), added, creator, priced };
    }

    /// Leaves the frame about to start unchecked against the answer its charge was computed from
    /// — its caller's at the opcode, or the transaction's before execution for the transaction's
    /// own frame: an inspector that may rewrite the frame's input runs between the charge and the
    /// count.
    pub(crate) const fn waive_start_check(&mut self) {
        self.pending_frame_charge.records = None;
        self.pending_top_level_records = None;
    }

    /// Draws up to `amount` from the running frame's history allowance and reports what it gave;
    /// the caller pays the rest out of the frame's own gas.
    ///
    /// Only a history charge may draw, and only the one a log makes: a write record, a state
    /// charge and a unit of computation are all paid for by the frame's gas alone. See
    /// [`storage_call_stipend`](crate::storage_call_stipend).
    #[inline]
    pub(crate) fn try_consume_stipend(&mut self, amount: u64) -> u64 {
        self.starts.last_mut().map_or(0, |side| side.consume_stipend(amount))
    }

    /* The frame side */

    /// Pushes Satin's side of the start of the frame `input` asks for at `depth`, which revm
    /// decides next: what its caller paid for the records the start makes, and, for a
    /// value-transferring `CALL` or `CALLCODE` below the transaction's own frame, the history
    /// allowance the frame is granted ([`storage_call_stipend`](crate::storage_call_stipend)).
    /// What the ledger then sees the start count is attached once revm decided it
    /// ([`attach_start`](Self::attach_start)).
    pub(crate) fn push_start(&mut self, input: &FrameInput, depth: usize) {
        self.push_side(depth);
        if let FrameInput::Call(inputs) = input {
            if depth > 0 && grants_stipend(inputs) {
                if let Some(side) = self.starts.last_mut() {
                    side.grant_stipend(storage_call_stipend());
                }
            }
        }
    }

    /// Pushes the side of a frame at `depth` answered without revm deciding its start — by the
    /// depth guard, an interceptor or an inspector — which keeps the frame sides aligned with the
    /// results revm returns. Such a frame makes none of the records its caller paid for, so the
    /// whole charge comes back when its side is popped, whatever the answer was.
    pub(crate) fn push_answered_start(&mut self, depth: usize) {
        self.push_side(depth);
    }

    /// Pushes a side at `depth` carrying the charge waiting for it: the opcode's for a frame below
    /// the transaction's own, and, for the transaction's own frame, what was charged before
    /// execution for the write record it makes.
    fn push_side(&mut self, depth: usize) {
        let charge = core::mem::replace(&mut self.pending_frame_charge, FrameCharge::NONE);
        let side = if depth == 0 {
            // The transaction's own frame takes nothing back: its recipient, when it writes one,
            // is an account the transaction had not changed.
            let records = self.pending_top_level_records.take();
            StartSide::new(depth, self.top_level_write_record_gas, 0, false, records)
        } else {
            StartSide::new(depth, charge.added, charge.creator, charge.priced, charge.records)
        };
        self.starts.push(side);
    }

    /// Attaches what the journal's ledger saw the latest start count, and what a frame revm built
    /// is taken back to, to that start's side, and holds the count to the records the start's
    /// charge was computed from ([`StartSide::attach`]).
    pub(crate) fn attach_start(&mut self, report: StartReport, built: Option<(StartId, Rollback)>) {
        if let Some(side) = self.starts.last_mut() {
            side.attach(report, built);
        }
    }

    /// The side of the running frame's start, or of the start just answered.
    pub(crate) fn running_start(&self) -> Option<&StartSide> {
        self.starts.last()
    }

    /// The number of starts whose frames have not returned yet.
    pub(crate) fn starts_open(&self) -> usize {
        self.starts.len()
    }

    /// Pops the side of the start of the frame `result` returns from, once revm closed the frame's
    /// level, and settles what the start and the frame leave behind, as the journal's `ledger`
    /// holds them:
    ///
    /// - the history the caller paid for the start's records, less the price of those the ledger
    ///   still holds ([`StartSide::refund`]), which the caller gets back once the frame merged into
    ///   it. The transaction's own frame was paid for before execution: its refund is kept for the
    ///   settlement ([`take_top_level_refund`](Self::take_top_level_refund));
    /// - the caller, held to its limits with what it now holds: a failed creation's nonce record,
    ///   which outlives the creation, lands on its creator, and a crossing is the stop the caller
    ///   returns before it runs on ([`stop_before_run`](Self::stop_before_run)). Any other return
    ///   leaves the caller within its limits — a failure hands it nothing, and a success no more
    ///   than the share it gave — and is held all the same;
    /// - the crossing the result is made of, the stop noted for the frame
    ///   ([`note_frame_stop`](Self::note_frame_stop)), which is the transaction's
    ///   [`limit_exceeded`](Self::limit_exceeded) when the frame is the transaction's own and its
    ///   result is still the stop's revert.
    ///
    /// Returns the side, and the history its caller gets back.
    pub(crate) fn on_start_return(
        &mut self,
        ledger: &Ledger,
        result: &FrameResult,
    ) -> Option<(StartSide, u64)> {
        let noted = self.frame_stop.take();
        // A charge an opcode of this frame made for a child that never started died with that
        // opcode, which failed after making it.
        self.pending_frame_charge = FrameCharge::NONE;
        let side = self.starts.pop()?;
        let refund = side.refund(ledger);
        if side.depth() == 0 {
            self.limit_exceeded = noted.filter(|stop| is_the_revert_of(result, stop));
            self.top_level_refund = refund;
            return Some((side, 0));
        }
        self.hold_running_frame(ledger.check());
        Some((side, refund))
    }

    /// The history gas the transaction's own frame's start gets back, once that frame returned
    /// ([`on_start_return`](Self::on_start_return)): what the transaction paid before execution
    /// for the write record that frame makes, unless the frame kept it. The settlement takes it
    /// and gives it back; a transaction whose own frame never started gets nothing back, having
    /// paid nothing that stands.
    pub(crate) const fn take_top_level_refund(&mut self) -> u64 {
        let refund = self.top_level_refund;
        self.top_level_refund = 0;
        refund
    }
}

/// Whether `result` is the revert `stop` makes a frame return: a revert whose output is the
/// stop's [`MegaLimitExceeded`](crate::MegaLimitExceeded).
fn is_the_revert_of(result: &FrameResult, stop: &LimitCheck) -> bool {
    let result = result.interpreter_result();
    result.result == InstructionResult::Revert && result.output == stop.revert_data()
}

/// Whether the frame `inputs` starts is granted a history allowance: a value-transferring `CALL`
/// or `CALLCODE` below the transaction's own frame.
///
/// `DELEGATECALL` and `STATICCALL` carry no value, and the transaction's own frame is not a call
/// anybody made — its sender chose the gas limit.
fn grants_stipend(inputs: &CallInputs) -> bool {
    matches!(inputs.scheme, CallScheme::Call | CallScheme::CallCode) && inputs.transfers_value()
}

/// The history gas a caller paid for the records the frame it starts makes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FrameCharge {
    /// The records the charge was computed from, when an opcode made one. The transaction's own
    /// frame has no such opcode.
    records: Option<FrameStartRecords>,
    /// What the records the start adds on the frame's own level cost.
    added: u64,
    /// What the record of the creator's own nonce cost.
    creator: u64,
    /// Whether the transaction prices history.
    priced: bool,
}

impl FrameCharge {
    /// Nothing charged.
    const NONE: Self = Self { records: None, added: 0, creator: 0, priced: false };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        limit::{RecordDelta, StartFacts},
        test_utils::MemoryDatabase,
        MegaContext, MegaSpecId,
    };
    use alloy_primitives::{address, Address, Bytes, U256};
    use revm::{
        context::{ContextTr, JournalTr},
        interpreter::{CallInput, CallValue},
    };

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

    /// What the ledger reports of a start revm made and held to the limits: `records`.
    fn report(records: FrameStartRecords) -> StartReport {
        StartReport {
            start: Some(StartId::default()),
            records,
            transfer_log: false,
            verdict: Some(LimitCheck::WithinLimit),
        }
    }

    /// `added` records a call's value transfer makes on the frame's level.
    const fn transfer(added: u64) -> FrameStartRecords {
        FrameStartRecords { on_level: RecordDelta { added, taken: 0 }, ..FrameStartRecords::NONE }
    }

    /// A creation's records: its created account's on the frame's level (`created`), and its
    /// creator's own below it (`creator`).
    const fn creation(created: bool, creator: bool) -> FrameStartRecords {
        FrameStartRecords { on_level: RecordDelta::NONE, created, creator }
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

    /// The allowance is the started frame's, below the transaction's own frame: its side draws on
    /// it, and nothing is left once the side is popped. The transaction's own frame is granted
    /// none, however it was started. At a history price of nothing the allowance is nothing, and
    /// there is nothing to draw.
    #[test]
    fn test_the_allowance_is_granted_to_the_frame_a_value_call_starts() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        let ledger = ctx.ledger();
        let stipend = storage_call_stipend();
        if stipend == 0 {
            crate::test_utils::note_price_guard("MEGA_SATIN_CPHB prices a history byte at nothing");
            return;
        }
        let mut limit = AdditionalLimit::default();
        limit.push_start(&call_from_to(SENDER, CALLEE, U256::from(1)), 0);
        assert_eq!(limit.try_consume_stipend(1), 0, "the transaction's own frame has none");
        limit.push_start(&call_from_to(CALLEE, TARGET, U256::from(1)), 1);
        assert_eq!(limit.try_consume_stipend(stipend - 1), stipend - 1);
        assert_eq!(limit.try_consume_stipend(5), 1, "what is left of it");
        assert_eq!(limit.try_consume_stipend(5), 0);
        let result = crate::synthetic_frame_result(
            &call_from_to(CALLEE, TARGET, U256::from(1)),
            InstructionResult::Stop,
            Bytes::new(),
        );
        let _ = limit.on_start_return(ledger, &result);
        assert_eq!(limit.try_consume_stipend(5), 0, "the caller's own frame has none");
    }

    /// An exempt layer answers every crossing of gas detention's as within the limits, in any
    /// dimension and at the transaction or at a frame; a reset ends the exemption, and the next
    /// transaction is held to its limits again.
    #[test]
    fn test_the_exemption_answers_every_crossing_and_lasts_one_transaction() {
        let mut limit = AdditionalLimit::default();
        limit.exempt();
        assert!(limit.is_exempt());
        for kind in [LimitKind::DataSize, LimitKind::ComputeGas] {
            for frame_local in [false, true] {
                let verdict = limit.crossed(kind, 0, 1, frame_local);
                assert_eq!(verdict, LimitCheck::WithinLimit, "{kind:?}, frame-local {frame_local}");
            }
        }
        limit.reset();
        assert!(!limit.is_exempt());
        assert_eq!(
            limit.crossed(LimitKind::ComputeGas, 0, 1, false),
            LimitCheck::ExceedsLimit {
                kind: LimitKind::ComputeGas,
                limit: 0,
                used: 1,
                frame_local: false
            },
            "after a reset the transaction is held to its limits"
        );
    }

    /// The records the ledger counts for a start are the records its caller was charged for: a
    /// value transfer records the caller's account and the recipient's, the two the charge was
    /// computed from, and a creation the created account and its creator's nonce. The charge counts
    /// the created account for every creation, the address not being known at the opcode, so a
    /// creation onto an account the transaction changed already counts less than it was charged
    /// for. A start revm refused, or failed before its level opened, counted nothing and is not
    /// held to the charge.
    #[test]
    fn test_the_records_counted_are_the_records_the_caller_was_charged_for() {
        let mut limit = AdditionalLimit::default();
        limit.push_start(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        let inner = call_from_to(CALLEE, TARGET, U256::from(1));
        for (charged, counted) in [
            (transfer(2), transfer(2)),
            (creation(true, true), creation(true, true)),
            (creation(true, true), creation(false, true)),
            (creation(true, false), creation(false, false)),
        ] {
            limit.stage_frame_charge(charged, 2, 0, true);
            limit.push_start(&inner, 1);
            limit.attach_start(report(counted), None);
        }

        limit.stage_frame_charge(transfer(2), 2, 0, true);
        limit.push_start(&inner, 1);
        let refused = StartReport { verdict: None, ..report(FrameStartRecords::NONE) };
        limit.attach_start(refused, None);
    }

    /// Every record but a creation's created account is held to the charge exactly: a transfer
    /// that counts fewer records than its caller was charged for trips, as one that counts more
    /// does, and so does a creator's nonce record counted where none was charged.
    #[test]
    #[cfg(debug_assertions)]
    fn test_only_the_created_account_may_count_less_than_it_was_charged() {
        let trips = |charged: FrameStartRecords, counted: FrameStartRecords| {
            std::panic::catch_unwind(move || {
                let mut limit = AdditionalLimit::default();
                limit.push_start(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
                limit.stage_frame_charge(charged, 0, 0, true);
                limit.push_start(&call_from_to(CALLEE, TARGET, U256::from(1)), 1);
                limit.attach_start(report(counted), None);
            })
            .is_err()
        };
        assert!(trips(transfer(2), transfer(1)), "a transfer that counts one record fewer");
        assert!(trips(creation(true, false), creation(true, true)), "a creator record not charged");
        assert!(
            trips(creation(false, false), creation(true, false)),
            "a created account not charged"
        );
        assert!(
            !trips(creation(true, false), creation(false, false)),
            "a created account not made"
        );
    }

    /// A charge computed from a different answer than the count is what a rewrite that turns one
    /// kind of frame into another would make. It trips rather than passing silently, because the
    /// charge and the refund its failure gets back would both be wrong.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "changed after its caller was charged for them")]
    fn test_a_charge_computed_from_another_answer_trips() {
        let mut limit = AdditionalLimit::default();
        limit.push_start(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
        limit.stage_frame_charge(FrameStartRecords::NONE, 0, 0, true);
        limit.push_start(&call_from_to(CALLEE, TARGET, U256::from(1)), 1);
        limit.attach_start(report(transfer(2)), None);
    }

    /// The transaction's own frame is held to the records its history was charged for before
    /// execution: its value's recipient, or the account a creation makes.
    #[test]
    fn test_the_transactions_own_frame_counts_the_records_its_history_paid_for() {
        for records in [transfer(0), transfer(1), creation(true, false)] {
            let mut limit = AdditionalLimit::default();
            limit.stage_top_level_records(records);
            limit.push_start(&call_from_to(SENDER, CALLEE, U256::from(1)), 0);
            limit.attach_start(report(records), None);
        }
    }

    /// A transaction's own frame that makes another record than its history paid for trips the
    /// check, rather than counting data size no history matches or keeping history no data size
    /// matches.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "changed after its caller was charged for them")]
    fn test_a_top_level_record_its_history_did_not_pay_for_trips() {
        let mut limit = AdditionalLimit::default();
        limit.stage_top_level_records(transfer(0));
        limit.push_start(&call_from_to(SENDER, CALLEE, U256::from(1)), 0);
        limit.attach_start(report(transfer(1)), None);
    }

    /// Under an inspector that may rewrite a start, the check is waived: the start its charge paid
    /// for may not be the one revm made.
    #[test]
    fn test_a_waived_start_is_not_held_to_its_charge() {
        let mut limit = AdditionalLimit::default();
        limit.stage_top_level_records(transfer(0));
        limit.waive_start_check();
        limit.push_start(&call_from_to(SENDER, CALLEE, U256::from(1)), 0);
        limit.attach_start(report(transfer(1)), None);
        limit.stage_frame_charge(FrameStartRecords::NONE, 0, 0, true);
        limit.waive_start_check();
        limit.push_start(&call_from_to(CALLEE, TARGET, U256::from(1)), 1);
        limit.attach_start(report(transfer(2)), None);
    }

    /// A frame answered without running — by the depth guard, a stop, an interceptor or an
    /// inspector — keeps none of the records its caller paid for, whatever its answer: the whole
    /// charge comes back when its side is popped, and the caller it returns into is held to the
    /// ledger's check.
    ///
    /// The depth guard's answer is reached here rather than through a transaction: under the
    /// execution cap no call chain reaches the call-stack limit, because each call forwards at most
    /// sixty-three sixty-fourths of what its caller has left, and 200,000,000 gas leaves about
    /// twenty at depth 1,024. A stop's and an interceptor's answers are also reached through
    /// transactions, in the history-byte tests.
    #[test]
    fn test_a_frame_answered_without_running_gives_the_whole_charge_back() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        let ledger = ctx.ledger();
        for answer in
            [InstructionResult::CallTooDeep, InstructionResult::Revert, InstructionResult::Stop]
        {
            let mut limit = AdditionalLimit::default();
            limit.push_start(&call_from_to(SENDER, CALLEE, U256::ZERO), 0);
            let inner = call_from_to(CALLEE, TARGET, U256::from(1));
            limit.stage_frame_charge(creation(true, true), 1_000, 2_000, true);
            limit.push_answered_start(1);
            let result = crate::synthetic_frame_result(&inner, answer, Bytes::new());
            let (_, refund) = limit.on_start_return(ledger, &result).expect("the side");
            assert_eq!(refund, 3_000, "{answer:?}: all of it back");
            assert_eq!(limit.stop_before_run(), None, "{answer:?}: the caller runs on");
            assert_eq!(limit.starts_open(), 1, "{answer:?}: the caller's side is left");
        }
    }

    /// The transaction's own frame's side keeps what was charged before execution for the write
    /// record the frame makes, and gives it to the settlement when the frame did not keep the
    /// record: a frame that failed, and a frame answered before revm decided it. A frame that kept
    /// its record gives nothing back, and a transaction whose first frame never started has
    /// nothing to give back.
    ///
    /// The record's history is charged at the price in effect, as pre-execution charges it, so the
    /// refunds hold at any history byte price, a price of nothing included.
    #[test]
    fn test_the_top_level_record_comes_back_once_at_the_settlement() {
        let charge = crate::write_record_history_gas(1).unwrap_or(0);
        let mut ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        let outer = call_from_to(SENDER, CALLEE, U256::from(1));
        let ended = |result| crate::synthetic_frame_result(&outer, result, Bytes::new());
        // The start kept: a level revm committed.
        for (case, failed, refund) in [("kept", false, 0), ("failed", true, charge)] {
            let mut limit = AdditionalLimit::default();
            limit.set_top_level_write_record_gas(charge);
            limit.push_start(&outer, 0);
            ctx.ledger_mut().begin_transaction(false, false, 0);
            ctx.ledger_mut().open_start(StartFacts {
                depth: 0,
                creation: false,
                transfers_value: true,
            });
            let checkpoint = ctx.journal_mut().checkpoint();
            ctx.ledger_mut().on_transfer(RecordDelta { added: 1, taken: 0 }, false, false);
            let report = ctx.ledger_mut().close_start();
            limit.attach_start(report, None);
            ctx.ledger_mut().frame_returning();
            if failed {
                ctx.journal_mut().checkpoint_revert(checkpoint);
            } else {
                ctx.journal_mut().checkpoint_commit();
            }
            let result =
                ended(if failed { InstructionResult::Revert } else { InstructionResult::Stop });
            let (side, caller) = limit.on_start_return(ctx.ledger(), &result).expect("the side");
            assert_eq!(side.depth(), 0, "{case}");
            assert_eq!(caller, 0, "{case}: the transaction's own frame has no caller to refund");
            assert_eq!(limit.take_top_level_refund(), refund, "{case}");
            assert_eq!(limit.take_top_level_refund(), 0, "{case}: taken once");
        }

        // Answered before revm decided it: nothing counted, everything back.
        let mut limit = AdditionalLimit::default();
        limit.set_top_level_write_record_gas(charge);
        limit.push_answered_start(0);
        let _ = limit.on_start_return(ctx.ledger(), &ended(InstructionResult::Stop));
        assert_eq!(limit.take_top_level_refund(), charge);

        // Never started: no side, nothing back.
        let mut limit = AdditionalLimit::default();
        limit.set_top_level_write_record_gas(charge);
        assert!(limit.on_start_return(ctx.ledger(), &ended(InstructionResult::OutOfGas)).is_none());
        assert_eq!(limit.take_top_level_refund(), 0);
    }

    /// The stop the transaction reports is the crossing its own frame's result is made of, read as
    /// that frame returns, and only while the result is still the stop's revert.
    #[test]
    fn test_limit_exceeded_is_the_stop_the_own_frames_result_is_made_of() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        let outer = call_from_to(SENDER, CALLEE, U256::ZERO);
        let stop = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 3,
            used: 4,
            frame_local: false,
        };
        for (case, output, result, reported) in [
            ("the stop's revert", stop.revert_data(), InstructionResult::Revert, Some(stop)),
            ("another output", Bytes::new(), InstructionResult::Revert, None),
            ("a success", stop.revert_data(), InstructionResult::Return, None),
        ] {
            let mut limit = AdditionalLimit::default();
            limit.push_start(&outer, 0);
            let data = limit.note_frame_stop(&stop);
            assert_eq!(data, stop.revert_data(), "{case}");
            let result = crate::synthetic_frame_result(&outer, result, output);
            let _ = limit.on_start_return(ctx.ledger(), &result);
            assert_eq!(limit.limit_exceeded().copied(), reported, "{case}");
        }
    }
}
