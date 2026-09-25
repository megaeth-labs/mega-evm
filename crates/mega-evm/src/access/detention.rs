//! The state of gas detention for the running transaction.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{cell::Cell, num::NonZeroU64};
use std::vec::Vec;

use alloy_primitives::{Bytes, U256};
use revm::interpreter::{gas::WithheldCrossing, Gas, InstructionResult, InterpreterResult};

use super::VolatileDataAccess;
use crate::system::VOLATILE_DATA_ACCESS_DISABLED_SELECTOR;

/// What detention keeps of one frame that runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DetainedFrame {
    /// The frame's regular gas spent when it suspended on a child ([`regular_spent`]).
    at_suspension: u64,
    /// While a child of the frame runs: what the frame adds to the transaction's compute, which is
    /// [`at_suspension`](Self::at_suspension) less the child's gas limit.
    contribution: u64,
}

/// Gas detention for the running transaction: which volatile data it read, and the most compute
/// it may reach because of it.
///
/// The `access` module describes the mechanism.
///
/// # Compute
///
/// Compute is the regular gas the transaction spends on what it runs. A frame's regular gas spent
/// is read off its [`Gas`]: its limit, less what it has left, less the state and history gas that
/// spilled onto its regular gas, which are not compute. What it has left is the whole of its
/// regular gas, the part withheld from regular charges included, so withheld gas is never spent,
/// and every other reader of the frame's gas sees the same figure.
///
/// The transaction's compute is its frames' compute: the frame that runs, and every frame
/// suspended on a child that runs, as it stood when it suspended, less the child's gas limit. A
/// value call's stipend is part of that limit and gas nobody paid, so taking the whole limit off
/// takes the stipend off too, as the caller's regular gas spent does once the child returned. So
/// the figure follows the regular ledger of the transaction's frames at every moment, before,
/// during and after each child:
///
/// ```text
/// compute = Σ suspended callers (spent at suspension − child's gas limit)
///         + regular_spent(running frame)
///         − burned
/// ```
///
/// The stipend stays outside compute on purpose: compute is regular gas drawn from the
/// transaction's own pools. A callee may run up to 2,300 gas on each value call's stipend beyond
/// the compute counted, and each value call costs its caller at least 9,100 of compute — the
/// 9,000 of the value transfer and a warm access of 100 — so the gas run after a read is at most
/// about 25% (2,300 / 9,100) more than the cap.
///
/// A keyless deployment's call is the transaction's own frame, and runs no code: it charges its
/// own work — the overhead of decoding the signed transaction and recovering its signer, and the
/// `CREATE` opcode's regular gas — on its own gas, held to the limit as any frame's is when it
/// runs, then suspends on its creation. Its charges are compute as a caller's are, whether a rule
/// then refuses the call or its creation runs.
///
/// The one part of the regular ledger that is not compute is what a halt burns (`burned`): a
/// frame that halts consumes the gas it had left, and its spill, without running anything with
/// it. A frame answered without running that halts burns its whole gas limit. An out-of-gas zeroes
/// what the frame had before the frame returns; the wrapper of the opcode whose charge failed
/// notes it first, `EXP`'s and the unbounded charges of `KECCAK256` and the copies into memory
/// included.
///
/// The one charge no wrapper sees is an opcode's static gas, which the interpreter's step loop
/// makes before the opcode runs. When it fails, what the halting frame had left counts as
/// compute: per halting frame, less than the failed charge's price, so at most 4,999, on
/// `SELFDESTRUCT`. Burned gas is counted as compute, so the stop comes earlier, never later. A
/// frame whose leftover takes the compute past the limit does so without a crossing, so the stop
/// is its caller's next charge, and the regular ledger at the stop holds that leftover's part
/// past the limit beside the limit.
///
/// # Refused reads
///
/// A frame in a subtree where volatile-data access is switched off does not read volatile data:
/// the Host refuses the load and the opcode's wrapper reverts the frame with
/// `VolatileDataAccessDisabled`. The switch is scoped to the frame that turned it off and the
/// frames below it, and turns back on when that frame returns. `MegaAccessControl`'s interceptor
/// steers it for the frame that calls the contract.
#[derive(Clone, Debug, Default)]
pub struct Detention {
    /// What the Host loaded for the running opcode, waiting for its wrapper.
    observed: Cell<VolatileDataAccess>,
    /// What the Host refused to load for the running opcode, waiting for its wrapper.
    refused: Cell<VolatileDataAccess>,
    /// Whether the transaction's reads are held to a cap. A system-originated transaction's are
    /// not, and neither are a transaction's whose caps are both unlimited.
    detains: bool,
    /// The cap a read of the block environment or of the beneficiary's account sets.
    block_env_cap: u64,
    /// The cap a read of the Oracle's storage sets.
    oracle_cap: u64,
    /// Whether the running frame's volatile reads are refused.
    refusing: bool,
    /// The depth of the shallowest frame that switched volatile-data access off, if one did.
    disabled_from: Option<usize>,
    /// The depth the switch starts off from in every transaction: test tooling.
    #[cfg(any(test, feature = "test-utils"))]
    disabled_from_at_start: Option<usize>,
    /// What the transaction read.
    accessed: VolatileDataAccess,
    /// The most compute the transaction may reach, once it read volatile data.
    limit: Option<u64>,
    /// The compute of the frames suspended on a child that runs.
    suspended: u64,
    /// The regular gas the frames that halted burned without running anything with it.
    burned: u64,
    /// What the running frame had left when a charge failed on an out-of-gas whose halt zeroes it.
    left_at_halt: Option<u64>,
    /// One entry per frame that runs or is suspended, the running one last.
    frames: Vec<DetainedFrame>,
}

impl Detention {
    /// Clears the state for a new transaction, keeping its allocation.
    ///
    /// `detains` is whether the transaction's reads may be held to a cap at all;
    /// `block_env_cap` and `oracle_cap` are the caps its reads set, `u64::MAX` for none. A
    /// transaction whose caps are both unlimited is not detained: nothing it reads can cap it.
    pub(crate) fn reset(&mut self, detains: bool, block_env_cap: u64, oracle_cap: u64) {
        self.observed.set(VolatileDataAccess::empty());
        self.refused.set(VolatileDataAccess::empty());
        self.detains = detains && (block_env_cap != u64::MAX || oracle_cap != u64::MAX);
        self.block_env_cap = block_env_cap;
        self.oracle_cap = oracle_cap;
        self.refusing = false;
        #[cfg(any(test, feature = "test-utils"))]
        {
            self.disabled_from = self.disabled_from_at_start;
        }
        #[cfg(not(any(test, feature = "test-utils")))]
        {
            self.disabled_from = None;
        }
        self.accessed = VolatileDataAccess::empty();
        self.limit = None;
        self.suspended = 0;
        self.burned = 0;
        self.left_at_halt = None;
        self.frames.clear();
    }

    /// Starts every transaction with volatile-data access switched off from `depth` down, as if
    /// the frame at `depth` switched it off before its first instruction: test tooling for the
    /// switch `MegaAccessControl` steers.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) const fn set_disabled_from_at_start(&mut self, depth: Option<usize>) {
        self.disabled_from_at_start = depth;
        self.disabled_from = depth;
    }

    /// What the transaction read so far.
    pub const fn accessed(&self) -> VolatileDataAccess {
        self.accessed
    }

    /// The most compute the transaction may reach, once it read volatile data: its compute at the
    /// read plus the read's cap, the lowest of these when it read more than once. `None` while it
    /// read nothing a cap holds.
    ///
    /// Compute here is the transaction's regular gas spent in its frames, without its intrinsic
    /// gas.
    pub const fn compute_limit(&self) -> Option<u64> {
        self.limit
    }

    /// Whether the transaction's reads are held to a cap.
    pub const fn detains(&self) -> bool {
        self.detains
    }

    /* The Host */

    /// Whether the Host refuses a load of `access`: the running frame is in a subtree where
    /// volatile-data access is switched off. A refusal is recorded for the running opcode's
    /// wrapper, which reverts the frame.
    #[inline]
    pub(crate) fn refuses(&self, access: VolatileDataAccess) -> bool {
        if !self.refusing {
            return false;
        }
        self.refused.set(access);
        true
    }

    /// Records that the Host loaded `access` for the running opcode. Its wrapper commits the read
    /// once the opcode completed.
    #[inline]
    pub(crate) fn observe(&self, access: VolatileDataAccess) {
        if self.detains {
            self.observed.set(self.observed.get() | access);
        }
    }

    /* The opcode wrappers */

    /// Whether the running frame's volatile reads are refused, so its opcodes must keep the gas
    /// they started with to hand it back to a refusal.
    #[inline]
    pub(crate) const fn is_refusing(&self) -> bool {
        self.refusing
    }

    /// Forgets what the Host observed or refused outside an opcode's wrapper, so it is not
    /// committed for the wrong opcode.
    #[inline]
    pub(crate) fn discard_stale_reads(&self) {
        self.observed.set(VolatileDataAccess::empty());
        self.refused.set(VolatileDataAccess::empty());
    }

    /// Whether the Host observed or refused anything for the running opcode.
    #[inline]
    pub(crate) fn has_reads(&self) -> bool {
        !(self.observed.get() | self.refused.get()).is_empty()
    }

    /// Names a refusal the running opcode's Host calls made `access`, whichever load was refused:
    /// the kind the opcode reads, for an opcode that loads another kind first.
    #[inline]
    pub(crate) fn name_refusal(&self, access: VolatileDataAccess) {
        if !self.refused.get().is_empty() {
            self.refused.set(access);
        }
    }

    /// Takes what the Host refused for the running opcode, if it refused anything.
    pub(crate) fn take_refused(&self) -> Option<VolatileDataAccess> {
        let refused = self.refused.replace(VolatileDataAccess::empty());
        (!refused.is_empty()).then_some(refused)
    }

    /// Takes what the Host observed for the running opcode.
    pub(crate) fn take_observed(&self) -> VolatileDataAccess {
        self.observed.replace(VolatileDataAccess::empty())
    }

    /// Commits the reads `observed` of the opcode the running frame completed, whose gas is `gas`:
    /// the transaction's limit becomes its compute now plus the reads' cap, unless it is already
    /// lower, and the frame's spendable gas is held to what the limit leaves it.
    ///
    /// `forwarded` is the regular gas the opcode forwarded to a frame it is about to start, which
    /// the frame's regular gas spent includes and its compute does not.
    pub(crate) fn commit_reads(
        &mut self,
        observed: VolatileDataAccess,
        gas: &mut Gas,
        forwarded: u64,
    ) {
        self.accessed |= observed;
        let cap = self.cap_of(observed);
        if cap == u64::MAX {
            return;
        }
        let compute = self.compute(gas).saturating_sub(forwarded);
        let limit = self.lower_limit(compute.saturating_add(cap));
        gas.limit_spendable(limit.saturating_sub(compute));
    }

    /// Records a read the transaction makes by being what it is, before any frame: a sender or a
    /// recipient that is the block beneficiary, an EIP-7702 authority that is, or a recipient that
    /// delegates to it. No compute has been spent, so the limit is the read's cap.
    pub(crate) fn mark_before_execution(&mut self, access: VolatileDataAccess) {
        if !self.detains {
            return;
        }
        self.accessed |= access;
        let cap = self.cap_of(access);
        if cap != u64::MAX {
            self.lower_limit(cap);
        }
    }

    /// Holds the running frame's spendable gas to what the limit leaves the transaction, once a
    /// read set one.
    ///
    /// Called where the frame's spendable gas can have grown past it: when the frame starts, when
    /// it resumes on the gas a child handed back, and after a storage write, whose restore of a
    /// slot to its original value refills the state and history gas that spilled onto regular gas.
    #[inline]
    pub(crate) fn hold(&self, gas: &mut Gas) {
        if let Some(limit) = self.limit {
            gas.limit_spendable(limit.saturating_sub(self.compute(gas)));
        }
    }

    /// Notes what the running frame has left, `left`, when an opcode's charge failed on an
    /// out-of-gas the halt after it zeroes: what the halt burns rather than what the frame ran.
    #[inline]
    pub(crate) const fn note_halt(&mut self, left: u64) {
        self.left_at_halt = Some(left);
    }

    /* The frame lifecycle */

    /// The frame at `depth`, whose gas is `gas`, is about to run: for the first time, or again
    /// after a child returned into it.
    ///
    /// A frame's first run adds its caller's compute to the transaction's, now that the frame's
    /// gas limit is known — what the caller forwarded, and a value call's stipend; a resumed
    /// frame takes it back out, because its own regular gas spent now accounts for the child. Then
    /// the frame is held to the limit, and the switch is read for it.
    #[inline]
    pub(crate) fn on_frame_run(&mut self, gas: &mut Gas, depth: usize) {
        self.refusing = self.disabled_from.is_some_and(|from| depth >= from);
        if !self.detains {
            return;
        }
        if self.frames.len() == depth {
            let contribution = self.caller_contribution(depth, gas.limit());
            if let Some(caller) = depth.checked_sub(1).and_then(|i| self.frames.get_mut(i)) {
                caller.contribution = contribution;
            }
            self.suspended = self.suspended.saturating_add(contribution);
            self.frames.push(DetainedFrame::default());
        } else if let Some(frame) = self.frames.get_mut(depth) {
            self.suspended = self.suspended.saturating_sub(frame.contribution);
            frame.contribution = 0;
        }
        debug_assert_eq!(self.frames.len(), depth + 1, "one entry per frame that runs");
        self.hold(gas);
    }

    /// The frame at `depth`, whose gas is `gas`, suspended to start a child. Its compute stands
    /// still until the child returns.
    #[inline]
    pub(crate) fn on_frame_suspend(&mut self, gas: &Gas, depth: usize) {
        if !self.detains {
            return;
        }
        if let Some(frame) = self.frames.get_mut(depth) {
            frame.at_suspension = regular_spent(gas);
        }
    }

    /// Records a read of `access` the running frame, whose gas is `gas`, makes itself rather than
    /// through an opcode — a keyless deployment's call reading its signer's account — and holds
    /// the frame to the limit it sets, as an opcode's read is committed
    /// ([`commit_reads`](Self::commit_reads)). A transaction detention does not hold records
    /// nothing, as the Host observes nothing for it.
    pub(crate) fn read_by_frame(&mut self, access: VolatileDataAccess, gas: &mut Gas) {
        if self.detains {
            self.commit_reads(access, gas, 0);
        }
    }

    /// The frame at `depth` returns `result` with `gas`, after it ran. The switch turns back on if
    /// the frame, or a frame below it, turned it off. A frame that halts burns what it had left.
    ///
    /// Returns the limit when the frame ran out of gas on a charge the withheld part of its gas
    /// would have paid ([`stop`](Self::stop)): the frame crossed the limit, and must stop the
    /// transaction rather than halt.
    #[inline]
    pub(crate) fn on_frame_end(
        &mut self,
        result: InstructionResult,
        gas: &mut Gas,
        depth: usize,
    ) -> Option<u64> {
        let left_at_halt = self.left_at_halt.take();
        if self.disabled_from.is_some_and(|from| from >= depth) {
            self.disabled_from = None;
        }
        if !self.detains {
            return None;
        }
        debug_assert_eq!(self.frames.len(), depth + 1, "the frame that returns is the last");
        self.frames.pop();
        if let Some(limit) = self.stop(gas) {
            return Some(limit);
        }
        if result.is_halt() {
            // The settlement rolls the spill back into regular gas and burns it all.
            let left = left_at_halt.unwrap_or_else(|| gas.remaining());
            self.burned = self.burned.saturating_add(left).saturating_add(gas.state_gas_spilled());
        }
        None
    }

    /// The regular gas a frame at `depth` that its caller forwarded `gas_limit` may spend before
    /// the transaction's compute reaches the limit: the spendable part the frame would start
    /// with. `None` while no read set a limit.
    pub(crate) fn allowance(&self, depth: usize, gas_limit: u64) -> Option<u64> {
        let limit = self.limit?;
        Some(limit.saturating_sub(self.compute_at_start(depth, gas_limit)))
    }

    /// The frame at `depth`, whose gas limit was `gas_limit`, was answered without running —
    /// `result` is the answer: a precompile's, an interceptor's, a `keylessDeploy` call's that
    /// carries value, an inspector's, or revm's for a call it did not start.
    ///
    /// An answer marked as a crossing — a precompile that ran out of the allowance it was held to
    /// ([`restore_forward`](Self::restore_forward)) — crossed the limit. Otherwise, an answer that
    /// halts burns the whole gas limit: nothing ran with it. An answer that spent more regular gas
    /// than the limit leaves the frame — the allowance it would have run on — is a charge the
    /// frame could not have made; state and history gas that spilled onto its regular gas are not
    /// counted, as they are not a running frame's compute. Such an answer is answered out of gas
    /// and marked as a crossing of what the frame would have had withheld. A crossing is
    /// settled by the same rule as a frame that ran ([`stop`](Self::stop)). Returns the limit
    /// when it crossed.
    pub(crate) fn on_answer(
        &mut self,
        result: &mut InterpreterResult,
        depth: usize,
        gas_limit: u64,
    ) -> Option<u64> {
        if !self.detains {
            return None;
        }
        if result.gas.withheld_crossing().is_none() {
            if result.result.is_halt() {
                self.burned = self.burned.saturating_add(gas_limit);
                return None;
            }
            let allowance = self.allowance(depth, gas_limit)?;
            // The regular gas the answer spent: state and history gas that spilled onto it are not
            // compute, as they are not a running frame's.
            let spent = gas_limit
                .saturating_sub(result.gas.remaining())
                .saturating_sub(result.gas.state_gas_spilled());
            if spent <= allowance {
                return None;
            }
            // The answer spent more than the allowance, and no more than the gas limit, so the
            // gas limit is above the allowance and the frame would have had the rest withheld. The
            // one answer of the engine's own that spends regular gas is that of a `keylessDeploy`
            // call carrying value; the limit is set to the forward all the same, so
            // an inspector's answer built on another limit is settled on what the frame was
            // forwarded.
            //
            // State and history gas that spilled onto regular gas would have drawn the withheld
            // part first, and the revert that settles the stop credits the spill back, so the
            // crossing withholds what is left once the spill is taken out, as the tracker's own
            // record of a running frame does. The spill is below `gas_limit - allowance`, since
            // the regular gas the answer spent past it exceeds the allowance.
            let withheld = NonZeroU64::new(
                (gas_limit - allowance).saturating_sub(result.gas.state_gas_spilled()),
            )?;
            result.result = InstructionResult::OutOfGas;
            result.gas.tracker_mut().set_limit(gas_limit);
            result.gas.set_withheld_crossing(Some(WithheldCrossing::new(withheld)));
        }
        self.stop(&mut result.gas)
    }

    /// Gives a precompile's answer, which ran on `withheld` less than its caller forwarded — the
    /// allowance, not the forward — the rest back, as the frame would have had it withheld: the
    /// answer's gas limit is the forward again, and an answer that did not halt keeps the
    /// withheld part unspent. A precompile that ran out of gas on the allowance ran out of what
    /// the limit left the transaction: the answer is marked as a crossing of the withheld part,
    /// and [`on_answer`](Self::on_answer) settles it as the stop.
    ///
    /// The precompile's price is not known without running it, so a precompile priced above its
    /// whole forward runs out of the allowance as well, and is the stop too. So is one priced
    /// within its forward, past the allowance, whose input fails a check made after its gas check,
    /// which without the read fails the call instead.
    pub(crate) fn restore_forward(result: &mut InterpreterResult, withheld: NonZeroU64) {
        let forward = result.gas.limit().saturating_add(withheld.get());
        result.gas.tracker_mut().set_limit(forward);
        if result.result == InstructionResult::PrecompileOOG {
            result.gas.set_withheld_crossing(Some(WithheldCrossing::new(withheld)));
        } else if !result.result.is_halt() {
            result.gas.erase_cost(withheld.get());
        }
    }

    /// Settles a frame whose regular gas ran out on a charge the withheld part would have paid —
    /// the fork records it as a [`WithheldCrossing`] — and returns the limit it crossed.
    ///
    /// The frame's gas becomes the withheld part at the crossing: the spendable part the frame had
    /// counts as spent, which brings the transaction's compute to the limit, and the withheld part
    /// goes back to its caller. Whether the halt already zeroed the frame's gas (`OutOfGas`) or not
    /// (`MemoryOOG`), the settlement is the same. A crossing only ever ends a frame on an
    /// out-of-gas, and only a frame detention held carries one.
    fn stop(&self, gas: &mut Gas) -> Option<u64> {
        let crossing = gas.withheld_crossing()?;
        gas.set_remaining(crossing.withheld());
        gas.clear_withheld_crossing();
        self.limit
    }

    /* The switch */

    /// Switches volatile-data access off for the frame at `depth` and every frame below it. A
    /// switch already off from a shallower frame stays as it is.
    ///
    /// The frame that steers the switch reads it when it resumes, after the call that steered it.
    pub fn disable_access(&mut self, depth: usize) {
        self.disabled_from = Some(self.disabled_from.map_or(depth, |from| from.min(depth)));
    }

    /// Switches volatile-data access back on for the frame at `depth`, and reports whether it
    /// could: a frame cannot switch back on what a frame above it switched off.
    pub fn enable_access(&mut self, depth: usize) -> bool {
        match self.disabled_from {
            Some(from) if depth > from => false,
            _ => {
                self.disabled_from = None;
                true
            }
        }
    }

    /// Whether volatile-data access is switched off for a frame at `depth`.
    pub fn is_access_disabled(&self, depth: usize) -> bool {
        self.disabled_from.is_some_and(|from| depth >= from)
    }

    /* Helpers */

    /// The transaction's compute while the running frame has `gas`.
    fn compute(&self, gas: &Gas) -> u64 {
        self.suspended.saturating_add(regular_spent(gas)).saturating_sub(self.burned)
    }

    /// The transaction's compute when a frame at `depth` with `gas_limit` starts, before it runs
    /// anything: every suspended caller's, the frame's own caller's included.
    fn compute_at_start(&self, depth: usize, gas_limit: u64) -> u64 {
        self.suspended
            .saturating_add(self.caller_contribution(depth, gas_limit))
            .saturating_sub(self.burned)
    }

    /// What the caller of a frame at `depth` with `gas_limit` adds to the transaction's compute
    /// while the frame runs: its regular gas spent at suspension, less the frame's gas limit. The
    /// transaction's own frame has no caller.
    fn caller_contribution(&self, depth: usize, gas_limit: u64) -> u64 {
        depth
            .checked_sub(1)
            .and_then(|i| self.frames.get(i))
            .map_or(0, |caller| caller.at_suspension.saturating_sub(gas_limit))
    }

    /// Lowers the limit to `limit`, unless it is already lower, and returns the limit.
    fn lower_limit(&mut self, limit: u64) -> u64 {
        let limit = self.limit.map_or(limit, |current| current.min(limit));
        self.limit = Some(limit);
        limit
    }

    /// The cap a read of `access` sets: the lowest of the caps of the kinds it holds, `u64::MAX`
    /// when none of them is capped.
    fn cap_of(&self, access: VolatileDataAccess) -> u64 {
        let mut cap = u64::MAX;
        if access.has_block_env_access() || access.has_beneficiary_balance_access() {
            cap = cap.min(self.block_env_cap);
        }
        if access.has_oracle_access() {
            cap = cap.min(self.oracle_cap);
        }
        cap
    }
}

/// The revert data of a read of `access` refused while volatile-data access is switched off:
/// `VolatileDataAccessDisabled(accessType)`, the access type being the kind's position in the
/// set ([`VolatileDataAccess::as_u8`]).
pub fn volatile_data_access_disabled_revert_data(access: VolatileDataAccess) -> Bytes {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    data.extend_from_slice(&U256::from(access.as_u8()).to_be_bytes::<32>());
    data.into()
}

/// The kind of volatile data the revert data of a refused read names:
/// `VolatileDataAccessDisabled(accessType)`, decoded with its argument as the `uint8` it is
/// encoded as. `None` when `revert_data` is not that error, or names no kind.
///
/// Decoding the argument as the contract's `VolatileDataAccessType` cannot name a `SLOTNUM`
/// refusal, whose access type, [`SLOT_NUM_ACCESS_TYPE`](crate::system::SLOT_NUM_ACCESS_TYPE), the
/// enum does not declare: the `IMegaAccessControl` binding decodes it to its `__Invalid`
/// placeholder.
pub fn decode_volatile_data_access_disabled(revert_data: &[u8]) -> Option<VolatileDataAccess> {
    let (selector, word) = revert_data.split_first_chunk::<4>()?;
    if *selector != VOLATILE_DATA_ACCESS_DISABLED_SELECTOR || word.len() != 32 {
        return None;
    }
    // A `uint8` word: 31 zero bytes, then the value.
    let (high, low) = word.split_at(31);
    if high.iter().any(|byte| *byte != 0) {
        return None;
    }
    VolatileDataAccess::from_access_type(low[0])
}

/// The regular gas a frame spent: its limit, less what it has left — the withheld part included —
/// less the state and history gas that spilled onto its regular gas.
#[inline]
pub(crate) const fn regular_spent(gas: &Gas) -> u64 {
    gas.limit().saturating_sub(gas.remaining()).saturating_sub(gas.state_gas_spilled())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS};

    /// A frame's gas: `limit` regular gas and `reservoir`.
    fn gas(limit: u64, reservoir: u64) -> Gas {
        Gas::new_with_regular_gas_and_reservoir(limit, reservoir)
    }

    fn detaining() -> Detention {
        let mut detention = Detention::default();
        detention.reset(true, BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS);
        detention
    }

    /// The cap of a read is the lowest of the caps of the kinds it holds.
    #[test]
    fn test_the_cap_of_a_read_is_its_kinds_lowest() {
        let mut detention = Detention::default();
        detention.reset(true, 7, 5);
        assert_eq!(detention.cap_of(VolatileDataAccess::TIMESTAMP), 7);
        assert_eq!(detention.cap_of(VolatileDataAccess::SLOT_NUM), 7);
        assert_eq!(detention.cap_of(VolatileDataAccess::BENEFICIARY_BALANCE), 7);
        assert_eq!(detention.cap_of(VolatileDataAccess::ORACLE), 5);
        assert_eq!(detention.cap_of(VolatileDataAccess::ORACLE | VolatileDataAccess::TIMESTAMP), 5);
        assert_eq!(detention.cap_of(VolatileDataAccess::empty()), u64::MAX);
    }

    /// A read holds the frame's spendable gas at its compute then plus the cap; the rest is
    /// withheld, and the frame's gas as every other reader sees it does not move.
    #[test]
    fn test_a_read_withholds_what_the_cap_leaves_over() {
        let mut detention = detaining();
        let mut frame = gas(100_000_000, 7);
        detention.on_frame_run(&mut frame, 0);
        assert!(frame.record_regular_cost(1_000));
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0);

        assert_eq!(detention.compute_limit(), Some(1_000 + BLOCK_ENV_ACCESS_COMPUTE_GAS));
        assert_eq!(frame.spendable(), BLOCK_ENV_ACCESS_COMPUTE_GAS);
        assert_eq!(frame.remaining(), 100_000_000 - 1_000);
        assert_eq!(frame.reservoir(), 7);
        assert_eq!(detention.accessed(), VolatileDataAccess::TIMESTAMP);
        assert_eq!(detention.on_frame_end(InstructionResult::Stop, &mut frame, 0), None);
    }

    /// A limit only goes down: a later read with room for more leaves it where it is.
    #[test]
    fn test_the_first_limit_binds_when_it_is_lower() {
        let mut detention = detaining();
        let mut frame = gas(100_000_000, 0);
        detention.on_frame_run(&mut frame, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0);
        assert!(frame.record_regular_cost(5_000));
        detention.commit_reads(VolatileDataAccess::ORACLE, &mut frame, 0);
        assert_eq!(detention.compute_limit(), Some(BLOCK_ENV_ACCESS_COMPUTE_GAS));
        assert_eq!(frame.spendable(), BLOCK_ENV_ACCESS_COMPUTE_GAS - 5_000);
    }

    /// State and history gas draw the withheld part first and are not compute: the frame's
    /// allowance is what it was.
    #[test]
    fn test_state_gas_drawn_from_the_withheld_part_is_not_compute() {
        let mut detention = detaining();
        let mut frame = gas(100_000_000, 0);
        detention.on_frame_run(&mut frame, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0);
        assert!(frame.record_state_cost(1_000_000));
        detention.hold(&mut frame);
        assert_eq!(detention.compute(&frame), 0);
        assert_eq!(frame.spendable(), BLOCK_ENV_ACCESS_COMPUTE_GAS);
    }

    /// A regular charge the withheld part would have paid is the cap: the frame's gas becomes the
    /// withheld part, and the spendable part it had counts as spent.
    #[test]
    fn test_a_crossing_stops_with_the_withheld_part_left() {
        for (result, zeroed) in
            [(InstructionResult::OutOfGas, true), (InstructionResult::MemoryOOG, false)]
        {
            let mut detention = detaining();
            let mut frame = gas(100_000_000, 0);
            detention.on_frame_run(&mut frame, 0);
            detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0);
            assert!(frame.record_regular_cost(10));
            assert!(!frame.record_regular_cost(BLOCK_ENV_ACCESS_COMPUTE_GAS));
            if zeroed {
                frame.spend_all();
            }
            let stop = detention.on_frame_end(result, &mut frame, 0);
            assert_eq!(stop, Some(BLOCK_ENV_ACCESS_COMPUTE_GAS), "{result:?}");
            assert_eq!(frame.remaining(), 100_000_000 - BLOCK_ENV_ACCESS_COMPUTE_GAS);
            assert_eq!(frame.withheld_crossing(), None);
            assert_eq!(regular_spent(&frame), BLOCK_ENV_ACCESS_COMPUTE_GAS, "compute at the limit");
        }
    }

    /// An out-of-gas nothing withheld could have paid halts and burns what the frame had.
    #[test]
    fn test_an_out_of_gas_beyond_the_whole_gas_halts() {
        let mut detention = detaining();
        let mut frame = gas(100_000_000, 0);
        detention.on_frame_run(&mut frame, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0);
        assert!(!frame.record_regular_cost(100_000_001));
        detention.note_halt(frame.remaining());
        frame.spend_all();
        assert_eq!(detention.on_frame_end(InstructionResult::OutOfGas, &mut frame, 0), None);
        assert_eq!(detention.burned, 100_000_000);

        // A frame nothing was withheld from runs out of its own gas.
        let mut detention = detaining();
        let mut frame = gas(1_000_000, 0);
        detention.on_frame_run(&mut frame, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0);
        assert_eq!(frame.withheld(), 0, "nothing to withhold");
        assert!(!frame.record_regular_cost(1_000_001));
        frame.spend_all();
        assert_eq!(detention.on_frame_end(InstructionResult::OutOfGas, &mut frame, 0), None);
    }

    /// A child's compute adds to its caller's; the caller's forwarded gas and the child's stipend
    /// do not, and after the child returns the caller's own gas accounts for it the same way.
    #[test]
    fn test_compute_spans_frames() {
        let mut detention = detaining();
        let mut caller = gas(100_000, 0);
        detention.on_frame_run(&mut caller, 0);
        assert!(caller.record_regular_cost(9_000 + 60_000));
        detention.on_frame_suspend(&caller, 0);
        assert_eq!(detention.compute_at_start(1, 60_000 + 2_300), 9_000 - 2_300);

        let mut child = gas(60_000 + 2_300, 0);
        detention.on_frame_run(&mut child, 1);
        assert_eq!(detention.compute(&child), 9_000 - 2_300);
        assert!(child.record_regular_cost(4_000));
        assert_eq!(detention.compute(&child), 9_000 + 4_000 - 2_300);
        detention.on_frame_end(InstructionResult::Stop, &mut child, 1);

        caller.erase_cost(child.remaining());
        detention.on_frame_run(&mut caller, 0);
        assert_eq!(detention.compute(&caller), 9_000 + 4_000 - 2_300);
        assert_eq!(detention.suspended, 0);
    }

    /// What a halting child burns is not compute: its caller's compute after it returns is what
    /// the child ran, not the gas the child was given — whether the halt left the gas in place or
    /// zeroed it after the child's wrapper noted it.
    #[test]
    fn test_what_a_halt_burns_is_not_compute() {
        for zeroed in [false, true] {
            let mut detention = detaining();
            let mut caller = gas(1_000_000, 0);
            detention.on_frame_run(&mut caller, 0);
            assert!(caller.record_regular_cost(500_000));
            detention.on_frame_suspend(&caller, 0);
            let mut child = gas(500_000, 0);
            detention.on_frame_run(&mut child, 1);
            assert!(child.record_regular_cost(1_000));
            assert!(child.record_state_cost(3_000));
            let result = if zeroed {
                detention.note_halt(child.remaining());
                child.spend_all();
                InstructionResult::OutOfGas
            } else {
                InstructionResult::InvalidFEOpcode
            };
            assert_eq!(detention.on_frame_end(result, &mut child, 1), None);

            // The caller's regular gas spent now holds everything it forwarded.
            detention.on_frame_run(&mut caller, 0);
            assert_eq!(detention.compute(&caller), 1_000, "zeroed: {zeroed}");
            assert_eq!(detention.burned, 500_000 - 1_000);
        }
    }

    /// An answer that halts burns the gas limit; one that spent more regular gas than the frame's
    /// allowance is answered out of gas and stops at the limit, with what the frame would have had
    /// withheld. State gas that spilled onto the answer's regular gas is not compute.
    #[test]
    fn test_an_answer_is_held_to_the_allowance_it_would_have_run_on() {
        let mut detention = detaining();
        let mut caller = gas(100_000_000, 0);
        detention.on_frame_run(&mut caller, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut caller, 0);
        assert!(caller.record_withheld_first_cost(90_000_000));
        detention.on_frame_suspend(&caller, 0);

        let answer = |spent: u64, result| {
            let mut gas = gas(90_000_000, 0);
            assert!(gas.record_regular_cost(spent));
            InterpreterResult::new(result, Bytes::new(), gas)
        };
        let mut within = answer(BLOCK_ENV_ACCESS_COMPUTE_GAS, InstructionResult::Return);
        assert_eq!(detention.on_answer(&mut within, 1, 90_000_000), None);
        assert_eq!(within.result, InstructionResult::Return);

        let mut spilled = answer(BLOCK_ENV_ACCESS_COMPUTE_GAS, InstructionResult::Revert);
        assert!(spilled.gas.record_state_cost(1_000));
        assert_eq!(spilled.gas.state_gas_spilled(), 1_000);
        assert_eq!(detention.on_answer(&mut spilled, 1, 90_000_000), None, "a spill is no compute");
        assert_eq!(spilled.result, InstructionResult::Revert);

        let mut halted = answer(0, InstructionResult::PrecompileError);
        assert_eq!(detention.on_answer(&mut halted, 1, 90_000_000), None);
        assert_eq!(detention.burned, 90_000_000);
        detention.burned = 0;

        let mut beyond = answer(BLOCK_ENV_ACCESS_COMPUTE_GAS + 1, InstructionResult::Return);
        assert_eq!(
            detention.on_answer(&mut beyond, 1, 90_000_000),
            Some(BLOCK_ENV_ACCESS_COMPUTE_GAS)
        );
        assert_eq!(beyond.result, InstructionResult::OutOfGas);
        assert_eq!(beyond.gas.remaining(), 90_000_000 - BLOCK_ENV_ACCESS_COMPUTE_GAS);
    }

    /// A precompile run on the allowance gets the rest of the forward back: a success keeps it
    /// unspent, a halt burns the forward as it would without the read, and an out-of-gas on the
    /// allowance is a crossing, which the answer's settlement stops with the rest left.
    #[test]
    fn test_a_precompile_run_on_the_allowance_is_settled_on_the_forward() {
        let mut detention = detaining();
        let mut caller = gas(100_000_000, 0);
        detention.on_frame_run(&mut caller, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut caller, 0);
        assert!(caller.record_withheld_first_cost(90_000_000));
        detention.on_frame_suspend(&caller, 0);
        let allowance = detention.allowance(1, 90_000_000).unwrap();
        assert_eq!(allowance, BLOCK_ENV_ACCESS_COMPUTE_GAS);
        let withheld = NonZeroU64::new(90_000_000 - allowance).unwrap();
        let answer = |result: InstructionResult, spent: u64| {
            let mut gas = gas(allowance, 0);
            assert!(gas.record_regular_cost(spent));
            if result.is_halt() {
                gas.spend_all();
            }
            let mut answer = InterpreterResult::new(result, Bytes::new(), gas);
            Detention::restore_forward(&mut answer, withheld);
            assert_eq!(answer.gas.limit(), 90_000_000, "{result:?}");
            answer
        };

        let mut answered = answer(InstructionResult::Return, 1_000);
        assert_eq!(answered.gas.remaining(), 90_000_000 - 1_000);
        assert_eq!(answered.gas.withheld_crossing(), None);
        assert_eq!(detention.on_answer(&mut answered, 1, 90_000_000), None);

        let mut failed = answer(InstructionResult::PrecompileError, 0);
        assert_eq!(failed.gas.remaining(), 0);
        assert_eq!(detention.on_answer(&mut failed, 1, 90_000_000), None);
        assert_eq!(detention.burned, 90_000_000, "the forward burns");
        detention.burned = 0;

        let mut out_of_gas = answer(InstructionResult::PrecompileOOG, 0);
        assert_eq!(out_of_gas.gas.withheld_crossing(), Some(WithheldCrossing::new(withheld)));
        assert_eq!(
            detention.on_answer(&mut out_of_gas, 1, 90_000_000),
            Some(BLOCK_ENV_ACCESS_COMPUTE_GAS)
        );
        assert_eq!(out_of_gas.gas.remaining(), withheld.get(), "the allowance counts as spent");
        assert_eq!(out_of_gas.gas.withheld_crossing(), None);
        assert_eq!(detention.burned, 0, "a crossing burns nothing");
    }

    /// A keyless deployment's call is the transaction's own frame: held to the limit before it
    /// charges anything, its charges are compute once its creation runs, and the gas it forwards
    /// is not.
    #[test]
    fn test_a_keyless_call_is_the_transactions_own_frame() {
        let mut detention = detaining();
        detention.mark_before_execution(VolatileDataAccess::BENEFICIARY_BALANCE);
        let mut call = gas(100_000_000, 0);
        detention.on_frame_run(&mut call, 0);
        assert_eq!(call.spendable(), BLOCK_ENV_ACCESS_COMPUTE_GAS);

        assert!(call.record_regular_cost(132_000));
        assert!(call.record_withheld_first_cost(50_000_000));
        detention.on_frame_suspend(&call, 0);
        let mut creation = gas(50_000_000, 0);
        detention.on_frame_run(&mut creation, 1);
        assert_eq!(detention.compute(&creation), 132_000, "the charges, not the forward");
        assert_eq!(creation.spendable(), BLOCK_ENV_ACCESS_COMPUTE_GAS - 132_000);
        assert_eq!(detention.allowance(2, 0), Some(BLOCK_ENV_ACCESS_COMPUTE_GAS - 132_000));
    }

    /// A frame's own read is committed as an opcode's is: the limit is its compute at the read
    /// plus the cap. A transaction detention does not hold records nothing.
    #[test]
    fn test_a_frames_own_read_sets_the_limit_from_its_compute() {
        let mut detention = detaining();
        let mut call = gas(100_000_000, 0);
        assert!(call.record_regular_cost(100_000));
        detention.read_by_frame(VolatileDataAccess::BENEFICIARY_BALANCE, &mut call);
        assert_eq!(detention.compute_limit(), Some(100_000 + BLOCK_ENV_ACCESS_COMPUTE_GAS));
        assert_eq!(detention.accessed(), VolatileDataAccess::BENEFICIARY_BALANCE);
        assert_eq!(call.spendable(), BLOCK_ENV_ACCESS_COMPUTE_GAS);

        let mut free = Detention::default();
        free.reset(false, BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS);
        free.read_by_frame(VolatileDataAccess::BENEFICIARY_BALANCE, &mut call);
        assert_eq!((free.compute_limit(), free.accessed()), (None, VolatileDataAccess::empty()));
    }

    /// The switch holds for the frame that turned it off and every frame below it, turns back on
    /// when that frame returns, and cannot be turned back on from below.
    #[test]
    fn test_the_switch_is_scoped_to_a_subtree() {
        let mut detention = detaining();
        let mut frame = gas(1_000_000, 0);
        detention.on_frame_run(&mut frame, 0);
        detention.on_frame_suspend(&frame, 0);
        let mut child = gas(100_000, 0);
        detention.on_frame_run(&mut child, 1);
        detention.disable_access(2);
        assert!(!detention.is_access_disabled(1));
        detention.disable_access(1);
        assert!(!detention.is_refusing(), "the frame reads the switch when it resumes");
        detention.on_frame_run(&mut child, 1);
        assert!(detention.is_refusing());
        assert!(!detention.is_access_disabled(0));
        assert!(detention.is_access_disabled(1) && detention.is_access_disabled(2));
        detention.disable_access(2);
        assert!(detention.is_access_disabled(1), "a deeper frame keeps the shallower switch");
        assert!(!detention.enable_access(2), "a frame below cannot switch it back on");
        assert!(detention.refuses(VolatileDataAccess::BLOCK_NUMBER));
        detention.name_refusal(VolatileDataAccess::BLOCK_HASH);
        assert_eq!(detention.take_refused(), Some(VolatileDataAccess::BLOCK_HASH));
        detention.name_refusal(VolatileDataAccess::BLOCK_HASH);
        assert_eq!(detention.take_refused(), None, "nothing refused, nothing named");

        detention.on_frame_end(InstructionResult::Stop, &mut child, 1);
        assert!(!detention.is_access_disabled(1), "on again once the frame returned");
        detention.on_frame_run(&mut frame, 0);
        assert!(!detention.is_refusing());

        detention.disable_access(0);
        detention.on_frame_run(&mut frame, 0);
        assert!(detention.refuses(VolatileDataAccess::TIMESTAMP));
        assert!(detention.enable_access(0), "the frame that switched it off switches it on");
        detention.on_frame_run(&mut frame, 0);
        assert!(!detention.refuses(VolatileDataAccess::TIMESTAMP));
    }

    /// A refusal's revert data decodes back to the kind it names, for every kind the engine
    /// names, the slot number's access type 12 included, which the contract's own ABI type cannot
    /// name.
    #[test]
    fn test_a_refusal_decodes_to_the_kind_it_names() {
        use crate::system::{IMegaAccessControl, VolatileDataAccessType, SLOT_NUM_ACCESS_TYPE};
        use alloy_sol_types::SolError;

        for access_type in 0..=SLOT_NUM_ACCESS_TYPE {
            let access = VolatileDataAccess::from_access_type(access_type).unwrap();
            let data = volatile_data_access_disabled_revert_data(access);
            assert_eq!(data.len(), 36);
            assert_eq!(decode_volatile_data_access_disabled(&data), Some(access));
        }
        // The enum-typed decoder loses 12 to its placeholder for an undeclared variant, and its
        // validating form refuses it: the argument is a `uint8`, and is decoded as one.
        let slot_num = volatile_data_access_disabled_revert_data(VolatileDataAccess::SLOT_NUM);
        assert_eq!(
            IMegaAccessControl::VolatileDataAccessDisabled::abi_decode(&slot_num)
                .unwrap()
                .accessType,
            VolatileDataAccessType::__Invalid,
        );
        assert!(
            IMegaAccessControl::VolatileDataAccessDisabled::abi_decode_validate(&slot_num).is_err()
        );
        let oracle = volatile_data_access_disabled_revert_data(VolatileDataAccess::ORACLE);
        assert_eq!(
            IMegaAccessControl::VolatileDataAccessDisabled::abi_decode_validate(&oracle)
                .unwrap()
                .accessType,
            VolatileDataAccessType::Oracle,
        );
    }

    /// Anything else decodes to nothing: another selector, a short or long payload, a word that is
    /// not a `uint8`, and an access type past the slot number's.
    #[test]
    fn test_other_revert_data_decodes_to_nothing() {
        let valid = volatile_data_access_disabled_revert_data(VolatileDataAccess::TIMESTAMP);
        let mut other_selector = valid.to_vec();
        other_selector[0] ^= 1;
        let mut wide = valid.to_vec();
        wide[4] = 1;
        let mut wide_low = valid.to_vec();
        wide_low[34] = 1;
        let mut past = valid.to_vec();
        past[35] = 13;
        let mut long = valid.to_vec();
        long.push(0);
        for data in [
            other_selector,
            valid[..35].to_vec(),
            valid[..4].to_vec(),
            valid[..3].to_vec(),
            long,
            wide,
            wide_low,
            past,
            vec![],
        ] {
            assert_eq!(decode_volatile_data_access_disabled(&data), None, "{data:?}");
        }
    }

    /// A transaction that is not detained records nothing and caps nothing: a system-originated
    /// one, and one whose caps are both unlimited. One unlimited cap leaves its kind uncapped.
    #[test]
    fn test_an_exempt_or_unlimited_transaction_is_not_detained() {
        for (detains, block_env_cap, oracle_cap) in [
            (false, BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS),
            (true, u64::MAX, u64::MAX),
        ] {
            let mut detention = Detention::default();
            detention.reset(detains, block_env_cap, oracle_cap);
            assert!(!detention.detains());
            let mut frame = gas(100_000_000, 0);
            detention.on_frame_run(&mut frame, 0);
            detention.observe(VolatileDataAccess::TIMESTAMP);
            assert!(!detention.has_reads());
            detention.mark_before_execution(VolatileDataAccess::BENEFICIARY_BALANCE);
            assert_eq!(detention.compute_limit(), None);
            assert_eq!(detention.accessed(), VolatileDataAccess::empty());
        }

        let mut detention = Detention::default();
        detention.reset(true, u64::MAX, 5);
        assert!(detention.detains());
        let mut frame = gas(100_000_000, 0);
        detention.on_frame_run(&mut frame, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0);
        detention.mark_before_execution(VolatileDataAccess::BENEFICIARY_BALANCE);
        assert_eq!(detention.compute_limit(), None, "an unlimited cap sets no limit");
        assert_eq!(frame.withheld(), 0);
        detention.commit_reads(VolatileDataAccess::ORACLE, &mut frame, 0);
        assert_eq!(detention.compute_limit(), Some(5));
    }
}
