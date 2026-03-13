# Rex4 Specification

## Abstract

Rex4 is the fourth patch to the Rex hardfork.
It introduces per-frame resource budgets, relative gas detention, two system control contracts, and keyless deploy sandbox external-environment inheritance.
All Rex3 semantics are preserved unless explicitly changed below.

## Changes

### 1. Per-frame resource budgets for all resource dimensions

#### Motivation

Transaction-only enforcement lets inner calls consume nearly all remaining budget.
Rex4 adds bounded per-frame forwarding so parent and sibling execution remains predictable.

#### Semantics

Previous behavior:
- Data size, KV updates, compute gas, and state growth are effectively constrained only by transaction-level limits.

New behavior:
- The top-level frame MUST start with the full transaction budget for each dimension.
- Each inner frame MUST receive `remaining * 98 / 100` of its parent remaining budget for each dimension.
- A frame-local exceed MUST revert that frame with `MegaLimitExceeded(uint8 kind, uint64 limit)` and MUST NOT halt the transaction.
- The `kind` discriminator MUST identify which resource dimension exceeded the frame-local budget.
- A transaction-level exceed MUST halt with `OutOfGas`.
- Compute gas used by reverted child frames MUST still count toward transaction compute usage.

### 2. MegaAccessControl system contract

#### Motivation

Detention is reactive after volatile access.
Rex4 adds proactive subtree-level volatile-access control.

#### Semantics

Previous behavior:
- No runtime subtree switch exists to disable volatile access.

New behavior:
- The access-control interface MUST provide disable, enable, and status-query operations for volatile access.
- Disabling volatile access MUST apply to the caller frame and all descendants in its subtree.
- Restricted volatile access MUST revert immediately with `VolatileDataAccessDisabled(uint8 accessType)`.
- Covered volatile access MUST include block-environment reads, beneficiary-targeted account access (including `SELFDESTRUCT` when targeting the beneficiary), and oracle storage reads.
- Blocked volatile access MUST NOT update volatile-access tracking and MUST NOT tighten detention.
- Enabling from a descendant MUST revert with `DisabledByParent()` when the restriction is inherited from an ancestor.
- Restriction MUST end when the activating frame returns.
- Value-bearing control calls MUST revert with `NonZeroTransfer()`.

### 3. MegaLimitControl system contract

#### Motivation

Contracts need a deterministic runtime query for effective remaining compute gas under both frame and detention constraints.

#### Semantics

Previous behavior:
- No dedicated system query returns effective remaining compute gas.

New behavior:
- The limit-control interface MUST provide `remainingComputeGas() -> uint64`.
- Returned value in Rex4 MUST equal `min(frame_remaining, tx_detained_remaining)` at call time.
- Value-bearing calls to this query MUST revert with `NonZeroTransfer()`.

### 4. Relative gas detention cap

#### Motivation

Absolute caps can halt immediately when access occurs late.
Rex4 limits post-access compute budget without retroactively penalizing pre-access compute.

#### Semantics

Previous behavior:
- Effective detained limit is an absolute cap per volatile-access category.

New behavior:
- Effective detained limit MUST be `current_usage + cap` at volatile access time.
- Execution MUST be allowed up to `min(tx_compute_limit, effective_detained_limit)`.
- Across multiple volatile accesses, the most restrictive effective limit MUST apply.

### 5. Keyless deploy sandbox external-environment inheritance

#### Motivation

Fixed sandbox pricing and dropped hints can diverge from transaction environment semantics.
Rex4 aligns sandbox behavior with parent external context while preserving isolation.

#### Semantics

Previous behavior:
- Sandbox storage-related operations use fixed 1x pricing.
- Sandbox oracle hints are not forwarded.

New behavior:
- Sandbox execution MUST inherit parent external environment inputs for dynamic pricing and oracle behavior.
- Sandbox storage-related operations MUST follow parent-context dynamic pricing basis.
- Sandbox oracle hints MUST be forwarded to the parent context.
- Sandbox-internal caches MUST remain isolated from parent cache mutation.
- Pre-Rex4 specs MUST retain prior behavior.

## Invariants

- `I-1`: Stable pre-Rex4 semantics MUST remain unchanged.
- `I-2`: Frame-local exceed MUST revert the frame and MUST NOT halt the transaction.
- `I-3`: Transaction-level exceed MUST halt with `OutOfGas`.
- `I-4`: Transaction compute usage MUST be monotonic and include reverted-frame compute usage.
- `I-5`: Volatile-access disable scope MUST be subtree-local and ancestor-owned.
- `I-6`: Blocked volatile access MUST NOT update volatile-access tracking or detention.
- `I-7`: Transaction detained compute limit MUST be monotonic non-increasing.
- `I-8`: `remainingComputeGas()` MUST NOT exceed either frame-local remaining or transaction-level detained remaining.
- `I-9`: Keyless deploy sandbox MUST inherit parent external-environment semantics while preserving sandbox cache isolation.

## Inheritance

Rex4 inherits Rex3 except for the deltas defined in `Changes`.
Semantic lineage: `Rex4 -> Rex3 -> Rex2 -> Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague`.

## References

- [Rex3 Specification](Rex3.md)
- [Rex2 Specification](Rex2.md)
- [Rex1 Specification](Rex1.md)
- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
- [Rex4 Implementation References (Informative)](impl/Rex4-Implementation-References.md)
- [Rex4 Behavior Details (Informative)](impl/Rex4-Behavior-Details.md)
- [Resource Accounting](../docs/RESOURCE_ACCOUNTING.md)
