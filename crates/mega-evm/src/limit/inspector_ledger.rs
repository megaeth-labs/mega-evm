//! The ledger of what an inspector did to a transaction.
//!
//! `MegaETH` wraps every inspector it is handed in a measurement shim (`MeasuredInspector`), and
//! the shim books what it measures here. Nothing in this module enforces anything: the gas lanes
//! are exactly the part of a transaction's gas movement that the EVM did not produce, kept separate
//! so that enforcement can ignore it and the conservation law can account for it, and the two
//! counters record rewrites that move no gas at all.

/// One signed lane of the ledger, and how much traffic it carried.
///
/// # Why a lane is two numbers
///
/// The lanes answer two different questions, and one number cannot answer both.
///
/// The conservation law needs the **net**: gas an inspector wrote into one object and took back
/// out of another really has left the transaction's envelope where it was, and a law stated over
/// the gross would be wrong by exactly the round trip.
///
/// The block guard needs the **gross**: it asks whether the transaction was left alone, and two
/// edits that cancel are two edits. A `+1` before the frame reads its own remaining gas and a `−1`
/// after it has read it net to nothing and leave the frame holding a number the EVM would never
/// have given it — and the same cancellation split across two frames, where only one of the two
/// survives to the receipt, moves what the sender pays while netting to zero.
///
/// So the gross is not a diagnostic beside the net; it is the number
/// [`InspectorLedger::is_zero`] is defined over. [`book`](Self::book) moves both, which is what
/// makes it impossible to move a lane without the guard seeing it.
///
/// # Saturation
///
/// Both halves saturate. A ledger is a reported quantity that feeds no identity beyond the law's
/// own term, and a saturated lane still answers the guard's question the same way an exact one
/// would; an overflow panic on the inspected path would not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lane {
    /// The sum of every booking, signed — what the transaction's envelope actually moved by.
    net: i128,
    /// The sum of every booking's magnitude — how much traffic this lane carried, in either
    /// direction.
    gross: u128,
}

impl Lane {
    /// A lane that carried one booking of `net`.
    ///
    /// The gross is `|net|`, which is what a single booking always produces. This is the
    /// constructor for a caller stating an expectation over a lane moved in one direction; a lane
    /// moved in both needs [`of`](Self::of), because the two numbers are then independent.
    #[inline]
    pub const fn once(net: i128) -> Self {
        Self { net, gross: net.unsigned_abs() }
    }

    /// A lane with both numbers stated, for a caller expecting bookings in both directions.
    #[inline]
    pub const fn of(net: i128, gross: u128) -> Self {
        Self { net, gross }
    }

    /// What the transaction's envelope moved by on this lane.
    #[inline]
    pub const fn net(self) -> i128 {
        self.net
    }

    /// How much traffic this lane carried, counting both directions.
    #[inline]
    pub const fn gross(self) -> u128 {
        self.gross
    }

    /// Whether nothing was ever booked here.
    ///
    /// Read off the gross, not the net: a lane whose bookings cancelled carried traffic, and the
    /// whole point of the pair is that the guard can tell that from a lane nobody touched.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.gross == 0
    }

    /// Books one movement on this lane.
    #[inline]
    pub(crate) const fn book(&mut self, delta: i128) {
        self.net = self.net.saturating_add(delta);
        self.gross = self.gross.saturating_add(delta.unsigned_abs());
    }
}

/// What an inspector conjured, destroyed, rewrote, or had refused, as measured at the callback
/// boundaries.
///
/// # Why the boundary is a sound place to measure
///
/// The EVM does not execute inside an inspector callback. Every change to an interpreter's gas
/// counter, to the action it is holding, to its working state, or to a frame input's gas limit
/// that is visible across a callback's entry and exit is therefore the inspector's, by
/// construction rather than by attribution heuristics. The shim takes one snapshot before
/// delegating and one after, and the difference lands here.
///
/// # Sign convention
///
/// Every field measuring gas is a [`Lane`], whose net reads *from the transaction's point of
/// view*: a positive value is gas the inspector conjured — gas that exists in the execution but
/// that nothing debited from the transaction's envelope — and a negative value is gas it
/// destroyed. Both directions are recorded, because the conservation law needs the net; and each
/// lane carries the gross beside it, because the block guard needs to know a lane moved at all.
///
/// # What it does not measure
///
/// What a callback does behind the shim's back. An inspector reaches state that no argument it is
/// handed describes — the *contents* of the interpreter's stack, memory, return buffer, calldata
/// and code, and the journal — and telling whether any of those came back changed needs a snapshot
/// of unbounded state that no callback boundary can take at a cost the inspected path can carry.
/// Everything about the interpreter that *is* a constant-time reading is the exception, and the
/// shim takes all of it: a frame whose memory was grown, whose program counter was stepped past an
/// instruction, or whose return buffer was conjured lands on
/// [`interventions`](Self::interventions). A rewrite that leaves every one of those readings where
/// it was leaves this all-zero.
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
///   [`reservoir`](Self::reservoir) move the envelope, and their nets are summed into
///   [`conjured_gas`](Self::conjured_gas), the law's `I` term;
/// - [`refund`](Self::refund) moves the refund, which the law — stated over `limit - remaining` —
///   cannot see at all;
/// - [`state_gas`](Self::state_gas) moves the receipt's state-gas figure, which the law does not
///   reach either.
///
/// All six are read by [`is_zero`](Self::is_zero), which is what the block guard asks — through
/// their gross halves, so that a lane whose bookings cancelled is not a lane nobody touched.
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
    /// Gas the inspector wrote into interpreter gas counters, across every callback that is
    /// handed a live [`Interpreter`](revm::interpreter::Interpreter).
    ///
    /// A running frame's counter is the frame's own budget, so raising it hands the frame gas the
    /// caller never forwarded and lowering it takes gas away that the caller will never get back.
    ///
    /// A callback that removed the interpreter's pending action lands here too: with no action
    /// left the frame carries on spending what it holds, which is exactly what the counter is.
    pub gas: Lane,

    /// Gas the inspector wrote into frame *envelopes* — the `gas_limit` a call or create frame
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
    pub env: Lane,

    /// Gas the inspector wrote into a frame *result* — what the frame hands back to its
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
    pub result: Lane,

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
    pub reservoir: Lane,

    /// EIP-8037 state gas the inspector wrote into the `state_gas_spent` counters.
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
    pub state_gas: Lane,

    /// Gas the inspector wrote into the `refunded` counters of the `Gas` objects it is handed.
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
    /// The lane's gross half is what makes the second of those safe. A `+R` on a frame that
    /// survives and a `−R` on one that is rolled back are equal and opposite where they are
    /// booked and not where they land, so a net-only reading would call that pair untouched while
    /// the sender pays `R` less.
    ///
    /// Both directions of the choice are safe here because the lane feeds no identity.
    /// Over-stating it costs nothing; under-stating it would let a transaction whose receipt
    /// an inspector moved into a block, which is the one thing the lane exists to prevent.
    pub refund: Lane,

    /// How many rewrites the shim refused because their shape is forbidden.
    ///
    /// Two shapes are, and both for the same reason: the journal decision they would need to move
    /// with was already taken, at a point no callback can reach.
    ///
    /// - A `create_end` (or the `frame_end` after it) turning a non-successful contract creation
    ///   into a successful one. Such a rewrite runs after the journal has already reverted the
    ///   frame and after the deposit predicates have already rejected the code, so honouring it
    ///   would report a deployment that never happened.
    /// - Any of the three `*_end` callbacks moving the classification of a result *frame init*
    ///   produced across the success / revert / halt boundary. revm decides the journal inside
    ///   `make_call_frame` and `MegaETH`'s interceptors decide theirs before they return, so
    ///   honouring it would hand the caller an answer the state behind it contradicts.
    ///
    /// A non-zero count means the transaction was failed with an `EVMError::Custom` rather than
    /// given a receipt.
    pub rejected_rewrites: u32,

    /// How many rewrites the shim saw that change what the execution *did* rather than what it
    /// cost.
    ///
    /// The six gas lanes above answer "did the transaction's numbers move". This answers the
    /// other half — "was the transaction left alone" — for the part of it a callback boundary can
    /// see, which is the arguments the shim itself is handed and the constant-time readings it
    /// can take off a live interpreter:
    ///
    /// - a frame result whose classification or returned output came back changed, at each of the
    ///   three callbacks that can change one (`call_end`, `create_end`, `frame_end` — revm runs
    ///   the variant-specific one and then the generic one over the same result, so each is
    ///   counted where it happens rather than once at the end);
    /// - a finished outcome's metadata — where a call's return data lands in its caller's memory,
    ///   which address a creation reports, the two EIP-8037 and precompile-log flags beside them —
    ///   which sits outside the `InterpreterResult` those callbacks also hold;
    /// - a frame's inputs edited anywhere but in their gas limit, at each of the three callbacks
    ///   that can edit them (`frame_start`, `call`, `create`);
    /// - a frame the inspector answered itself, with a synthetic outcome instead of letting the
    ///   EVM build it;
    /// - any constant-time reading of a live interpreter's working set, at each of the four
    ///   callbacks handed one. The rule the shim's snapshot is built on is stated over the cost of
    ///   the reading rather than over a list, so it covers the program counter and the code's
    ///   identity, revm's `continue_execution` flag, the stack's length, the return buffer's
    ///   identity, the memory's size and window offset, the memo of how far that memory has been
    ///   paid for, the frame's four identifying fields and its calldata's identity, the static
    ///   flag and the spec id. Two of those are rewrites with no other trace at all: a stepped
    ///   program counter deletes an instruction from the frame, and moving the memory together
    ///   with its memo leaves every interpreter invariant intact while changing what the next
    ///   expanding opcode is charged.
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
        self.gas.net() + self.env.net() + self.result.net() + self.reservoir.net()
    }

    /// Whether the inspector left the transaction's gas accounting exactly as the EVM produced it.
    ///
    /// True for every observation-only inspector, and for every transaction that ran without one.
    /// Not the converse of "an inspector changed something": see the type's own documentation for
    /// the rewrites that move no gas and so leave this true.
    ///
    /// Each lane is asked through its gross half, so a lane an inspector moved and moved back is
    /// not a lane it left alone.
    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.gas.is_zero() &&
            self.env.is_zero() &&
            self.result.is_zero() &&
            self.reservoir.is_zero() &&
            self.state_gas.is_zero() &&
            self.refund.is_zero() &&
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
            gas: Lane::once(2_300),
            env: Lane::once(-2_300),
            ..InspectorLedger::default()
        };
        assert_eq!(ledger.conjured_gas(), 0);
        assert!(!ledger.is_zero(), "the lanes moved, even though they cancel");
    }

    /// ★ Two bookings on *one* lane that cancel are the shape a net-only guard admitted.
    ///
    /// The net is what the conservation law needs and it is genuinely zero here — the transaction's
    /// envelope really did end where it started. What is not zero is that the lane carried
    /// traffic, and between the two bookings the execution saw a number the EVM would never have
    /// produced.
    #[test]
    fn test_bookings_that_cancel_on_one_lane_are_not_zero() {
        let mut lane = Lane::default();
        lane.book(1);
        lane.book(-1);
        assert_eq!(lane.net(), 0, "the envelope is unmoved, and the law must read it that way");
        assert_eq!(lane.gross(), 2, "but the lane carried two bookings");
        assert!(!lane.is_zero());

        let ledger = InspectorLedger { gas: lane, ..InspectorLedger::default() };
        assert_eq!(ledger.conjured_gas(), 0);
        assert!(!ledger.is_zero(), "the guard must refuse a transaction whose lanes cancelled");
    }

    /// The same, on the lane where the two halves land in different frames — one that survives to
    /// the receipt and one the journal rolls back.
    #[test]
    fn test_refund_bookings_that_cancel_are_not_zero() {
        let mut refund = Lane::default();
        refund.book(2_000);
        refund.book(-2_000);
        let ledger = InspectorLedger { refund, ..InspectorLedger::default() };
        assert_eq!(
            ledger.conjured_gas(),
            0,
            "the refund lane is not a term of the law in either direction",
        );
        assert!(!ledger.is_zero());
    }

    /// A lane nobody booked is the only zero lane.
    #[test]
    fn test_an_untouched_lane_is_the_only_zero_one() {
        assert!(Lane::default().is_zero());
        assert!(!Lane::once(1).is_zero());
        assert!(!Lane::once(-1).is_zero());
        assert!(!Lane::of(0, 2).is_zero(), "a cancelled lane is not an untouched one");
    }

    /// `once` states the gross a single booking produces, which is what a caller expecting one
    /// booking means.
    #[test]
    fn test_once_is_a_single_booking() {
        let mut lane = Lane::default();
        lane.book(-2_300);
        assert_eq!(lane, Lane::once(-2_300));
    }

    /// Both halves saturate rather than overflow.
    #[test]
    fn test_a_lane_saturates() {
        let mut lane = Lane::of(i128::MAX, u128::MAX);
        lane.book(i128::MAX);
        assert_eq!(lane.net(), i128::MAX);
        assert_eq!(lane.gross(), u128::MAX);

        let mut down = Lane::of(i128::MIN, 0);
        down.book(i128::MIN);
        assert_eq!(down.net(), i128::MIN);
        assert_eq!(down.gross(), i128::MIN.unsigned_abs());
    }

    /// The reservoir is envelope-moving gas and joins the law's term; the refund and the
    /// state-gas figure are not, and must not.
    #[test]
    fn test_only_the_envelope_moving_lanes_are_conjured_gas() {
        let reservoir =
            InspectorLedger { reservoir: Lane::once(10_000), ..InspectorLedger::default() };
        assert_eq!(
            reservoir.conjured_gas(),
            10_000,
            "a reservoir lowers the envelope the receipt reports, so the law needs it back",
        );
        assert!(!reservoir.is_zero());

        for ledger in [
            InspectorLedger { refund: Lane::once(20_000), ..InspectorLedger::default() },
            InspectorLedger { state_gas: Lane::once(5_000), ..InspectorLedger::default() },
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
