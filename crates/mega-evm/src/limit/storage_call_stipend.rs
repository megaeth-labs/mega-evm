//! Tracks the `STORAGE_CALL_STIPEND` granted to value-transferring `CALL` / `CALLCODE`
//! frames. Two spec-gated models share this tracker.
//!
//! REX5+ uses a separated allowance: `call_inputs.gas_limit` is not inflated, and the
//! five `storage_gas_ext` charging sites (CALL/CALLCODE new-account, CREATE/CREATE2,
//! SSTORE first-time-write, LOG, SELFDESTRUCT) drain `try_consume` before charging the
//! residual against EVM `Gas`. `additional_limit_ext::{sstore, log}` are not consumers —
//! they check non-gas dimensions only.
//!
//! REX4 uses the legacy inflation: `call_inputs.gas_limit` is inflated by
//! `STORAGE_CALL_STIPEND`, the per-frame compute budget is capped at the pre-inflation
//! limit, and unused stipend is burned on return by clamping `gas.remaining()`. Preserved
//! byte-for-byte for replay parity.
//!
//! Pre-REX4 specs do not grant a stipend; both models collapse to a zero-allowance no-op.
//! Synthetic system-contract interception results that short-circuit `frame_init` push a
//! zero-allowance entry for stack alignment.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use revm::{
    handler::FrameResult,
    interpreter::{interpreter_action::FrameInit, CallScheme, FrameInput, Gas},
};

use super::compute_gas;
use crate::{constants, MegaSpecId};

/// Tracks per-frame `STORAGE_CALL_STIPEND` grants.
///
/// See the module-level doc for the dual-mode (REX5+ separated allowance vs REX4
/// legacy inflation) design and its rationale.
#[derive(Debug, Clone)]
pub(crate) struct StorageCallStipendTracker {
    /// Per-spec stipend grant amount. 0 for pre-REX4 (disabled).
    stipend_amount: u64,
    /// `true` for REX5+ (separated-allowance model); `false` for REX4 and pre-REX4
    /// (legacy inflation model, including the no-stipend case where
    /// `stipend_amount == 0`).
    rex5_enabled: bool,
    /// Per-frame stipend state. Pushed in `before_frame_init` / `push_empty_frame`,
    /// popped in `before_frame_return_result`.
    stack: Vec<StipendFrame>,
}

/// Per-frame stipend state.
///
/// `remaining` is the REX5+ separated-allowance balance. `legacy_inflated` is `true`
/// iff this frame was granted a REX4 `gas.limit()` inflation and must burn unused
/// stipend on return; `false` for REX5+ grants, empty frames, and pre-REX4.
#[derive(Debug, Clone, Copy, Default)]
struct StipendFrame {
    remaining: u64,
    legacy_inflated: bool,
}

impl StorageCallStipendTracker {
    pub(crate) fn new(spec: MegaSpecId) -> Self {
        Self {
            stipend_amount: Self::stipend_for_spec(spec),
            rex5_enabled: spec.is_enabled(MegaSpecId::REX5),
            stack: Vec::new(),
        }
    }

    /// Returns the stipend amount for the given spec. Zero for pre-REX4.
    fn stipend_for_spec(spec: MegaSpecId) -> u64 {
        if spec.is_enabled(MegaSpecId::REX4) {
            constants::rex4::STORAGE_CALL_STIPEND
        } else {
            0
        }
    }

    pub(crate) fn reset(&mut self) {
        self.stack.clear();
    }

    /// Pushes a zero-allowance entry for an empty frame (synthetic system-contract
    /// interception result or inspector early-return). Keeps the stack aligned with
    /// the EVM frame stack so subsequent pops match.
    pub(crate) fn push_empty_frame(&mut self) {
        self.stack.push(StipendFrame::default());
    }

    /// Drains up to `amount` from the current frame's allowance and returns the
    /// portion drained. The caller charges the residual via the original site's
    /// gas-charging macro (`gas!` or `gas_or_fail!`). Returns 0 pre-REX5; REX4 covers
    /// storage via `gas.limit()` inflation and does not draw from this allowance.
    pub(crate) fn try_consume(&mut self, amount: u64) -> u64 {
        if !self.rex5_enabled {
            return 0;
        }
        let Some(frame) = self.stack.last_mut() else {
            return 0;
        };
        let drained = frame.remaining.min(amount);
        frame.remaining -= drained;
        drained
    }

    /// Pushes a per-frame entry. REX5+ records the grant as an internal allowance.
    /// REX4 inflates `call_inputs.gas_limit`, caps the per-frame compute budget, and
    /// records the pre-inflation limit for the return-time burn clamp.
    ///
    /// Must be called **after** `compute_gas.before_frame_init()` so the compute frame
    /// exists for the REX4 cap-tightening.
    pub(crate) fn before_frame_init(
        &mut self,
        frame_init: &mut FrameInit,
        compute_gas: &mut compute_gas::ComputeGasTracker,
    ) {
        if !self.detect_grant(frame_init) {
            self.stack.push(StipendFrame::default());
            return;
        }

        if self.rex5_enabled {
            // REX5+ separated-allowance model: no gas_limit mutation, no compute cap.
            self.stack
                .push(StipendFrame { remaining: self.stipend_amount, legacy_inflated: false });
            return;
        }

        // REX4 legacy inflation model.
        let FrameInput::Call(call_inputs) = &mut frame_init.frame_input else {
            // `detect_grant` only returns true for Call frames, so this branch is
            // unreachable. Defend against future divergence.
            self.stack.push(StipendFrame::default());
            return;
        };
        let pre_inflation_limit = call_inputs.gas_limit;
        call_inputs.gas_limit = call_inputs.gas_limit.saturating_add(self.stipend_amount);
        compute_gas.cap_current_frame_limit(pre_inflation_limit);
        // REX4 stores the full grant in `remaining` so `current_frame_stipend()` can
        // surface it to `effective_remaining_for_rescue`. `try_consume` is a no-op on
        // REX4, so the value stays static.
        self.stack.push(StipendFrame { remaining: self.stipend_amount, legacy_inflated: true });
    }

    /// Pops the per-frame entry. REX4 also burns unused stipend by clamping
    /// `gas.remaining()` to `gas.limit().saturating_sub(stipend_amount)` so the caller
    /// never recovers more than the pre-stipend limit. Deriving the cap from
    /// post-inflation `gas.limit()` matches the saturating-add at inflation time.
    /// REX5 has nothing to burn (the allowance never entered `gas.limit()`).
    ///
    /// `LAST_FRAME` matches the other trackers' hook signature so the top-level
    /// duplicate `last_frame_result` call can safely no-op on an empty stack.
    pub(crate) fn before_frame_return_result<const LAST_FRAME: bool>(
        &mut self,
        result: &mut FrameResult,
    ) {
        assert!(LAST_FRAME || !self.stack.is_empty(), "frame stack is empty");
        let frame = self.stack.pop().unwrap_or_default();
        if self.rex5_enabled {
            // REX5+: nothing to burn — `gas.limit()` never held stipend.
            return;
        }
        if frame.legacy_inflated {
            let gas = result.gas_mut();
            // Derive the burn cap from post-inflation `gas.limit()` so the saturating
            // arithmetic mirrors the inflation site exactly.
            let original_limit = gas.limit().saturating_sub(self.stipend_amount);
            let burn = gas.remaining().saturating_sub(original_limit);
            if burn > 0 {
                let _ = gas.record_cost(burn);
            }
        }
    }

    /// Current frame's stipend balance. REX5: remaining (un-drained) allowance. REX4:
    /// the original grant amount (static; used by `effective_remaining_for_rescue` to
    /// exclude the inflated portion from the rescued-gas refund). Pre-REX4: 0.
    pub(crate) fn current_frame_stipend(&self) -> u64 {
        self.stack.last().map(|frame| frame.remaining).unwrap_or(0)
    }

    /// Portion of `gas.remaining()` to add to `rescued_gas` on a TX-level limit exceed.
    /// REX5 returns `gas.remaining()` directly (allowance never entered `gas.limit()`).
    /// REX4 excludes the current frame's stipend so system-granted gas is not refunded
    /// to the sender.
    pub(crate) fn effective_remaining_for_rescue(&self, gas: &Gas) -> u64 {
        if self.rex5_enabled {
            return gas.remaining();
        }
        let stipend = self.current_frame_stipend();
        if stipend > 0 {
            let original_limit = gas.limit().saturating_sub(stipend);
            gas.remaining().min(original_limit)
        } else {
            gas.remaining()
        }
    }

    /// Returns `true` if the given `frame_init` qualifies for a stipend grant.
    ///
    /// The stipend is granted to value-transferring internal `CALL`/`CALLCODE`
    /// frames (depth > 0). Top-level transaction frames, non-value-transfer
    /// CALL/CALLCODE, DELEGATECALL/STATICCALL, and CREATE/CREATE2 do not qualify.
    fn detect_grant(&self, frame_init: &FrameInit) -> bool {
        if self.stipend_amount == 0 {
            return false;
        }
        let FrameInput::Call(call_inputs) = &frame_init.frame_input else {
            return false;
        };
        let is_internal_call = frame_init.depth != 0;
        let is_value_transfer = call_inputs.transfers_value();
        let is_call_or_callcode =
            matches!(call_inputs.scheme, CallScheme::Call | CallScheme::CallCode);
        is_internal_call && is_value_transfer && is_call_or_callcode
    }
}
