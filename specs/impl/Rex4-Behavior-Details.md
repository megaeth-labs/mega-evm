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

### Examples

**Frame-local revert protects the parent's budget.**
Suppose a transaction has a state growth limit of 1000.
Contract A calls Contract B.
B receives a frame budget of 980 (1000 × 98/100) and attempts to create 981 new storage slots.
B's frame reverts with `MegaLimitExceeded(kind=3, limit=980)`, and all 981 slots are discarded.
A catches the revert, continues executing, and can still use the full remaining budget of 1000 to write its own storage.
In pre-Rex4 specs, B's writes would have succeeded (no per-frame enforcement), consuming most of the transaction budget and leaving A with almost nothing.

**Compute gas is the exception — it persists across reverts.**
Unlike data size, KV updates, and state growth, compute gas consumed by a reverted child frame is not discarded.
If B burns 1.9M compute gas and then reverts due to a per-frame limit exceed, A's transaction-level compute gas usage still increases by 1.9M.
Per-frame compute gas limits act as early termination guardrails, not as budget protection.

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

### Examples

**Proactive volatile-access control for parallel-friendly contracts.**
A DeFi contract wants to guarantee that none of its internal logic reads block environment fields, so that the sequencer can safely parallelize it with other transactions.
At the top of its entry point, the contract calls `disableVolatileDataAccess()`.
Any subcall in the entire call tree that reads `TIMESTAMP`, `COINBASE`, oracle storage, or accesses the beneficiary's account will immediately revert with `VolatileDataAccessDisabled(accessType)`.
Non-volatile operations (arithmetic, storage reads/writes, calls to other contracts) are unaffected.
When the entry-point frame returns, the restriction is automatically lifted — no explicit cleanup needed.

**Subtree ownership prevents descendants from weakening the restriction.**
If Contract A disables volatile access and then calls Contract B, B cannot call `enableVolatileDataAccess()` to override A's restriction.
B's enable attempt will revert with `DisabledByParent()`.
Only A's frame returning releases the restriction.
This ensures that a parent contract's isolation guarantee cannot be circumvented by untrusted callees.

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

### Examples

**Relative cap prevents late-access penalization.**
A contract performs 25M compute gas worth of pure arithmetic, then reads `TIMESTAMP` to check a deadline.
With the block-environment detention cap of 20M:
- Pre-Rex4 (absolute cap): The transaction halts immediately — 25M already consumed exceeds the 20M absolute cap.
- Rex4 (relative cap): The effective limit becomes 25M + 20M = 45M. The transaction can continue for up to 20M more gas after the access.

This means contracts no longer need to front-load volatile reads before any heavy computation.

**Multiple accesses — the earliest anchors the cap.**
A transaction reads `TIMESTAMP` at 5M usage, then reads `NUMBER` at 10M usage.
The first access sets an effective limit of 5M + 20M = 25M.
The second access computes 10M + 20M = 30M, but the most restrictive limit wins: 25M.
Developers should be aware that the first volatile access in a transaction determines the tightest bound.

## 5. Keyless deploy sandbox details

Rex4 keyless deploy sandbox shares parent external environment references for SALT and oracle behavior.
Rex4 keeps a sandbox-local dynamic gas cache to avoid parent cache pollution.
Rex4 forwards constructor oracle hints through shared oracle environment handling.
Pre-Rex4 keyless deploy sandbox continues using empty external environment behavior for compatibility.

## References

- [Rex4 Specification](../Rex4.md)
- [Rex4 Implementation References](Rex4-Implementation-References.md)
