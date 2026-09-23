//! The state of gas detention for the running transaction.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::cell::Cell;
use std::vec::Vec;

use alloy_primitives::{Bytes, U256};
use revm::interpreter::{Gas, InstructionResult};

use super::VolatileDataAccess;
use crate::{
    constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS},
    system::VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
};

/// What detention keeps of one frame that runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DetainedFrame {
    /// Regular gas withheld from the frame, which its reservoir holds until the frame returns.
    withheld: u64,
    /// Regular gas the frame's limit carries that its caller did not pay: a value call's stipend.
    minted: u64,
    /// The frame's own compute when it suspended on a child: its regular gas spent, less what
    /// detention withheld.
    at_suspension: u64,
    /// While a child of the frame runs: what the frame adds to the transaction's compute, which is
    /// [`at_suspension`](Self::at_suspension) less the gas it forwarded to that child.
    contribution: u64,
}

/// Gas detention for the running transaction: which volatile data it read, the most compute it
/// may reach because of it, and what each frame had withheld to enforce that.
///
/// The `access` module describes the mechanism.
///
/// # Compute
///
/// Compute is the regular gas the transaction spends on what it runs: a frame's regular gas spent
/// is its limit less what it has left, less the state and history gas that spilled onto its
/// regular gas, which are not compute. The transaction's compute is its frames' compute: the
/// frame that runs, read off its [`Gas`], and every frame suspended on a child that runs, as it
/// stood when it suspended, less the gas it forwarded. A value call's stipend is gas nobody paid,
/// so a frame's stipend is taken off its compute; once the frame returns, its caller's regular gas
/// spent accounts for it the same way. So the figure follows the regular ledger of the
/// transaction's frames at every moment, before, during and after each child.
///
/// The one part of the regular ledger that is not compute is what a halt burns: a frame that
/// halts consumes the gas it had left without running anything with it. That gas is taken off the
/// transaction's compute when the frame returns. A frame that ran out of gas has nothing left by
/// then, so what it had when the charge failed counts as spent.
///
/// # Refused reads
///
/// A frame in a subtree where volatile-data access is switched off does not read volatile data:
/// the Host refuses the load and the opcode's wrapper reverts the frame with
/// `VolatileDataAccessDisabled`. The switch is scoped to the frame that turned it off and the
/// frames below it, and turns back on when that frame returns.
#[derive(Clone, Debug, Default)]
pub struct Detention {
    /// What the Host loaded for the running opcode, waiting for its wrapper.
    observed: Cell<VolatileDataAccess>,
    /// What the Host refused to load for the running opcode, waiting for its wrapper.
    refused: Cell<VolatileDataAccess>,
    /// Whether the transaction's reads are held to a cap. A system-originated transaction and the
    /// neutral configuration's are not.
    detains: bool,
    /// Whether the neutral configuration runs, which detains no transaction.
    neutral: bool,
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
    /// The stipends of the frames that run or are suspended.
    minted: u64,
    /// The regular gas the frames that halted burned without running anything with it.
    burned: u64,
    /// One entry per frame that runs or is suspended, the running one last.
    frames: Vec<DetainedFrame>,
}

impl Detention {
    /// Clears the state for a new transaction, keeping its allocation. `detains` is whether the
    /// transaction's reads are held to a cap.
    pub(crate) fn reset(&mut self, detains: bool) {
        self.observed.set(VolatileDataAccess::empty());
        self.refused.set(VolatileDataAccess::empty());
        self.detains = detains && !self.neutral;
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
        self.minted = 0;
        self.burned = 0;
        self.frames.clear();
    }

    /// Turns detention off for every transaction: the neutral configuration.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) const fn set_neutral(&mut self, neutral: bool) {
        self.neutral = neutral;
        self.detains &= !neutral;
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
    /// read nothing.
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

    /// Takes what the Host refused for the running opcode, if it refused anything.
    pub(crate) fn take_refused(&self) -> Option<VolatileDataAccess> {
        let refused = self.refused.replace(VolatileDataAccess::empty());
        (!refused.is_empty()).then_some(refused)
    }

    /// Takes what the Host observed for the running opcode.
    pub(crate) fn take_observed(&self) -> VolatileDataAccess {
        self.observed.replace(VolatileDataAccess::empty())
    }

    /// Commits the reads `observed` of the opcode the frame at `depth` completed, whose gas is
    /// `gas`: the transaction's limit becomes its compute now plus the reads' cap, unless it is
    /// already lower, and the frame keeps no more regular gas than the limit leaves it.
    ///
    /// `forwarded` is the regular gas the opcode forwarded to a frame it is about to start, which
    /// the frame's regular gas spent includes and its compute does not.
    pub(crate) fn commit_reads(
        &mut self,
        observed: VolatileDataAccess,
        gas: &mut Gas,
        depth: usize,
        forwarded: u64,
    ) {
        self.accessed |= observed;
        let compute = self.compute(gas, depth).saturating_sub(forwarded);
        let limit = compute.saturating_add(cap_of(observed));
        self.limit = Some(self.limit.map_or(limit, |current| current.min(limit)));
        self.cap_frame(gas, depth, compute);
    }

    /// Records a read the transaction makes by being what it is, before any frame: a sender or a
    /// recipient that is the block beneficiary, or an EIP-7702 authority that is. No compute has
    /// been spent, so the limit is the read's cap.
    pub(crate) fn mark_before_execution(&mut self, access: VolatileDataAccess) {
        if !self.detains {
            return;
        }
        self.accessed |= access;
        let limit = cap_of(access);
        self.limit = Some(self.limit.map_or(limit, |current| current.min(limit)));
    }

    /// Holds the running frame at `depth` to the limit again, after an opcode that can hand it
    /// regular gas back: a storage write restored to its original value refills the state and
    /// history gas it spilled onto regular gas, which may have spilled before the limit was set.
    #[inline]
    pub(crate) fn recap(&mut self, gas: &mut Gas, depth: usize) {
        if self.limit.is_some() {
            let compute = self.compute(gas, depth);
            self.cap_frame(gas, depth, compute);
        }
    }

    /* The frame lifecycle */

    /// The frame at `depth`, whose gas is `gas`, is about to run: for the first time, or again
    /// after a child returned into it. `minted` is the stipend its limit carries, which is
    /// read the first time only.
    ///
    /// A frame's first run adds its caller's compute to the transaction's, now that the gas the
    /// caller forwarded is known; a resumed frame takes it back out, because its own regular gas
    /// spent now accounts for the child. Then the frame is held to the limit, and the switch is
    /// read for it.
    #[inline]
    pub(crate) fn on_frame_run(&mut self, gas: &mut Gas, depth: usize, minted: u64) {
        self.refusing = self.disabled_from.is_some_and(|from| depth >= from);
        if !self.detains {
            return;
        }
        if self.frames.len() == depth {
            if let Some(caller) = depth.checked_sub(1).and_then(|i| self.frames.get_mut(i)) {
                let forwarded = gas.limit().saturating_sub(minted);
                caller.contribution = caller.at_suspension.saturating_sub(forwarded);
                self.suspended = self.suspended.saturating_add(caller.contribution);
            }
            self.frames.push(DetainedFrame { minted, ..Default::default() });
            self.minted = self.minted.saturating_add(minted);
        } else if let Some(frame) = self.frames.get_mut(depth) {
            self.suspended = self.suspended.saturating_sub(frame.contribution);
            frame.contribution = 0;
        }
        debug_assert_eq!(self.frames.len(), depth + 1, "one entry per frame that runs");
        self.recap(gas, depth);
    }

    /// The frame at `depth`, whose gas is `gas`, suspended to start a child. Its compute stands
    /// still until the child returns.
    #[inline]
    pub(crate) fn on_frame_suspend(&mut self, gas: &Gas, depth: usize) {
        if !self.detains {
            return;
        }
        if let Some(frame) = self.frames.get_mut(depth) {
            frame.at_suspension = regular_spent(gas).saturating_sub(frame.withheld);
        }
    }

    /// The frame at `depth` returns `result` with `gas`: the gas withheld from it goes back into
    /// its regular gas, and the switch turns back on if the frame, or a frame below it, turned it
    /// off. When the frame halts, what the halt burns is not compute.
    ///
    /// Returns the limit, and the compute the transaction had spent, when the frame ran out of
    /// gas while detention withheld some of it: the cap bound before the frame's own gas, and the
    /// frame must stop the transaction rather than halt.
    #[inline]
    pub(crate) fn on_frame_end(
        &mut self,
        result: InstructionResult,
        gas: &mut Gas,
        depth: usize,
    ) -> Option<(u64, u64)> {
        if self.disabled_from.is_some_and(|from| from >= depth) {
            self.disabled_from = None;
        }
        if !self.detains {
            return None;
        }
        debug_assert_eq!(self.frames.len(), depth + 1, "the frame that returns is the last");
        let withheld = self.frames.last().map_or(0, |frame| frame.withheld);
        let stop = (withheld > 0 && runs_out_of_gas(result))
            .then(|| (self.limit.unwrap_or(u64::MAX), self.compute(gas, depth)));
        if let Some(frame) = self.frames.pop() {
            self.minted = self.minted.saturating_sub(frame.minted);
        }
        release(gas, withheld);
        if stop.is_none() && result.is_halt() {
            // The settlement rolls the spill back into regular gas and burns it all.
            let burned = gas.remaining().saturating_add(gas.state_gas_spilled());
            self.burned = self.burned.saturating_add(burned);
        }
        stop
    }

    /* The switch */

    /// Switches volatile-data access off for the frame at `depth` and every frame below it. A
    /// switch already off from a shallower frame stays as it is.
    pub fn disable_access(&mut self, depth: usize) {
        if self.disabled_from.is_none_or(|from| depth < from) {
            self.disabled_from = Some(depth);
        }
        self.refusing = self.disabled_from.is_some_and(|from| depth >= from);
    }

    /// Switches volatile-data access back on for the frame at `depth`, and reports whether it
    /// could: a frame cannot switch back on what a frame above it switched off.
    pub fn enable_access(&mut self, depth: usize) -> bool {
        match self.disabled_from {
            Some(from) if depth > from => false,
            _ => {
                self.disabled_from = None;
                self.refusing = false;
                true
            }
        }
    }

    /// Whether volatile-data access is switched off for a frame at `depth`.
    pub fn is_access_disabled(&self, depth: usize) -> bool {
        self.disabled_from.is_some_and(|from| depth >= from)
    }

    /* Helpers */

    /// The transaction's compute while the frame at `depth` runs with `gas`.
    fn compute(&self, gas: &Gas, depth: usize) -> u64 {
        let own = self.frames.get(depth).map_or(0, |frame| frame.withheld);
        self.suspended
            .saturating_add(regular_spent(gas))
            .saturating_sub(own)
            .saturating_sub(self.minted)
            .saturating_sub(self.burned)
    }

    /// Withholds from the frame at `depth` the regular gas it has beyond what the limit leaves
    /// the transaction, `compute` being the transaction's compute now.
    ///
    /// The gas moves into the frame's reservoir. A regular charge cannot draw on the reservoir,
    /// so the frame cannot compute with it; a state or history charge draws on the reservoir
    /// first, so the frame can still pay for what it writes and appends, which is not compute.
    fn cap_frame(&mut self, gas: &mut Gas, depth: usize, compute: u64) {
        let Some(limit) = self.limit else { return };
        let allowance = limit.saturating_sub(compute);
        let excess = gas.remaining().saturating_sub(allowance);
        if excess == 0 {
            return;
        }
        let Some(frame) = self.frames.get_mut(depth) else { return };
        gas.set_remaining(gas.remaining() - excess);
        gas.set_reservoir(gas.reservoir().saturating_add(excess));
        frame.withheld = frame.withheld.saturating_add(excess);
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

/// The cap a read of `access` sets: the lowest of the caps of the kinds it holds.
fn cap_of(access: VolatileDataAccess) -> u64 {
    let mut cap = u64::MAX;
    if access.has_block_env_access() || access.has_beneficiary_balance_access() {
        cap = cap.min(BLOCK_ENV_ACCESS_COMPUTE_GAS);
    }
    if access.has_oracle_access() {
        cap = cap.min(ORACLE_ACCESS_COMPUTE_GAS);
    }
    cap
}

/// The regular gas a frame spent: its limit, less what it has left, less the state and history
/// gas that spilled onto its regular gas.
#[inline]
pub(crate) const fn regular_spent(gas: &Gas) -> u64 {
    gas.limit().saturating_sub(gas.remaining()).saturating_sub(gas.state_gas_spilled())
}

/// Whether `result` is running out of gas: a charge the frame could not pay. Running into the
/// memory limit is not, nor is a precompile's out-of-gas, which no frame that runs returns.
const fn runs_out_of_gas(result: InstructionResult) -> bool {
    matches!(
        result,
        InstructionResult::OutOfGas |
            InstructionResult::MemoryOOG |
            InstructionResult::InvalidOperandOOG |
            InstructionResult::ReentrancySentryOOG
    )
}

/// Hands `withheld` back to the regular gas of the frame whose final gas is `gas`, as if it had
/// never been withheld.
///
/// Withheld gas sits in the reservoir, where the frame's state and history charges may have drawn
/// on it. What is left of it goes back to regular gas; what was drawn is counted as having
/// spilled onto regular gas, which is where those charges would have come from had nothing been
/// withheld. The frame's result then settles exactly as it would have without detention: a
/// success or a revert gives the caller the unspent regular gas, a halt burns it, and the
/// reservoir returns to what the caller handed down.
#[inline]
pub(crate) fn release(gas: &mut Gas, withheld: u64) {
    if withheld == 0 {
        return;
    }
    let back = withheld.min(gas.reservoir());
    gas.set_reservoir(gas.reservoir() - back);
    gas.set_remaining(gas.remaining().saturating_add(back));
    gas.add_state_gas_spilled(withheld - back);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame's gas: `limit` regular gas and `reservoir`.
    fn gas(limit: u64, reservoir: u64) -> Gas {
        Gas::new_with_regular_gas_and_reservoir(limit, reservoir)
    }

    fn detaining() -> Detention {
        let mut detention = Detention::default();
        detention.reset(true);
        detention
    }

    /// The cap of a read is the lowest of the caps of the kinds it holds; both caps are the
    /// same today, so every kind sets the same one.
    #[test]
    fn test_the_cap_of_a_read_is_its_kinds_lowest() {
        assert_eq!(cap_of(VolatileDataAccess::TIMESTAMP), BLOCK_ENV_ACCESS_COMPUTE_GAS);
        assert_eq!(cap_of(VolatileDataAccess::SLOT_NUM), BLOCK_ENV_ACCESS_COMPUTE_GAS);
        assert_eq!(cap_of(VolatileDataAccess::BENEFICIARY_BALANCE), BLOCK_ENV_ACCESS_COMPUTE_GAS);
        assert_eq!(cap_of(VolatileDataAccess::ORACLE), ORACLE_ACCESS_COMPUTE_GAS);
        assert_eq!(
            cap_of(VolatileDataAccess::ORACLE | VolatileDataAccess::TIMESTAMP),
            BLOCK_ENV_ACCESS_COMPUTE_GAS.min(ORACLE_ACCESS_COMPUTE_GAS)
        );
        assert_eq!(cap_of(VolatileDataAccess::empty()), u64::MAX);
    }

    /// A read caps the frame at its compute then plus the cap: the regular gas beyond it moves
    /// into the reservoir, and the frame returns it on release.
    #[test]
    fn test_a_read_withholds_what_the_cap_leaves_over() {
        let mut detention = detaining();
        let mut frame = gas(100_000_000, 7);
        detention.on_frame_run(&mut frame, 0, 0);
        assert!(frame.record_regular_cost(1_000));
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0, 0);

        assert_eq!(detention.compute_limit(), Some(1_000 + BLOCK_ENV_ACCESS_COMPUTE_GAS));
        assert_eq!(frame.remaining(), BLOCK_ENV_ACCESS_COMPUTE_GAS);
        assert_eq!(frame.reservoir(), 7 + 100_000_000 - 1_000 - BLOCK_ENV_ACCESS_COMPUTE_GAS);
        assert_eq!(detention.accessed(), VolatileDataAccess::TIMESTAMP);

        assert_eq!(detention.on_frame_end(InstructionResult::Stop, &mut frame, 0), None);
        assert_eq!((frame.remaining(), frame.reservoir()), (100_000_000 - 1_000, 7));
        assert_eq!(frame.state_gas_spilled(), 0);
    }

    /// A limit only goes down: a later read with room for more leaves it where it is.
    #[test]
    fn test_the_first_limit_binds_when_it_is_lower() {
        let mut detention = detaining();
        let mut frame = gas(100_000_000, 0);
        detention.on_frame_run(&mut frame, 0, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0, 0);
        assert!(frame.record_regular_cost(5_000));
        detention.commit_reads(VolatileDataAccess::ORACLE, &mut frame, 0, 0);
        assert_eq!(detention.compute_limit(), Some(BLOCK_ENV_ACCESS_COMPUTE_GAS));
        assert_eq!(frame.remaining(), BLOCK_ENV_ACCESS_COMPUTE_GAS - 5_000);
    }

    /// What a state charge drew from the withheld gas counts as spilled once released: that is
    /// where it would have come from, and a revert gives it back to regular gas as a spill.
    #[test]
    fn test_release_counts_what_state_drew_as_spilled() {
        let mut detention = detaining();
        let mut frame = gas(100_000_000, 0);
        detention.on_frame_run(&mut frame, 0, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0, 0);
        let withheld = 100_000_000 - BLOCK_ENV_ACCESS_COMPUTE_GAS;
        assert!(frame.record_state_cost(withheld + 10));
        assert_eq!(frame.remaining(), BLOCK_ENV_ACCESS_COMPUTE_GAS - 10);

        detention.on_frame_end(InstructionResult::Revert, &mut frame, 0);
        assert_eq!(frame.remaining(), BLOCK_ENV_ACCESS_COMPUTE_GAS - 10);
        assert_eq!(frame.reservoir(), 0);
        assert_eq!(frame.state_gas_spilled(), withheld + 10);
        frame.rollback_state_gas();
        assert_eq!(frame.remaining(), 100_000_000, "a revert gives all of it back");
    }

    /// Out of gas with gas withheld is the cap binding; out of gas without is the frame's own.
    #[test]
    fn test_out_of_gas_is_the_cap_only_with_gas_withheld() {
        let mut detention = detaining();
        let mut frame = gas(100_000_000, 0);
        detention.on_frame_run(&mut frame, 0, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0, 0);
        frame.spend_all();
        let stop = detention.on_frame_end(InstructionResult::OutOfGas, &mut frame, 0);
        assert_eq!(stop, Some((BLOCK_ENV_ACCESS_COMPUTE_GAS, BLOCK_ENV_ACCESS_COMPUTE_GAS)));

        let mut detention = detaining();
        let mut frame = gas(1_000_000, 0);
        detention.on_frame_run(&mut frame, 0, 0);
        detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0, 0);
        assert_eq!(frame.remaining(), 1_000_000, "nothing to withhold");
        assert_eq!(detention.on_frame_end(InstructionResult::OutOfGas, &mut frame, 0), None);

        for result in [InstructionResult::MemoryLimitOOG, InstructionResult::InvalidFEOpcode] {
            let mut detention = detaining();
            let mut frame = gas(100_000_000, 0);
            detention.on_frame_run(&mut frame, 0, 0);
            detention.commit_reads(VolatileDataAccess::TIMESTAMP, &mut frame, 0, 0);
            assert_eq!(detention.on_frame_end(result, &mut frame, 0), None, "{result:?}");
        }
    }

    /// A child's compute adds to its caller's; the caller's forwarded gas and the child's stipend
    /// do not, and after the child returns the caller's own gas accounts for it the same way.
    #[test]
    fn test_compute_spans_frames() {
        let mut detention = detaining();
        let mut caller = gas(100_000, 0);
        detention.on_frame_run(&mut caller, 0, 0);
        assert!(caller.record_regular_cost(9_000 + 60_000));
        detention.on_frame_suspend(&caller, 0);

        let mut child = gas(60_000 + 2_300, 0);
        detention.on_frame_run(&mut child, 1, 2_300);
        assert_eq!(detention.compute(&child, 1), 9_000 - 2_300);
        assert!(child.record_regular_cost(4_000));
        assert_eq!(detention.compute(&child, 1), 9_000 + 4_000 - 2_300);
        detention.on_frame_end(InstructionResult::Stop, &mut child, 1);

        caller.erase_cost(child.remaining());
        detention.on_frame_run(&mut caller, 0, 0);
        assert_eq!(detention.compute(&caller, 0), 9_000 + 4_000 - 2_300);
        assert_eq!((detention.suspended, detention.minted), (0, 0));
    }

    /// What a halting child burns is not compute: its caller's compute after it returns is what
    /// the child ran, not the gas the child was given.
    #[test]
    fn test_what_a_halt_burns_is_not_compute() {
        let mut detention = detaining();
        let mut caller = gas(1_000_000, 0);
        detention.on_frame_run(&mut caller, 0, 0);
        assert!(caller.record_regular_cost(500_000));
        detention.on_frame_suspend(&caller, 0);
        let mut child = gas(500_000, 0);
        detention.on_frame_run(&mut child, 1, 0);
        assert!(child.record_regular_cost(1_000));
        assert!(child.record_state_cost(3_000));
        assert_eq!(detention.on_frame_end(InstructionResult::InvalidFEOpcode, &mut child, 1), None);

        // The caller's regular gas spent now holds everything it forwarded.
        detention.on_frame_run(&mut caller, 0, 0);
        assert_eq!(detention.compute(&caller, 0), 1_000);
        assert_eq!(detention.burned, 500_000 - 1_000);
    }

    /// The switch holds for the frame that turned it off and every frame below it, turns back on
    /// when that frame returns, and cannot be turned back on from below.
    #[test]
    fn test_the_switch_is_scoped_to_a_subtree() {
        let mut detention = detaining();
        let mut frame = gas(1_000_000, 0);
        detention.on_frame_run(&mut frame, 0, 0);
        detention.on_frame_suspend(&frame, 0);
        let mut child = gas(100_000, 0);
        detention.on_frame_run(&mut child, 1, 0);
        detention.disable_access(1);
        assert!(detention.is_refusing());
        assert!(!detention.is_access_disabled(0));
        assert!(detention.is_access_disabled(1) && detention.is_access_disabled(2));
        detention.disable_access(2);
        assert!(detention.is_access_disabled(1), "a deeper frame keeps the shallower switch");
        assert!(!detention.enable_access(2), "a frame below cannot switch it back on");
        assert!(detention.refuses(VolatileDataAccess::TIMESTAMP));
        assert_eq!(detention.take_refused(), Some(VolatileDataAccess::TIMESTAMP));

        detention.on_frame_end(InstructionResult::Stop, &mut child, 1);
        assert!(!detention.is_access_disabled(1), "on again once the frame returned");
        detention.on_frame_run(&mut frame, 0, 0);
        assert!(!detention.is_refusing());

        detention.disable_access(0);
        assert!(detention.enable_access(0), "the frame that switched it off switches it on");
        assert!(!detention.refuses(VolatileDataAccess::TIMESTAMP));
    }

    /// A transaction that is not detained records nothing and caps nothing.
    #[test]
    fn test_an_exempt_transaction_is_not_detained() {
        let mut detention = Detention::default();
        detention.reset(false);
        let mut frame = gas(100_000_000, 0);
        detention.on_frame_run(&mut frame, 0, 0);
        detention.observe(VolatileDataAccess::TIMESTAMP);
        assert!(!detention.has_reads());
        detention.mark_before_execution(VolatileDataAccess::BENEFICIARY_BALANCE);
        assert_eq!(detention.compute_limit(), None);
        assert_eq!(detention.accessed(), VolatileDataAccess::empty());
    }
}
