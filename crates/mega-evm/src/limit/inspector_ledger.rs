//! The ledger of what an inspector did to a transaction.
//!
//! Nothing here enforces anything. The gas lanes are the part of a transaction's gas movement the
//! EVM did not produce, kept apart so enforcement can ignore it and the conservation law can
//! account for it; the two counters record rewrites that move no gas at all.

/// One signed lane of the ledger, and how much traffic it carried.
///
/// Two numbers because two consumers ask different questions. The conservation law needs the
/// **net**: gas written into one object and taken back out of another really did leave the
/// envelope where it was. The block guard needs the **gross**: two edits that cancel are two
/// edits, and in between them the frame held a number the EVM would never have given it — or the
/// two landed in different frames and only one survived to the receipt.
///
/// So the gross is not a diagnostic beside the net; it is what [`InspectorLedger::is_zero`] is
/// defined over.
///
/// Both halves saturate. A ledger feeds no identity beyond the law's own term, and a saturated
/// lane answers the guard the same way an exact one would; an overflow panic would not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lane {
    /// The sum of every booking, signed — what the transaction's envelope actually moved by.
    net: i128,
    /// The sum of every booking's magnitude — how much traffic this lane carried, in either
    /// direction.
    gross: u128,
}

impl Lane {
    /// A lane that carried one booking of `net`, whose gross is therefore `|net|`.
    ///
    /// A lane moved in both directions needs [`of`](Self::of): the two numbers are then
    /// independent.
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
        self.book_crossing(delta);
        self.book_movement(delta);
    }

    /// Records that an edit of `delta` crossed a callback boundary, without saying yet whether it
    /// moved the transaction's envelope.
    ///
    /// The pair with [`book_movement`](Self::book_movement), for the lanes that cannot answer the
    /// second question where they answer the first. An edit to a frame's result moves the envelope
    /// only if the frame hands its remainder back, which the classification decides and no
    /// boundary knows — so the traffic is recorded here, at the boundary, and the movement is
    /// booked at the frame's settlement point.
    ///
    /// Splitting them is what keeps the guard's question answerable on those lanes. An edit whose
    /// frame then halts moves nothing and must stay out of the net, but it is still an edit the
    /// inspector made, and one that can change what the transaction produces before the
    /// classification catches up with it.
    #[inline]
    pub(crate) const fn book_crossing(&mut self, delta: i128) {
        self.gross = self.gross.saturating_add(delta.unsigned_abs());
    }

    /// Books a movement whose traffic [`book_crossing`](Self::book_crossing) already recorded.
    #[inline]
    pub(crate) const fn book_movement(&mut self, delta: i128) {
        self.net = self.net.saturating_add(delta);
    }
}

/// What an inspector conjured, destroyed, rewrote, or had refused, as measured at the callback
/// boundaries.
///
/// The EVM does not execute inside a callback, so anything visible across one is the inspector's
/// by construction rather than by attribution. The shim snapshots before delegating and after, and
/// the difference lands here.
///
/// # Sign convention
///
/// Every gas field is a [`Lane`] whose net reads from the transaction's point of view: positive is
/// gas that exists in the execution but that nothing debited from the envelope, negative is gas
/// the envelope funded that no frame received.
///
/// # What it does not measure
///
/// What a callback does behind the shim's back: the *contents* of the interpreter's stack, memory,
/// return buffer, calldata and code, and the journal. Telling whether those came back changed
/// needs a snapshot of unbounded state that a per-opcode boundary cannot take. Every constant-time
/// reading of the interpreter is the exception and all of it is taken, landing on
/// [`interventions`](Self::interventions).
///
/// So an empty ledger says no gas moved that the EVM did not move, and nothing the shim was handed
/// came back different. It does not say the transaction is the one the EVM would have produced
/// alone.
///
/// # Why the lanes are grouped as they are
///
/// A receipt reports its spent envelope, the refund applied to it, and — under EIP-8037 — the state
/// gas consumed. Which of the three a lane moves is what decides whether the conservation law can
/// see it:
///
/// - [`gas`](Self::gas), [`env`](Self::env), [`result`](Self::result) and
///   [`reservoir`](Self::reservoir) move the envelope, and their nets sum into
///   [`conjured_gas`](Self::conjured_gas), the law's `I` term;
/// - [`refund`](Self::refund) moves the refund, which the law — stated over `limit - remaining` —
///   cannot see;
/// - [`state_gas`](Self::state_gas) moves the receipt's state-gas figure, which it cannot reach
///   either.
///
/// All six are read by [`is_zero`](Self::is_zero) through their gross halves.
///
/// # What consumes it
///
/// [`conjured_gas`](Self::conjured_gas) is the term the destroyed-remainder derivation adds to the
/// envelope; without it, gas created out of nothing reads as the transaction having spent less
/// than it did and the derived total can go negative. Everything else here is reported and nothing
/// more — no limit is compared against it, and enforcement never sees an inspector's adjustment,
/// because the site that books one shifts the checkpoint baseline by the same amount.
///
/// Cumulative over the transaction and deliberately not a per-frame stack. A caller wanting one
/// frame's aggregate reads the ledger at that frame's entry and exit and subtracts; the type is
/// `Copy` and every field is a running total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InspectorLedger {
    /// Gas the inspector wrote into interpreter gas counters, at every callback handed a live
    /// [`Interpreter`](revm::interpreter::Interpreter).
    ///
    /// A running frame's counter is its own budget, so raising it hands the frame gas the caller
    /// never forwarded and lowering it takes gas the caller will never get back. A callback that
    /// removed the pending action lands here too: with no action left, the frame carries on
    /// spending exactly what the counter holds.
    pub gas: Lane,

    /// Gas the inspector wrote into a frame *envelope* — the `gas_limit` a call or create frame is
    /// about to be built with.
    ///
    /// The caller's own `CALL` / `CREATE` opcode debited the forwarded amount before any callback
    /// ran, so a raised limit is gas nobody paid for and a lowered one is gas the caller paid for
    /// and no frame receives.
    ///
    /// A synthetic outcome moves this lane's net by nothing: the frame is intercepted and the EVM
    /// never reads the inputs the same callback edited. Gas the inspector then sizes that outcome
    /// from travels on [`result`](Self::result) instead.
    ///
    /// The lane also carries an edit made one step earlier, to the `gas_limit` inside a pending
    /// `NewFrame` action. That object is the one the caller's opcode produced, so its debit is
    /// already behind it and a later interception cannot un-make the edit — it is booked either
    /// way. Its traffic is booked where it is made and its movement when the child's frame-start
    /// callback can tell an edit from an interception.
    pub env: Lane,

    /// Gas the inspector wrote into a frame *result* — what the frame hands back to its caller.
    ///
    /// The lane's two halves are booked in two different places, because the two questions are
    /// answered in two different places. Whether an edit *moved the envelope* depends on how the
    /// frame ends: a returning or reverting frame's remainder is reclaimed by its caller, a
    /// halting one's is not handed back at all. Only the frame's settlement point knows that, so
    /// it books the net. Whether the inspector *made* an edit is known at the boundary, so that is
    /// where the traffic is booked.
    ///
    /// Splitting them is load-bearing rather than tidy. An edit staged at `step_end` into a
    /// construction frame's pending `Return` action is charged the code deposit out of that same
    /// action before anything settles, so it can turn a successful creation into an `OutOfGas`
    /// that deploys nothing — while the classification and output a boundary compares stay exactly
    /// where they were. Booking only the net would leave that transaction reading as untouched.
    ///
    /// The same lane carries the gas of a result the inspector produced outright by answering a
    /// frame with a synthetic outcome. There is no EVM-produced number on the other side, so it is
    /// measured against the envelope the answering callback was handed, which the transaction did
    /// fund; it settles at the same point and by the same classification.
    pub result: Lane,

    /// The EIP-8037 state-gas pool the transaction ends holding, which is gas nothing funded.
    ///
    /// `MegaETH` runs with EIP-8037 off on every path and every spec, so a non-zero reservoir is
    /// the inspector's in whole. It moves the envelope — the receipt reports
    /// `limit - remaining - reservoir` as spent — so it joins
    /// [`conjured_gas`](Self::conjured_gas).
    ///
    /// Settled once from the final figure rather than differenced at a boundary, for two reasons
    /// that each rule a boundary out. revm propagates a reservoir between frames by *replacement*,
    /// so an edit made with a `NewFrame` action pending is erased by the child that action builds.
    /// And `state_gas_spent` converts into a reservoir on a failing frame, at a site no callback
    /// sees. The final number is exactly the part of every edit that survived.
    pub reservoir: Lane,

    /// EIP-8037 state gas the inspector wrote into the `state_gas_spent` counters.
    ///
    /// The reservoir's counterpart on the spending side, and settled the same way. revm reads it
    /// at two places regardless of whether the EIP is enabled: a successful transaction reports
    /// its final value, and a failing frame folds it into its caller's reservoir. The second is
    /// already inside [`reservoir`](Self::reservoir), so this lane carries the first — and stays
    /// out of [`conjured_gas`](Self::conjured_gas), because the receipt's state-gas figure is not
    /// the envelope and adding it would make the law wrong by exactly this amount.
    pub state_gas: Lane,

    /// Gas the inspector wrote into the `refunded` counters of the `Gas` objects it is handed.
    ///
    /// The one receipt number the conservation law cannot see: the law is stated over
    /// `limit - remaining`, which no refund enters. What a refund does reach is what the sender
    /// pays. So the lane exists for [`is_zero`](Self::is_zero) and is kept out of
    /// [`conjured_gas`](Self::conjured_gas).
    ///
    /// Nominal in two senses, and deliberately so. Not what survived the EIP-3529 *cap*, because
    /// splitting that cap between the EVM's refunds and an inspector's needs a priority rule the
    /// protocol does not have — EVM-first, inspector-first and pro rata are all defensible, so
    /// none of them is a measurement. And not what survived the *frame chain*, because a refund
    /// reaches the receipt only if every frame above the edited one returns successfully, which no
    /// boundary can answer without a refund stack aligned to the frame lifecycle.
    ///
    /// The gross half is what makes the second safe: a `+R` on a surviving frame and a `−R` on a
    /// rolled-back one are equal and opposite where they are booked and not where they land.
    /// Over-stating costs nothing here, because the lane feeds no identity; under-stating would
    /// admit a transaction whose receipt an inspector moved.
    pub refund: Lane,

    /// How many rewrites the shim refused because their shape is forbidden.
    ///
    /// Two shapes are, both because the journal decision they would have to move with was already
    /// taken where no callback can reach it: a contract creation rewritten from failure into
    /// success, after the journal reverted and the deposit predicates rejected the code; and any
    /// `*_end` moving the classification of a result *frame init* produced across the success /
    /// revert / halt boundary, which revm and `MegaETH`'s interceptors both decide before
    /// returning.
    ///
    /// A non-zero count means the transaction failed with an `EVMError::Custom` rather than being
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
