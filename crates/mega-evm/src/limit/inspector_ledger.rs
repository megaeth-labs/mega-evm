//! The ledger of what an inspector did to a transaction.
//!
//! `MegaETH` wraps every inspector it is handed in a measurement shim (`MeasuredInspector`), and
//! the shim books what it measures here. Nothing in this module enforces anything: the gas lanes
//! are exactly the part of a transaction's gas movement that the EVM did not produce, kept separate
//! so that enforcement can ignore it and the conservation law can account for it, and the two
//! counters record rewrites that move no gas at all.

/// What an inspector conjured, destroyed, rewrote, or had refused, as measured at the callback
/// boundaries.
///
/// # Why the boundary is a sound place to measure
///
/// The EVM does not execute inside an inspector callback. Every change to an interpreter's gas
/// counter, to the action it is holding, or to a frame input's gas limit that is visible across a
/// callback's entry and exit is therefore the inspector's, by construction rather than by
/// attribution heuristics. The shim takes one snapshot before delegating and one after, and the
/// difference lands here.
///
/// # Sign convention
///
/// Every field measuring gas is signed and reads *from the transaction's point of view*: a positive
/// value is gas the inspector conjured — gas that exists in the execution but that nothing debited
/// from the transaction's envelope — and a negative value is gas it destroyed. Both directions are
/// recorded, because the conservation law needs the net, not the gross.
///
/// # What it does not measure
///
/// What a callback does behind the shim's back. An inspector reaches state that no argument it is
/// handed describes — the interpreter's stack and memory, the journal — and telling whether any of
/// those came back changed needs a snapshot of unbounded state that no callback boundary can take
/// at a cost the inspected path can carry. Those rewrites leave this all-zero.
///
/// So an empty ledger says two things: no gas moved that the EVM did not move, and nothing the
/// shim was handed came back different. It does not say the transaction is the one the EVM would
/// have produced alone.
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
    ///
    /// A callback that removed the interpreter's pending action lands here too: with no action
    /// left the frame carries on spending what it holds, which is exactly what the counter is.
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
    /// The same lane carries an edit made one step earlier, to the `gas_limit` inside a pending
    /// `NewFrame` action — the object the caller's `CALL` / `CREATE` opcode produced, before any
    /// callback saw the inputs built from it. It is booked whether or not the frame is then
    /// intercepted: an interception discards inputs the *same* callback edited a moment before,
    /// which is why that edit reaches nothing, and it cannot un-make an edit another callback made
    /// to the action the caller's debit is already behind.
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
    ///
    /// The same lane carries an edit made one step earlier, to the gas inside a pending `Return`
    /// action. That action *is* the frame's result a moment later, so the two are one number
    /// measured on either side of the classification, and they settle together.
    pub result: i128,

    /// How many rewrites the shim refused because their shape is forbidden.
    ///
    /// Today exactly one shape is: a `create_end` (or the `frame_end` after it) turning a
    /// non-successful contract creation into a successful one. Such a rewrite runs after the
    /// journal has already reverted the frame and after the deposit predicates have already
    /// rejected the code, so honouring it would report a deployment that never happened.
    pub rejected_rewrites: u32,

    /// How many rewrites the shim saw that change what the execution *did* rather than what it
    /// cost.
    ///
    /// The three gas lanes above answer "did the transaction's numbers move". This answers the
    /// other half — "was the transaction left alone" — for the part of it a callback boundary can
    /// see, which is the arguments the shim itself is handed:
    ///
    /// - a frame result whose classification or returned output came back changed, at each of the
    ///   three callbacks that can change one (`call_end`, `create_end`, `frame_end` — revm runs
    ///   the variant-specific one and then the generic one over the same result, so each is
    ///   counted where it happens rather than once at the end);
    /// - a frame's inputs edited anywhere but in their gas limit, at each of the three callbacks
    ///   that can edit them (`frame_start`, `call`, `create`);
    /// - a frame the inspector answered itself, with a synthetic outcome instead of letting the
    ///   EVM build it.
    ///
    /// Gas edits are deliberately excluded — a gas limit or a result's remaining gas moving is
    /// what the lanes above are for, and counting it here would say the same thing twice.
    ///
    /// A classification rewrite is the shape that made this lane necessary: it moves no gas
    /// anywhere, so every gas lane stays zero while the transaction produces different state and
    /// a different receipt.
    pub interventions: u32,
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
    /// Not the converse of "an inspector changed something": see the type's own documentation for
    /// the rewrites that move no gas and so leave this true.
    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.gas == 0 &&
            self.env == 0 &&
            self.result == 0 &&
            self.rejected_rewrites == 0 &&
            self.interventions == 0
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
        let ledger = InspectorLedger {
            gas: 2_300,
            env: -2_300,
            result: 0,
            rejected_rewrites: 0,
            interventions: 0,
        };
        assert_eq!(ledger.conjured_gas(), 0);
        assert!(!ledger.is_zero(), "the lanes moved, even though they cancel");
    }

    /// A refused rewrite moves no gas but must still show the transaction was not left alone.
    #[test]
    fn test_a_rejected_rewrite_alone_is_not_zero() {
        let ledger = InspectorLedger { rejected_rewrites: 1, ..InspectorLedger::default() };
        assert_eq!(ledger.conjured_gas(), 0);
        assert!(!ledger.is_zero());
    }

    /// A classification rewrite is the shape the gas lanes cannot see: it moves nothing, so the
    /// only thing standing between it and an all-zero ledger is this counter.
    #[test]
    fn test_an_intervention_alone_is_not_zero() {
        let ledger = InspectorLedger { interventions: 1, ..InspectorLedger::default() };
        assert_eq!(ledger.conjured_gas(), 0);
        assert!(!ledger.is_zero());
    }

    #[test]
    fn test_default_ledger_is_zero() {
        assert!(InspectorLedger::default().is_zero());
    }
}
