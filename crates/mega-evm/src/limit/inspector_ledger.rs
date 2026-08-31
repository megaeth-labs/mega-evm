//! The ledger of what an inspector did to a transaction's gas accounting.
//!
//! `MegaETH` wraps every inspector it is handed in a measurement shim (`MeasuredInspector`), and
//! the shim books what it measures here. Nothing in this module enforces anything: the numbers it
//! holds are exactly the part of a transaction's gas movement that the EVM did not produce, kept
//! separate so that enforcement can ignore it and the conservation law can account for it.

/// What an inspector conjured, destroyed, or refused, as measured at the callback boundaries.
///
/// # Why the boundary is a sound place to measure
///
/// The EVM does not execute inside an inspector callback. Every change to an interpreter's gas
/// counter, or to a frame input's gas limit, that is visible across a callback's entry and exit is
/// therefore the inspector's, by construction rather than by attribution heuristics. The shim takes
/// one snapshot before delegating and one after, and the difference lands here.
///
/// # Sign convention
///
/// Every field measuring gas is signed and reads *from the transaction's point of view*: a positive
/// value is gas the inspector conjured — gas that exists in the execution but that nothing debited
/// from the transaction's envelope — and a negative value is gas it destroyed. Both directions are
/// recorded, because the conservation law needs the net, not the gross.
///
/// # What consumes it
///
/// - [`conjured_gas`](Self::conjured_gas) is the term the destroyed-remainder derivation adds to
///   the envelope, so that a transaction run under a rewriting inspector still satisfies `destroyed
///   = spent + minted + conjured − non_compute − enforced`. Without it, gas the inspector created
///   out of nothing would show up as the transaction having spent less than it really did, and the
///   derived destroyed total would go negative.
/// - The ledger as a whole is a *reported* quantity. No resource limit is ever compared against it,
///   and enforcement never sees an inspector's adjustment: `record_inspector_gas_adjustment` shifts
///   the checkpoint baseline by the same amount it books here, so the compute-gas measurement of a
///   frame covers the work the EVM performed and nothing else.
///
/// # Reading a frame-level aggregate
///
/// The ledger is cumulative over the whole transaction and is deliberately not a per-frame stack:
/// aligning another stack with the EVM's frame lifecycle is exactly the machinery the frame-loop
/// rework replaces. A caller that wants what an inspector did to *one* frame reads the whole
/// ledger at the frame's entry and again at its exit and takes the difference — the type is `Copy`
/// and every field is a running total, so the difference of two readings is the aggregate over the
/// window between them, whatever happened inside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InspectorLedger {
    /// Net gas the inspector wrote into interpreter gas counters, across every callback that is
    /// handed a live [`Interpreter`](revm::interpreter::Interpreter).
    ///
    /// A running frame's counter is the frame's own budget, so raising it hands the frame gas the
    /// caller never forwarded and lowering it takes gas away that the caller will never get back.
    pub gas: i128,

    /// Net gas the inspector wrote into frame *envelopes* — the `gas_limit` a call or create frame
    /// is about to be built with.
    ///
    /// The caller was debited the forwarded amount by its own `CALL` / `CREATE` opcode, before any
    /// inspector callback ran, so a raised limit is gas nobody paid for and a lowered one is gas
    /// the caller paid for and no frame ever receives.
    ///
    /// Only adjustments that actually reach a frame are booked. When the callback returns a
    /// synthetic outcome it has intercepted the frame entirely, and the EVM never reads the inputs
    /// it edited — so the edit by itself moves nothing. (The inspector can of course read its own
    /// edit back and size the synthetic outcome from it. That gas travels through the result lane
    /// below, not through this one.)
    ///
    /// Adjustments to a frame's *result* gas belong to [`result`](Self::result), which is booked
    /// from the frame's own settlement point rather than from a callback boundary.
    pub env: i128,

    /// Net gas the inspector wrote into a frame *result* — what the frame hands back to its
    /// caller — across the last callback that can rewrite that result.
    ///
    /// Unlike the other two lanes this one cannot be booked at the callback boundary, because
    /// whether the edit moves anything depends on how the frame ends: a returning or reverting
    /// frame's remaining gas is reclaimed by its caller, so an edit to it changes what the
    /// transaction spends, while a halting frame's is not handed back at all and an edit to it
    /// changes nothing. The frame's settlement point knows the final classification and books
    /// this lane only in the first case; in the second it reconstructs the EVM's own number and
    /// settles the destroyed remainder against that instead.
    pub result: i128,

    /// How many rewrites the shim refused because their shape is forbidden.
    ///
    /// Today exactly one shape is: a `create_end` (or the `frame_end` after it) turning a
    /// non-successful contract creation into a successful one. Such a rewrite runs after the
    /// journal has already reverted the frame and after the deposit predicates have already
    /// rejected the code, so honouring it would report a deployment that never happened.
    pub rejected_rewrites: u32,
}

impl InspectorLedger {
    /// The gas the inspector conjured, net of what it destroyed — the term the destroyed-remainder
    /// derivation adds to the transaction's envelope.
    #[inline]
    pub const fn conjured_gas(&self) -> i128 {
        self.gas + self.env + self.result
    }

    /// Whether the inspector left the transaction's gas accounting exactly as the EVM produced it.
    ///
    /// True for every observation-only inspector, and for every transaction that ran without one.
    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.gas == 0 && self.env == 0 && self.result == 0 && self.rejected_rewrites == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The derivation term is the net of both lanes, so an injection into an interpreter counter
    /// and a matching reduction of a frame's envelope cancel — the transaction's envelope really is
    /// unmoved in that case.
    #[test]
    fn test_conjured_gas_is_the_net_of_both_lanes() {
        let ledger = InspectorLedger { gas: 2_300, env: -2_300, result: 0, rejected_rewrites: 0 };
        assert_eq!(ledger.conjured_gas(), 0);
        assert!(!ledger.is_zero(), "the lanes moved, even though they cancel");
    }

    /// A refused rewrite moves no gas but must still show the transaction was not left alone.
    #[test]
    fn test_a_rejected_rewrite_alone_is_not_zero() {
        let ledger = InspectorLedger { gas: 0, env: 0, result: 0, rejected_rewrites: 1 };
        assert_eq!(ledger.conjured_gas(), 0);
        assert!(!ledger.is_zero());
    }

    #[test]
    fn test_default_ledger_is_zero() {
        assert!(InspectorLedger::default().is_zero());
    }
}
