# Rex4 Implementation References

This document is informative.
Normative semantics are defined in [Rex4 Specification](../Rex4.md).
If this mapping conflicts with the normative spec text, the normative spec wins.

## Scope

This document maps each Rex4 spec change and invariant to implementation and tests.
It is intended for code navigation, auditing, and regression test discovery.

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

Tests:
- [crates/mega-evm/tests/rex4/frame_limits.rs](../../crates/mega-evm/tests/rex4/frame_limits.rs)
- [crates/mega-evm/tests/rex4/frame_state_growth.rs](../../crates/mega-evm/tests/rex4/frame_state_growth.rs)

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

Tests:
- [crates/mega-evm/tests/rex4/access_control.rs](../../crates/mega-evm/tests/rex4/access_control.rs)

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

Tests:
- [crates/mega-evm/tests/rex4/limit_control.rs](../../crates/mega-evm/tests/rex4/limit_control.rs)

### 4. Relative gas detention cap

Spec clauses:
- Detained limit becomes `current_usage + cap` at volatile access time in Rex4.
- The most restrictive effective limit still applies across accesses.

Implementation:
- [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs)
- [crates/mega-evm/src/access/tracker.rs](../../crates/mega-evm/src/access/tracker.rs)
- [crates/mega-evm/src/limit/limit.rs](../../crates/mega-evm/src/limit/limit.rs)

Tests:
- [crates/mega-evm/tests/rex4/gas_detention.rs](../../crates/mega-evm/tests/rex4/gas_detention.rs)

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

Tests:
- [crates/mega-evm/tests/rex4/keyless_deploy.rs](../../crates/mega-evm/tests/rex4/keyless_deploy.rs)

## Invariant Mapping

- `I-1`: Legacy behavior compatibility.
  Coverage:
  [crates/mega-evm/tests/rex4/gas_detention.rs](../../crates/mega-evm/tests/rex4/gas_detention.rs),
  [crates/mega-evm/tests/rex3/oracle_gas_limit.rs](../../crates/mega-evm/tests/rex3/oracle_gas_limit.rs),
  [crates/mega-evm/tests/rex2/keyless_deploy.rs](../../crates/mega-evm/tests/rex2/keyless_deploy.rs).
- `I-2` and `I-3`: Frame-local revert versus transaction halt.
  Coverage:
  [crates/mega-evm/tests/rex4/frame_limits.rs](../../crates/mega-evm/tests/rex4/frame_limits.rs),
  [crates/mega-evm/tests/rex4/frame_state_growth.rs](../../crates/mega-evm/tests/rex4/frame_state_growth.rs).
- `I-4`: Monotonic transaction compute usage including reverted frames.
  Coverage:
  [crates/mega-evm/tests/rex4/frame_limits.rs](../../crates/mega-evm/tests/rex4/frame_limits.rs).
- `I-5` and `I-6`: Subtree-local volatile-disable and non-marking of blocked access.
  Coverage:
  [crates/mega-evm/tests/rex4/access_control.rs](../../crates/mega-evm/tests/rex4/access_control.rs).
- `I-7`: Monotonic non-increasing detained limit.
  Coverage:
  [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs),
  [crates/mega-evm/tests/rex4/gas_detention.rs](../../crates/mega-evm/tests/rex4/gas_detention.rs).
- `I-8`: `remainingComputeGas()` bounded by frame and detained remaining.
  Coverage:
  [crates/mega-evm/tests/rex4/limit_control.rs](../../crates/mega-evm/tests/rex4/limit_control.rs).
- `I-9`: Keyless deploy inheritance plus sandbox cache isolation.
  Coverage:
  [crates/mega-evm/src/sandbox/execution.rs](../../crates/mega-evm/src/sandbox/execution.rs),
  [crates/mega-evm/tests/rex4/keyless_deploy.rs](../../crates/mega-evm/tests/rex4/keyless_deploy.rs).

## Maintenance Notes

Update this mapping when Rex4 semantics change.
Update this mapping when implementation locations move.
Update this mapping when new tests are added or existing tests are renamed.
