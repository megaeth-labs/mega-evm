//! The shadow: the lanes held to the journal's ledger wherever the two must agree.
//!
//! The ledger ([`Ledger`]) takes every decision; the lanes ([`AdditionalLimit`]) count the same
//! writes beside it until they go. Under the `ledger-shadow` feature the engine compares the two
//! at the points of a transaction's life where both describe the same thing, and panics on the
//! first mismatch. Each point is a function here, called where the point is; without the feature
//! every one of them compiles to nothing.
//!
//! - After a completed write is counted, a code deposit, an admitted hint and the keyless take-back
//!   of a creator's record: the verdicts, then what the transaction keeps, the history bytes beside
//!   its records, the number of frames, the running frame's budget and usage, and the check.
//! - When revm built a frame: the start's verdict, then the same, which holds the new frame's
//!   budget and usage — a creation's measured from before its creator's record.
//! - When revm answered a start: the verdict the ledger captured while the start's level was open,
//!   or, for a call gas detention answers without running, the hypothetical one.
//! - When a frame returns: the history its caller gets back, by the ledger's formula, the check of
//!   the caller it returns into, then the totals; for the transaction's own frame, the formula's
//!   refund is kept for the settlement.
//! - When the transaction's own frame settles: the history its own record got back, at whichever
//!   site the lanes gave it back, then the totals.
//!
//! Between a start revm answered and that start's return the two differ by design: the ledger has
//! already taken back what a failed answer journaled, and the lanes take it back when the answer's
//! lane is popped. Nothing is compared there. A refund is compared only where the records the
//! lanes counted for a start are the ones its charge paid for: an inspector that rewrites a start's
//! input can split them, and then the lanes keep a charge the formula gives back.
//!
//! A tool's checkpoint that enclosed a frame start and is reverted takes back what the frames
//! inside it kept; the ledger follows it and the lanes cannot. From then on the transaction is not
//! compared ([`Ledger::diverged`]).

use super::{ledger::StartSide, AdditionalLimit, Ledger, LimitCheck, StartReport};

#[cfg(feature = "ledger-shadow")]
mod compare {
    use super::super::{AdditionalLimit, Ledger, LimitCheck, LimitUsage};

    /// What the lanes hold the transaction and the running frame to.
    #[derive(Debug)]
    pub(crate) struct LanesView {
        /// What the transaction keeps.
        pub(crate) usage: LimitUsage,
        /// The log and code bytes it keeps.
        pub(crate) log_and_code_bytes: u64,
        /// The frames on the call stack.
        pub(crate) frames: usize,
        /// The running frame's budget and what it keeps.
        pub(crate) running: Option<(u64, LimitUsage)>,
        /// The check.
        pub(crate) check: LimitCheck,
    }

    /// Panics with the point of the transaction and the quantity on which the two disagree.
    pub(super) fn equal<T: PartialEq + core::fmt::Debug>(
        lanes: &T,
        ledger: &T,
        at: &str,
        what: &str,
    ) {
        assert!(
            lanes == ledger,
            "ledger shadow mismatch at {at}: {what}: lanes {lanes:?}, ledger {ledger:?}"
        );
    }

    /// Holds what the two hold the transaction and the running frame to.
    pub(super) fn agree(lanes: &AdditionalLimit, ledger: &Ledger, at: &str) {
        if ledger.diverged() {
            return;
        }
        let view = lanes.shadow_view();
        equal(&view.usage, &ledger.usage(), at, "usage");
        equal(&view.log_and_code_bytes, &ledger.log_and_code_bytes(), at, "log and code bytes");
        equal(&view.frames, &ledger.frame_levels(), at, "frames");
        equal(&view.running, &ledger.running_budget(), at, "running frame's budget and usage");
        equal(&view.check, &ledger.check(), at, "check");
    }

    /// Holds a verdict the lanes reached (`None` where they held nothing) to the ledger's.
    pub(super) fn verdict(
        lanes: Option<LimitCheck>,
        ledger_verdict: Option<LimitCheck>,
        ledger: &Ledger,
        at: &str,
    ) {
        if !ledger.diverged() {
            equal(&lanes, &ledger_verdict, at, "verdict");
        }
    }

    /// The ledger's check, as a verdict: `None` when it holds.
    pub(super) fn crossing(ledger: &Ledger) -> Option<LimitCheck> {
        let check = ledger.check();
        check.exceeded_limit().then_some(check)
    }
}

#[cfg(feature = "ledger-shadow")]
pub(crate) use compare::LanesView;

/// A completed write, counted on both with the lanes' verdict and the ledger's.
#[inline(always)]
pub(crate) fn completed_write(
    lanes: &AdditionalLimit,
    ledger: &Ledger,
    lanes_verdict: LimitCheck,
    ledger_verdict: LimitCheck,
) {
    #[cfg(feature = "ledger-shadow")]
    {
        let at = "a completed write";
        compare::verdict(Some(lanes_verdict), Some(ledger_verdict), ledger, at);
        compare::agree(lanes, ledger, at);
    }
    #[cfg(not(feature = "ledger-shadow"))]
    let _ = (lanes, ledger, lanes_verdict, ledger_verdict);
}

/// A creation's deposited code, counted on both with the lanes' verdict and the ledger's.
#[inline(always)]
pub(crate) fn code_deposit(
    lanes: &AdditionalLimit,
    ledger: &Ledger,
    lanes_verdict: LimitCheck,
    ledger_verdict: LimitCheck,
) {
    #[cfg(feature = "ledger-shadow")]
    {
        let at = "a code deposit";
        compare::verdict(Some(lanes_verdict), Some(ledger_verdict), ledger, at);
        compare::agree(lanes, ledger, at);
    }
    #[cfg(not(feature = "ledger-shadow"))]
    let _ = (lanes, ledger, lanes_verdict, ledger_verdict);
}

/// An Oracle hint, admitted or refused by both with the lanes' verdict and the ledger's.
#[inline(always)]
pub(crate) fn hint(
    lanes: &AdditionalLimit,
    ledger: &Ledger,
    lanes_verdict: LimitCheck,
    ledger_verdict: LimitCheck,
) {
    #[cfg(feature = "ledger-shadow")]
    {
        let at = "an Oracle hint";
        compare::verdict(Some(lanes_verdict), Some(ledger_verdict), ledger, at);
        compare::agree(lanes, ledger, at);
    }
    #[cfg(not(feature = "ledger-shadow"))]
    let _ = (lanes, ledger, lanes_verdict, ledger_verdict);
}

/// A keyless deployment's take-back of its signer's nonce record: whether the lanes took one back
/// (`lanes_taken`) and the ledger did (`ledger_taken`), then the stop the lanes left the call.
#[inline(always)]
pub(crate) fn keyless_take_back(
    lanes: &AdditionalLimit,
    ledger: &Ledger,
    lanes_taken: bool,
    ledger_taken: bool,
) {
    #[cfg(feature = "ledger-shadow")]
    if !ledger.diverged() {
        let at = "a keyless take-back";
        compare::equal(&lanes_taken, &ledger_taken, at, "taken back");
        let pending = lanes.shadow_pending_stop();
        compare::verdict(pending, compare::crossing(ledger), ledger, at);
        compare::agree(lanes, ledger, at);
    }
    #[cfg(not(feature = "ledger-shadow"))]
    let _ = (lanes, ledger, lanes_taken, ledger_taken);
}

/// A start revm built, held by the lanes to `lanes_verdict` and seen by the ledger as `report`.
#[inline(always)]
pub(crate) fn built_start(
    lanes: &AdditionalLimit,
    ledger: &Ledger,
    lanes_verdict: LimitCheck,
    report: &StartReport,
) {
    #[cfg(feature = "ledger-shadow")]
    {
        let at = "a built start";
        compare::verdict(Some(lanes_verdict), report.verdict, ledger, at);
        compare::agree(lanes, ledger, at);
    }
    #[cfg(not(feature = "ledger-shadow"))]
    let _ = (lanes, ledger, lanes_verdict, report);
}

/// A start revm answered, or gas detention answered without revm, held by the lanes to
/// `lanes_verdict` (`None` where they held nothing) and seen by the ledger as `report`.
#[inline(always)]
pub(crate) fn answered_start(
    ledger: &Ledger,
    lanes_verdict: Option<LimitCheck>,
    report: &StartReport,
) {
    #[cfg(feature = "ledger-shadow")]
    compare::verdict(lanes_verdict, report.verdict, ledger, "an answered start");
    #[cfg(not(feature = "ledger-shadow"))]
    let _ = (ledger, lanes_verdict, report);
}

/// The return of the frame `side` stands for, whose caller the lanes gave back `refund`.
///
/// Below the transaction's own frame, the refund is held to the ledger's formula, where the records
/// the lanes counted are the ones the charge paid for, and the stop the lanes left the caller to
/// the ledger's check. The transaction's own frame has no caller: the formula's refund of its
/// record is kept for the settlement ([`settlement`]).
#[inline(always)]
pub(crate) fn frame_return(
    lanes: &mut AdditionalLimit,
    ledger: &Ledger,
    refund: u64,
    side: &StartSide,
) {
    #[cfg(feature = "ledger-shadow")]
    {
        let at = "a frame's return";
        if side.depth() == 0 {
            if side.comparable() {
                lanes.shadow_set_top_level_refund_by_formula(side.refund(ledger));
            }
        } else if !ledger.diverged() {
            if side.comparable() {
                compare::equal(&refund, &side.refund(ledger), at, "refund");
            }
            let pending = lanes.shadow_pending_stop();
            compare::verdict(pending, compare::crossing(ledger), ledger, "a caller's check");
        }
        compare::agree(lanes, ledger, at);
    }
    #[cfg(not(feature = "ledger-shadow"))]
    let _ = (lanes, ledger, refund, side);
}

/// The settlement of the transaction's own frame: the history its own record got back, at
/// whichever site the lanes gave it back, held to the ledger's formula, then the totals.
#[inline(always)]
pub(crate) fn settlement(lanes: &AdditionalLimit, ledger: &Ledger) {
    #[cfg(feature = "ledger-shadow")]
    {
        let at = "the transaction's settlement";
        let (by_lanes, by_formula) = lanes.shadow_top_level_refunds();
        if let (false, Some(by_formula)) = (ledger.diverged(), by_formula) {
            compare::equal(&by_lanes, &by_formula, at, "the own frame's record refund");
        }
        compare::agree(lanes, ledger, at);
    }
    #[cfg(not(feature = "ledger-shadow"))]
    let _ = (lanes, ledger);
}
