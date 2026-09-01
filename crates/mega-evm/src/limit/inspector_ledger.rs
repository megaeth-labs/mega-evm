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
/// # The three numbers a receipt carries
///
/// A transaction's receipt reports its spent envelope, the refund applied to it, and — under
/// EIP-8037 — the state gas it consumed. The lanes are grouped by which of the three a rewrite
/// moves, because that is what decides whether the conservation law can see it:
///
/// - [`gas`](Self::gas), [`env`](Self::env), [`result`](Self::result) and
///   [`reservoir`](Self::reservoir) move the envelope, and are summed into
///   [`conjured_gas`](Self::conjured_gas), the law's `I` term;
/// - [`refund`](Self::refund) moves the refund, which the law — stated over `limit - remaining` —
///   cannot see at all;
/// - [`state_gas`](Self::state_gas) moves the receipt's state-gas figure, which the law does not
///   reach either.
///
/// All six are read by [`is_zero`](Self::is_zero), which is what the block guard asks.
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
    ///
    /// And it carries the gas of a result the inspector produced outright, by answering a frame
    /// with a synthetic outcome. That one is not a difference across a callback — no frame is
    /// built, so there is no EVM-produced number on the other side — but against the envelope the
    /// answering callback was handed, which the transaction did fund. The two are the same
    /// question either way, and they settle at the same point for the same reason: a returning or
    /// reverting outcome hands its gas back to the caller, a halting one hands nothing back and
    /// the whole envelope is destroyed whatever figure the outcome claimed.
    pub result: i128,

    /// The EIP-8037 state-gas pool the transaction ends holding, which is gas nothing funded.
    ///
    /// `MegaETH` runs with EIP-8037 off on every path and every spec, so no instruction can charge
    /// against a reservoir and no `MegaETH` site ever fills one: the reservoir a transaction ends
    /// with is zero unless an inspector wrote it. What a non-zero one does is move the envelope —
    /// the receipt reports `limit - remaining - reservoir` as spent, and the caller is reimbursed
    /// `remaining + reservoir + refunded` — so this lane is summed into
    /// [`conjured_gas`](Self::conjured_gas) alongside the three above.
    ///
    /// Unlike them it is settled once, at the transaction's own settlement point, rather than at a
    /// callback boundary. Two facts make that the only sound reading. revm propagates a reservoir
    /// between frames by *replacement* — a returning child's reservoir overwrites its caller's —
    /// so an edit made while a `NewFrame` action is already pending is erased by the child that
    /// action builds, and a boundary difference would book gas that moved nothing. And the
    /// `state_gas_spent` counter converts into a reservoir on a frame that fails, at a site no
    /// callback sees. Reading the final number instead covers both: it is exactly the part of
    /// every edit that survived, and `MegaETH` contributes none of it, so no difference has to be
    /// taken to isolate the inspector's share.
    pub reservoir: i128,

    /// Net EIP-8037 state gas the inspector wrote into the `state_gas_spent` counters.
    ///
    /// The reservoir's counterpart on the spending side, and dead for the same reason `MegaETH`
    /// never fills one — except at the two places revm reads it regardless of whether EIP-8037 is
    /// enabled: a successful transaction reports its final value on the receipt, and a failing
    /// frame folds it back into its caller's reservoir.
    ///
    /// The second of those two effects is already inside [`reservoir`](Self::reservoir) — the
    /// final reservoir is read after the fold — so this lane carries the first, and is
    /// deliberately not part of [`conjured_gas`](Self::conjured_gas): the receipt's state-gas
    /// figure is not the envelope, and adding it to the law's `I` term would make the law
    /// wrong by exactly this amount. Settled at the same point and for the same reasons as the
    /// lane above.
    pub state_gas: i128,

    /// Net gas the inspector wrote into the `refunded` counters of the `Gas` objects it is handed.
    ///
    /// A refund is the one number on a receipt the conservation law cannot see: the law is stated
    /// over `total_gas_spent`, which is `limit - remaining` and which no refund enters. What a
    /// refund does reach is `tx_gas_used` — what the sender actually pays — and the caller's
    /// reimbursement. So the lane exists for [`is_zero`](Self::is_zero) and the block guard behind
    /// it, and is deliberately kept out of [`conjured_gas`](Self::conjured_gas).
    ///
    /// # Nominal, in both senses
    ///
    /// The figure booked is what the inspector wrote, not what survived to the receipt.
    ///
    /// Not what survived the *cap*, because EIP-3529 caps the transaction's whole refund at a
    /// fifth of what it burnt, over the sum of every refund the transaction accumulated, at a
    /// point after the envelope is final and with no frame left standing. Splitting that cap
    /// between the EVM's own refunds and an inspector's needs a priority rule the protocol
    /// does not have — EVM-first, inspector-first and pro rata are all defensible, which means
    /// none of them is a measurement.
    ///
    /// And not what survived the *frame chain*, because revm hands a frame's refund to its caller
    /// only when the frame succeeded: an edit reaches the receipt exactly when every frame from
    /// the one that was edited up to the top returns successfully. That is a condition no callback
    /// boundary and no single settlement point can answer without a refund stack aligned to the
    /// EVM's frame lifecycle, which is the machinery this ledger deliberately does not have.
    ///
    /// Both directions of the choice are safe here because the lane feeds no identity.
    /// Over-stating it costs nothing; under-stating it would let a transaction whose receipt
    /// an inspector moved into a block, which is the one thing the lane exists to prevent.
    pub refund: i128,

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
        self.gas + self.env + self.result + self.reservoir
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
            self.reservoir == 0 &&
            self.state_gas == 0 &&
            self.refund == 0 &&
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
        let ledger = InspectorLedger { gas: 2_300, env: -2_300, ..InspectorLedger::default() };
        assert_eq!(ledger.conjured_gas(), 0);
        assert!(!ledger.is_zero(), "the lanes moved, even though they cancel");
    }

    /// The reservoir is envelope-moving gas and joins the law's term; the refund and the
    /// state-gas figure are not, and must not.
    #[test]
    fn test_only_the_envelope_moving_lanes_are_conjured_gas() {
        let reservoir = InspectorLedger { reservoir: 10_000, ..InspectorLedger::default() };
        assert_eq!(
            reservoir.conjured_gas(),
            10_000,
            "a reservoir lowers the envelope the receipt reports, so the law needs it back",
        );
        assert!(!reservoir.is_zero());

        for ledger in [
            InspectorLedger { refund: 20_000, ..InspectorLedger::default() },
            InspectorLedger { state_gas: 5_000, ..InspectorLedger::default() },
        ] {
            assert_eq!(
                ledger.conjured_gas(),
                0,
                "the law is stated over `limit - remaining`, which neither of these enters",
            );
            assert!(!ledger.is_zero(), "but the block guard still has to see it: {ledger:?}");
        }
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
