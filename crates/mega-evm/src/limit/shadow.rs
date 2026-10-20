//! The shadow: the journal's ledger held to the lanes wherever the two must agree.
//!
//! The lanes ([`AdditionalLimit`]) take every decision; the ledger ([`Ledger`]) counts the same
//! writes beside them. Under the `ledger-shadow` feature the engine compares the two at the points
//! of a transaction's life where both describe the same thing, and panics on the first mismatch:
//!
//! - after a completed write is counted, a code deposit, an admitted hint, and the keyless
//!   take-back of a creator's record: what the transaction keeps, the history bytes beside its
//!   records, the check, and the running frame's budget and usage ([`agree`]);
//! - when revm built a frame: the start's verdict, and the new frame's budget and usage — the
//!   creation's measured from before its creator's record ([`agree`], [`verdict`]);
//! - when revm answered a start: the verdict the ledger captured while the start's level was open,
//!   or, for a call gas detention answers without running, the hypothetical one ([`verdict`]);
//! - when a frame returns: the history its caller gets back ([`refund`]), the check of the caller
//!   it returns into ([`verdict`]), and then [`agree`];
//! - when the transaction's own frame settles: what the transaction keeps, and the history its own
//!   frame's record gets back ([`refund`]).
//!
//! Between a start revm answered and that start's return the two differ by design: the ledger has
//! already taken back what a failed answer journaled, and the lanes take it back when the answer's
//! lane is popped. Nothing is compared there.
//!
//! A tool's checkpoint that enclosed a frame start and is reverted takes back what the frames
//! inside it kept; the ledger follows it and the lanes cannot. From then on the transaction is not
//! compared ([`Ledger::diverged`]).

use super::{ledger::StartSide, AdditionalLimit, Ledger, LimitCheck, LimitUsage};

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
macro_rules! shadow_eq {
    ($lanes:expr, $ledger:expr, $at:expr, $what:expr) => {{
        let (lanes, ledger) = (&$lanes, &$ledger);
        assert!(
            lanes == ledger,
            "ledger shadow mismatch at {}: {}: lanes {:?}, ledger {:?}",
            $at,
            $what,
            lanes,
            ledger,
        );
    }};
}

/// Holds what the two hold the transaction and the running frame to: what the transaction keeps,
/// the log and code bytes, the check, the number of frames and the running frame's budget and
/// usage.
pub(crate) fn agree(lanes: &AdditionalLimit, ledger: &Ledger, at: &str) {
    if ledger.diverged() {
        return;
    }
    let view = lanes.shadow_view();
    shadow_eq!(view.usage, ledger.usage(), at, "usage");
    shadow_eq!(view.log_and_code_bytes, ledger.log_and_code_bytes(), at, "log and code bytes");
    shadow_eq!(view.frames, ledger.frame_levels(), at, "frames");
    shadow_eq!(view.running, ledger.running_budget(), at, "running frame's budget and usage");
    shadow_eq!(view.check, ledger.check(), at, "check");
}

/// Holds a verdict the lanes reached (`None` where they held nothing) to the ledger's.
pub(crate) fn verdict(
    lanes: Option<LimitCheck>,
    ledger_verdict: Option<LimitCheck>,
    ledger: &Ledger,
    at: &str,
) {
    if ledger.diverged() {
        return;
    }
    shadow_eq!(lanes, ledger_verdict, at, "verdict");
}

/// Holds the history the lanes give a frame's caller back (`lanes`) to what `side`'s caller gets
/// back by the ledger's formula, where the records the lanes counted are those the charge paid for.
pub(crate) fn refund(lanes: u64, side: &StartSide, ledger: &Ledger, at: &str) {
    if ledger.diverged() || !side.comparable() {
        return;
    }
    shadow_eq!(lanes, side.refund(ledger), at, "refund");
}
