//! Sensitive data access tracking for the `MegaETH` EVM.
//!
//! Sensitive data means those data that may be modified by the system (e.g., sequencer, oracle, or
//! payload builder). Once a transaction accesses sensitive data, the system will immediate limit
//! the remaining gas in all message calls to a small amount of gas, forcing the transaction to
//! finish execution soon. These restrictions are necessary to prevent `DoS` attacks on EVM.

mod block;
mod tracker;

pub use block::*;
use revm::{handler::FrameResult, interpreter::Gas};
pub use tracker::*;

mod oracle;
pub use oracle::*;

use crate::constants;

/// Force limit the remaining gas to the sensitive data access remaining gas.
///
/// This semantic should only be used when `MegaSpecId::MINI_REX` is enabled.
pub fn force_limit_remaining_gas(gas: &mut Gas, tracker: &mut SensitiveDataAccessTracker) {
    let remaining = gas.remaining();
    if remaining > constants::mini_rex::SENSITIVE_DATA_ACCESS_REMAINING_GAS {
        // Calculate how much gas we're artificially consuming for enforcement
        let enforcement_gas = remaining - constants::mini_rex::SENSITIVE_DATA_ACCESS_REMAINING_GAS;
        tracker.record_enforcement_gas(enforcement_gas);

        let limit = gas.limit();
        let new_spent =
            limit.saturating_sub(constants::mini_rex::SENSITIVE_DATA_ACCESS_REMAINING_GAS);
        gas.set_spent(new_spent);
    }
}

/// Force limit the remaining gas on a frame result.
///
/// This semantic should only be used when `MegaSpecId::MINI_REX` is enabled.
pub fn force_limit_remaining_gas_in_frame_result(
    result: &mut FrameResult,
    tracker: &mut SensitiveDataAccessTracker,
) {
    match result {
        FrameResult::Call(call_outcome) => {
            let gas = &mut call_outcome.result.gas;
            force_limit_remaining_gas(gas, tracker);
        }
        FrameResult::Create(create_outcome) => {
            let gas = &mut create_outcome.result.gas;
            force_limit_remaining_gas(gas, tracker);
        }
    }
}
