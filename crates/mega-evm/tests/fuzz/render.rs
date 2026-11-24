//! A canonical rendering of what a transaction produced, so two runs are compared byte for byte
//! and a difference reads as a diff.

use std::{collections::BTreeMap, fmt::Write as _};

use alloy_primitives::{Address, U256};
use mega_evm::MegaTransactionOutcome;
use revm::state::{Account, EvmState};

use crate::gen::case::Execution;

/// The state sorted by address, each account's storage sorted by key, so two equal states render
/// the same whatever their maps' iteration order.
pub(crate) fn render_state(state: &EvmState) -> String {
    let mut out = String::new();
    let sorted: BTreeMap<Address, &Account> = state.iter().map(|(a, acc)| (*a, acc)).collect();
    for (address, account) in sorted {
        let _ = writeln!(
            out,
            "  {address}: nonce={} balance={} code_hash={} status={:?}",
            account.info.nonce, account.info.balance, account.info.code_hash, account.status
        );
        let slots: BTreeMap<U256, _> = account.storage.iter().map(|(k, v)| (*k, v)).collect();
        for (key, slot) in slots {
            let _ = writeln!(
                out,
                "    slot {key}: {} -> {}{}",
                slot.original_value,
                slot.present_value,
                if slot.is_cold { " (cold)" } else { "" }
            );
        }
    }
    out
}

/// Everything an outcome carries, rendered canonically: the result with its gas, logs and output,
/// the gas by ledger, the usage, the stop and the state.
pub(crate) fn render_outcome(outcome: &MegaTransactionOutcome) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "result: {:?}", outcome.result);
    let _ = writeln!(out, "gas: {:?}", outcome.gas);
    let _ = writeln!(out, "usage: {:?}", outcome.usage);
    let _ = writeln!(out, "limit_exceeded: {:?}", outcome.limit_exceeded);
    let _ = writeln!(out, "state:");
    out.push_str(&render_state(&outcome.state));
    out
}

/// An execution rendered canonically: the outcome, or the error.
pub(crate) fn render(execution: &Execution) -> String {
    match execution {
        Ok(outcome) => render_outcome(outcome),
        Err(error) => format!("refused: {error}\n"),
    }
}
