//! The differences a debug build's start ledger finds between its figures and the fork's, for a
//! test that runs an inspector shape whose figures are accepted to change.
//!
//! The start ledger panics on any difference. An inspector that restores a snapshot of a running
//! tracker, replaces an interpreter, answers or rewrites with pools, an allowance or a start charge
//! it set, copies a tracker or an outcome from another start, or builds an input itself, moves the
//! fork's figures where the ledger, which keeps them off the tracker, does not follow: a test that
//! runs one tolerates the differences, then asserts each of them.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use crate::{ExternalEnvTypes, MegaContext};
use revm::Database;

pub use crate::limit::{LedgerDivergence, LedgerFigure};

/// Has `ctx`'s start ledger record every difference from now on rather than panic on it, and take
/// the fork's pools where a caller's differ.
pub fn tolerate_start_ledger_divergences<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
) {
    ctx.additional_limit.ledger_mut().tolerate_divergences();
}

/// The differences `ctx`'s start ledger recorded since they were last taken, in the order it found
/// them; none when it was not told to tolerate them.
pub fn take_start_ledger_divergences<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
) -> Vec<LedgerDivergence> {
    ctx.additional_limit.ledger_mut().take_divergences().unwrap_or_default()
}
