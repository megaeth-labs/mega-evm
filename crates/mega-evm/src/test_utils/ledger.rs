//! The start ledger's settlements, for a test that reads the frame budgets and start charges the
//! engine settled.
//!
//! The engine keeps a frame's budgets, its start charge and its history allowance on its own start
//! ledger, not on the frame's gas: a test reads them as the ledger settled each start, once the
//! transaction ran. An inspector's answer and a result whose gas an inspector replaced settle as
//! unlimited budgets whatever gas the inspector built, and a tracker an inspector
//! restores from a snapshot leaves the ledger's draws where they were, which is what a test of
//! such an inspector asserts.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use crate::{ExternalEnvTypes, MegaContext};
use revm::Database;

pub use crate::limit::{LedgerPools, LedgerSettlement, PoolAmounts, PoolId};

/// Has `ctx`'s start ledger record every settlement from now on, across transactions, until they
/// are taken.
pub fn record_start_ledger_settlements<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
) {
    ctx.additional_limit.ledger_mut().record_settlements();
}

/// The settlements `ctx`'s start ledger recorded since they were last taken, in the order it made
/// them, innermost first within a transaction; none when it was not told to record them.
pub fn take_start_ledger_settlements<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
) -> Vec<LedgerSettlement> {
    ctx.additional_limit.ledger_mut().take_settlements().unwrap_or_default()
}
