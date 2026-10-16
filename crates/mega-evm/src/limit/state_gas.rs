//! What the state-gas limit counts: the state gas the running transaction holds.
//!
//! EIP-8037 charges state gas for exactly the state a transaction adds — a fresh storage slot, a
//! new account, the bytes of deployed code, a delegation — at the state's own price, scaled by the
//! SALT bucket it lands in. So the state a transaction grows is the state gas it spends, and a
//! limit on that gas is the limit on its growth; nothing counts new accounts and slots beside it.
//!
//! revm keeps state gas on each frame's own gas, net of what the frame refilled, and merges a
//! frame's into its caller's when the frame succeeds; a frame that fails rolls its own back. What
//! the transaction holds at any point is therefore what it was charged before its first frame,
//! plus what every frame on the call stack holds. The first part is fixed once the first frame
//! starts, and every frame below the running one is suspended, so each frame's share of the rest
//! is fixed while it runs: [`StateGasMeter`] keeps, for each frame, what the transaction holds
//! outside it, and the running frame adds what it holds itself.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

/// The state gas the running transaction holds outside the running frame.
///
/// One entry per frame on the call stack, pushed and popped with the frame's lane.
#[derive(Clone, Debug, Default)]
pub(crate) struct StateGasMeter {
    /// The state gas charged before the first frame: the account a deposit-like transaction
    /// creates for its caller, the applied EIP-7702 authorities, and the recipient or created
    /// account EIP-2780 charges the first frame for.
    before_frames: i64,
    /// For each frame on the call stack, the state gas the transaction holds outside it: what was
    /// charged before the first frame, and what each frame below it held when the frame above it
    /// started.
    outside: Vec<i64>,
    /// The state gas the frame starting the next frame holds, until that frame's entry is pushed.
    caller: i64,
}

impl StateGasMeter {
    /// Clears the meter for a new transaction, keeping its allocation.
    pub(crate) fn reset(&mut self) {
        self.before_frames = 0;
        self.outside.clear();
        self.caller = 0;
    }

    /// Records the state gas charged before the first frame.
    pub(crate) const fn set_before_frames(&mut self, spent: i64) {
        self.before_frames = spent;
    }

    /// Records the state gas the frame starting the next frame holds.
    pub(crate) const fn note_caller(&mut self, held: i64) {
        self.caller = held;
    }

    /// Pushes the entry of a frame about to start: what the transaction holds outside it is what
    /// it holds outside its caller plus what its caller holds, or, for the first frame, what was
    /// charged before any frame.
    ///
    /// A frame no caller noted anything for — one an inspector answered before it reached frame
    /// init — never runs, so what it is given is never read.
    pub(crate) fn push(&mut self) {
        let caller = core::mem::take(&mut self.caller);
        let outside = match self.outside.last() {
            Some(outside) => outside.saturating_add(caller),
            None => self.before_frames,
        };
        self.outside.push(outside);
    }

    /// Pops the entry of the frame that returned.
    pub(crate) fn pop(&mut self) {
        self.outside.pop();
    }

    /// The state gas the transaction holds while the running frame holds `running`; outside any
    /// frame, `running` is all of it.
    ///
    /// A frame can hold less than nothing — it refilled a slot a caller of its filled — but never
    /// by more than its callers hold, so the sum is never below zero.
    pub(crate) fn held(&self, running: i64) -> u64 {
        let outside = self.outside.last().copied().unwrap_or_default();
        u64::try_from(outside.saturating_add(running)).unwrap_or_default()
    }

    /// The number of entries, which is the number of frames on the call stack.
    pub(crate) fn depth(&self) -> usize {
        self.outside.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each frame is given what the transaction holds outside it: the state gas charged before
    /// the first frame, and what each caller held when it started the next frame. A frame that
    /// returned leaves its caller where it was.
    #[test]
    fn test_each_frame_holds_what_its_callers_held_when_it_started() {
        let mut meter = StateGasMeter::default();
        assert_eq!(meter.held(7), 7, "outside any frame, what is charged is all of it");

        meter.set_before_frames(100);
        meter.push();
        assert_eq!(meter.held(0), 100, "the first frame starts on what was charged before it");
        assert_eq!(meter.held(20), 120);

        meter.note_caller(20);
        meter.push();
        assert_eq!(meter.held(5), 125, "a child counts its caller's twenty");
        meter.note_caller(5);
        meter.push();
        assert_eq!(meter.held(-5), 120, "a grandchild that refilled its caller's fill");
        assert_eq!(meter.depth(), 3);

        meter.pop();
        meter.pop();
        assert_eq!(meter.held(30), 130, "the first frame, holding its child's merged gas");

        // A frame nobody noted a caller for starts on what its caller's caller left it.
        meter.push();
        assert_eq!(meter.held(0), 100);

        meter.reset();
        assert_eq!(meter.depth(), 0);
        meter.push();
        assert_eq!(meter.held(0), 0, "a reset forgets what was charged before the frames");
    }
}
