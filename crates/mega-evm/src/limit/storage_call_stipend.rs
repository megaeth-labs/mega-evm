//! Tracks the `STORAGE_CALL_STIPEND` granted to value-transferring `CALL`/`CALLCODE` frames
//! under REX4+.
//!
//! The tracker maintains a per-frame stack of stipend amounts, aligned with the EVM call stack.
//! On frame return the unused stipend is burned so that it cannot leak back to the caller.
//! Synthetic system-contract interception results that short-circuit `frame_init` do not pass
//! through this tracker; they only push an empty tracking frame for stack alignment.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use revm::{
    handler::FrameResult,
    interpreter::{interpreter_action::FrameInit, CallScheme, FrameInput},
};

use super::compute_gas;
use crate::{constants, MegaSpecId};

/// Tracks per-frame `STORAGE_CALL_STIPEND` grants.
///
/// Each child frame entry records the stipend amount that was added to its `gas_limit`.
/// On frame return, the unused portion is burned from the returned gas so that the caller
/// never recovers more than the original (pre-stipend) gas limit.
#[derive(Debug, Clone)]
pub(crate) struct StorageCallStipendTracker {
    rex4_enabled: bool,
    /// Per-frame stipend amounts.
    /// Pushed in `before_frame_init` / `push_empty_frame`, popped in `before_frame_return_result`.
    stack: Vec<u64>,
}

/// Metadata for a granted `STORAGE_CALL_STIPEND`, returned by `before_frame_init` and
/// consumed by `after_frame_init`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StorageCallStipendGrant {
    /// The extra gas added to the callee's `gas_limit`.
    stipend: u64,
    /// The original child gas limit before the stipend was added.
    compute_gas_cap: u64,
}

impl StorageCallStipendTracker {
    pub(crate) fn new(spec: MegaSpecId) -> Self {
        Self { rex4_enabled: spec.is_enabled(MegaSpecId::REX4), stack: Vec::new() }
    }

    pub(crate) fn reset(&mut self) {
        self.stack.clear();
    }

    /// Pushes a zero-stipend entry for an empty frame (system contract / inspector
    /// interception).
    pub(crate) fn push_empty_frame(&mut self) {
        self.stack.push(0);
    }

    /// Detects whether a `STORAGE_CALL_STIPEND` should be granted and inflates `gas_limit`
    /// in place.
    ///
    /// Must be called **before** the other trackers push their frames, because the inflated
    /// `gas_limit` affects subsequent frame initialization.
    /// Returns the grant metadata to be passed to `after_frame_init`.
    pub(crate) fn before_frame_init(
        &self,
        frame_init: &mut FrameInit,
    ) -> Option<StorageCallStipendGrant> {
        self.detect_and_apply(frame_init)
    }

    /// Pushes the stipend to the stack and caps the per-frame compute gas budget.
    ///
    /// Must be called **after** `compute_gas.before_frame_init()` so that the compute gas
    /// frame exists for `cap_current_frame_limit` to tighten.
    pub(crate) fn after_frame_init(
        &mut self,
        grant: Option<StorageCallStipendGrant>,
        compute_gas: &mut compute_gas::ComputeGasTracker,
    ) {
        if let Some(grant) = grant {
            self.stack.push(grant.stipend);
            compute_gas.cap_current_frame_limit(grant.compute_gas_cap);
        } else {
            self.stack.push(0);
        }
    }

    /// Burns the unused `STORAGE_CALL_STIPEND` from the returned gas.
    ///
    /// The child's `gas_limit` was inflated by `stipend`:
    ///
    ///   `original_limit = gas_limit - stipend`
    ///   `burn           = remaining - min(remaining, original_limit)`
    ///   `returned       = remaining - burn          (≤ original_limit)`
    ///
    /// The burn applies identically regardless of whether the child succeeded or reverted,
    /// because there is no discardable usage to roll back.
    ///
    /// `LAST_FRAME` matches the other trackers' hook signature so the top-level duplicate
    /// `last_frame_result` call can safely no-op on an empty stack.
    pub(crate) fn before_frame_return_result<const LAST_FRAME: bool>(
        &mut self,
        result: &mut FrameResult,
    ) {
        assert!(LAST_FRAME || !self.stack.is_empty(), "frame stack is empty");
        let stipend = self.stack.pop().unwrap_or(0);
        if stipend > 0 {
            let gas = result.gas_mut();
            let original_limit = gas.limit().saturating_sub(stipend);
            let burn = gas.remaining().saturating_sub(original_limit);
            if burn > 0 {
                // Infallible: burn ≤ remaining by construction.
                let _ = gas.record_cost(burn);
            }
        }
    }

    /// Detects whether a `STORAGE_CALL_STIPEND` should be granted to a `CALL`/`CALLCODE`
    /// with value transfer, and if so, inflates `gas_limit` in place.
    ///
    /// The stipend is only granted to child call frames (`depth > 0`), never to the
    /// top-level transaction frame.
    fn detect_and_apply(&self, frame_init: &mut FrameInit) -> Option<StorageCallStipendGrant> {
        if !self.rex4_enabled {
            return None;
        }
        let FrameInput::Call(call_inputs) = &mut frame_init.frame_input else {
            return None;
        };
        let is_internal_call = frame_init.depth != 0;
        let is_value_transfer = call_inputs.transfers_value();
        let is_call_or_callcode =
            matches!(call_inputs.scheme, CallScheme::Call | CallScheme::CallCode);
        if !(is_internal_call && is_value_transfer && is_call_or_callcode) {
            return None;
        }

        let stipend = constants::rex4::STORAGE_CALL_STIPEND;
        let compute_gas_cap = call_inputs.gas_limit;
        call_inputs.gas_limit = call_inputs.gas_limit.saturating_add(stipend);
        Some(StorageCallStipendGrant { stipend, compute_gas_cap })
    }
}
