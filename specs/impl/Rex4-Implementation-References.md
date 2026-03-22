# Rex4 Implementation References

This document is informative.
Normative semantics are defined in [Rex4 Specification](../Rex4.md).
If this mapping conflicts with the normative spec text, the normative spec wins.

## Scope

This document maps each Rex4 spec change and invariant to implementation.
It is intended for code navigation and auditing.

## Change Mapping

### 1. Per-frame resource budgets for all resource dimensions

Spec clauses:
- Child frame budget forwarding ratio is 98/100 from parent remaining.
- Frame-local exceed reverts with `MegaLimitExceeded(uint8 kind, uint64 limit)`.
- Transaction-level exceed halts with `OutOfGas`.
- Reverted child compute gas still contributes to transaction compute usage.

Implementation:
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs)
- [crates/mega-evm/src/limit/frame_limit.rs](../../crates/mega-evm/src/limit/frame_limit.rs)
- [crates/mega-evm/src/limit/data_size.rs](../../crates/mega-evm/src/limit/data_size.rs)
- [crates/mega-evm/src/limit/kv_update.rs](../../crates/mega-evm/src/limit/kv_update.rs)
- [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs)
- [crates/mega-evm/src/limit/state_growth.rs](../../crates/mega-evm/src/limit/state_growth.rs)
- [crates/mega-evm/src/limit/limit.rs](../../crates/mega-evm/src/limit/limit.rs)
- [crates/mega-evm/src/limit/mod.rs](../../crates/mega-evm/src/limit/mod.rs)

### 2. MegaAccessControl system contract

Spec clauses:
- `disableVolatileDataAccess()` applies to caller subtree.
- Restricted volatile access reverts with `VolatileDataAccessDisabled(uint8 accessType)`.
- `enableVolatileDataAccess()` cannot override ancestor-owned restriction and reverts with `DisabledByParent()`.
- Blocked volatile access does not update volatile tracking and does not tighten detention.
- Non-zero value transfer reverts with `NonZeroTransfer()`.
- `CALL` and `STATICCALL` are intercepted.
- `DELEGATECALL` and `CALLCODE` are not intercepted.

Implementation:
- [crates/mega-evm/src/system/control.rs](../../crates/mega-evm/src/system/control.rs)
- [crates/mega-evm/src/system/intercept.rs](../../crates/mega-evm/src/system/intercept.rs)
- [crates/mega-evm/src/access/tracker.rs](../../crates/mega-evm/src/access/tracker.rs)
- [crates/mega-evm/src/evm/host.rs](../../crates/mega-evm/src/evm/host.rs)
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs)
- [crates/system-contracts/contracts/MegaAccessControl.sol](../../crates/system-contracts/contracts/MegaAccessControl.sol)

### 3. MegaLimitControl system contract

Spec clauses:
- `remainingComputeGas() -> uint64` is available at the limit-control system address.
- Returned value is `min(frame_remaining, tx_detained_remaining)` at call time.
- Non-zero value transfer reverts with `NonZeroTransfer()`.
- `CALL` and `STATICCALL` are intercepted.
- `DELEGATECALL` and `CALLCODE` are not intercepted.

Implementation:
- [crates/mega-evm/src/system/limit_control.rs](../../crates/mega-evm/src/system/limit_control.rs)
- [crates/mega-evm/src/system/intercept.rs](../../crates/mega-evm/src/system/intercept.rs)
- [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs)
- [crates/system-contracts/contracts/MegaLimitControl.sol](../../crates/system-contracts/contracts/MegaLimitControl.sol)

### 4. Relative gas detention cap

Spec clauses:
- Detained limit becomes `current_usage + cap` at volatile access time in Rex4.
- The most restrictive effective limit still applies across accesses.

Implementation:
- [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs)
- [crates/mega-evm/src/access/tracker.rs](../../crates/mega-evm/src/access/tracker.rs)
- [crates/mega-evm/src/limit/limit.rs](../../crates/mega-evm/src/limit/limit.rs)

### 5. Keyless deploy sandbox external-environment inheritance

Spec clauses:
- Sandbox inherits parent external environment inputs for dynamic pricing and oracle behavior.
- Sandbox forwards oracle hints to parent context.
- Sandbox keeps local cache isolation.
- Pre-Rex4 behavior is preserved.

Implementation:
- [crates/mega-evm/src/sandbox/execution.rs](../../crates/mega-evm/src/sandbox/execution.rs)
- [crates/mega-evm/src/sandbox/state.rs](../../crates/mega-evm/src/sandbox/state.rs)
- [crates/mega-evm/src/external/gas.rs](../../crates/mega-evm/src/external/gas.rs)
- [crates/mega-evm/src/external/oracle.rs](../../crates/mega-evm/src/external/oracle.rs)

## Invariant Mapping

- `I-1`: Stable pre-Rex4 semantics unchanged.
  Implementation: spec-gated branching throughout the codebase.
- `I-2` and `I-3`: Frame-local exceed reverts the frame; transaction-level exceed halts.
  Implementation: [crates/mega-evm/src/limit/limit.rs](../../crates/mega-evm/src/limit/limit.rs), per-dimension trackers.
- `I-4`: Transaction compute usage is monotonic and includes reverted-frame compute usage.
  Implementation: [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs) (persistent usage model).
- `I-5` and `I-6`: Volatile-access disable scope is subtree-local and ancestor-owned; blocked access does not update tracking.
  Implementation: [crates/mega-evm/src/system/control.rs](../../crates/mega-evm/src/system/control.rs), [crates/mega-evm/src/access/tracker.rs](../../crates/mega-evm/src/access/tracker.rs).
- `I-7`: Transaction detained compute limit is monotonic non-increasing.
  Implementation: [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs).
- `I-8`: `remainingComputeGas()` does not exceed either frame-local remaining or transaction-level detained remaining.
  Implementation: [crates/mega-evm/src/system/limit_control.rs](../../crates/mega-evm/src/system/limit_control.rs).
- `I-9`: Keyless deploy sandbox inherits parent external-environment semantics while preserving sandbox cache isolation.
  Implementation: [crates/mega-evm/src/sandbox/execution.rs](../../crates/mega-evm/src/sandbox/execution.rs).

## Maintenance Notes

Update this mapping when Rex4 semantics change.
Update this mapping when implementation locations move.
