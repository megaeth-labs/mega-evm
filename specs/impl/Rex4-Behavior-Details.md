# Rex4 Behavior Details

This document is informative.
Normative semantics are defined in [Rex4 Specification](../Rex4.md).
If this document conflicts with the normative spec text, the normative spec wins.

## 1. Per-frame resource budgets

The forwarding ratio is 98/100 of the parent's remaining budget for child frames.
The same forwarding model is applied to data size, KV updates, compute gas, and state growth.
Frame-local limit exceed returns a revert outcome to the parent frame.
Transaction-level exceed remains a halt with `OutOfGas`.
Compute gas is still persistent across reverts, so reverted child compute usage is not discarded.
`MegaLimitExceeded(uint8 kind, uint64 limit)` carries the exceeded dimension and frame-local limit value.
The `kind` values are `0=data size`, `1=KV updates`, `2=compute gas`, and `3=state growth`.

## 2. MegaAccessControl details

The access-control system contract address is `0x6342000000000000000000000000000000000004`.
It exposes `disableVolatileDataAccess()`, `enableVolatileDataAccess()`, and `isVolatileDataAccessDisabled()`.
Disable state is depth-scoped and applies to the caller subtree.
A descendant cannot clear an ancestor-owned disable state and receives `DisabledByParent()`.
Disable state is released when the activating frame returns.
Blocked volatile access reverts with `VolatileDataAccessDisabled(uint8 accessType)`.
Blocked volatile access does not mark volatile-access tracking and does not tighten detained limits.
Covered volatile classes include block-environment reads, beneficiary-targeted account access, and oracle storage reads.
Beneficiary-targeted class includes `SELFDESTRUCT` when the target is the beneficiary.
Value-bearing calls to this control interface revert with `NonZeroTransfer()`.
Interception applies to `CALL` and `STATICCALL`.
`DELEGATECALL` and `CALLCODE` are not intercepted and fall through to on-chain bytecode behavior.

## 3. MegaLimitControl details

The limit-control system contract address is `0x6342000000000000000000000000000000000005`.
It exposes `remainingComputeGas() -> uint64`.
Returned value is the runtime minimum of caller frame remaining and transaction-level detained remaining.
The value is a snapshot at call time and can change as execution proceeds.
Value-bearing calls to this query revert with `NonZeroTransfer()`.
Interception applies to `CALL` and `STATICCALL`.
`DELEGATECALL` and `CALLCODE` are not intercepted and fall through to on-chain bytecode behavior.

## 4. Relative detention details

Rex4 computes effective detained limit as `usage_at_access + cap`.
Pre-Rex4 computes effective detained limit as absolute `cap`.
The final execution boundary still respects transaction compute-gas limit clamping.
The practical bound is `min(tx_compute_limit, effective_detained_limit)`.
Across multiple volatile accesses, the most restrictive effective limit remains binding.

## 5. Keyless deploy sandbox details

Rex4 keyless deploy sandbox shares parent external environment references for SALT and oracle behavior.
Rex4 keeps a sandbox-local dynamic gas cache to avoid parent cache pollution.
Rex4 forwards constructor oracle hints through shared oracle environment handling.
Pre-Rex4 keyless deploy sandbox continues using empty external environment behavior for compatibility.

## References

- [Rex4 Specification](../Rex4.md)
- [Rex4 Implementation References](Rex4-Implementation-References.md)
