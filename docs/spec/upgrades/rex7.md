---
description: Rex7 network upgrade — checkpoint-settled compute gas accounting with gas-clamp enforcement; plain opcodes record no compute gas between checkpoints, within-limit transactions stay bit-identical to Rex6, and limit-exceeding opcodes are stopped before they execute.
---

# Rex7 Network Upgrade

This page is an informative summary of the Rex7 specification.
For the full normative definition, see the Rex7 spec in the mega-evm repository.

{% hint style="warning" %}
**Unstable** — Rex7 is under active development.
Anything recorded on this page may change before Rex7 is frozen, and nothing here should be relied on.
{% endhint %}

## Summary

Rex7 changes how a node records and enforces [compute gas](../glossary.md#compute-gas) during execution.

Through [Rex6](rex6.md), every metered opcode records its own compute gas after it finishes, and a compute-limit exceed is evaluated at that opcode.
Rex7 replaces that per-opcode recording for ordinary opcodes with **checkpoint settlement**: plain opcodes run without a compute-gas recording step, and the node settles the compute gas of an entire segment when it reaches a checkpoint.

Rex7 also introduces **gas-clamp enforcement**: between checkpoints the node restricts the interpreter-visible remaining gas to the remaining compute headroom, so the inherited EVM's own per-opcode gas check stops a limit-crossing opcode before that opcode executes.

For a transaction that never crosses a compute-gas, detention, or other resource limit, Rex7 is bit-identical to Rex6: the same gas, the same receipt, the same state, and the same `GAS` opcode readings.
For a transaction that does cross a compute-gas or detention limit inside a plain-opcode segment, the halt lands before the crossing opcode rather than after it, the crossing opcode's cost is excluded from recorded compute usage, and remaining gas remains refundable under the same rescue rules as other transaction-level compute-limit halts.

One deliberate accounting carve-out remains: a frame that ends in an exceptional halt (including ordinary out-of-gas) settles its entire burned EVM-gas budget as compute gas at frame exit, so a transaction that contains an inner out-of-gas call can report higher compute usage under Rex7 than under Rex6 even though EVM gas and the receipt are unchanged.

## What Changed

### Checkpoint-Settled Compute Gas Accounting

#### Previous behavior

From [MiniRex](minirex.md) through [Rex6](rex6.md), a node records compute gas at every metered opcode:

- Each opcode belongs to a metering class defined in [Compute Gas Accounting](../evm/compute-gas.md).
- After the opcode body completes (or at the equivalent single measurement window for storage-affecting opcodes), the node records `(gas_before − gas_after)` less any storage-gas and forwarded-child exclusions, and evaluates the compute-gas limit.
- Plain opcodes (arithmetic, stack, memory, jumps, and similar) each open and close their own measurement window.
- A compute-gas or detention exceed is evaluated after the opcode that crossed the limit has finished, so that opcode's cost is included in recorded usage and the recorded total can land strictly above the limit.

Frame entry, frame resume, and frame exit do not themselves settle a multi-opcode segment; they only participate in per-frame budget push/pop and in the non-opcode recording sites listed in [Compute Gas Accounting](../evm/compute-gas.md#non-opcode-recording-sites).

#### New behavior

Under Rex7, a node MUST settle compute gas at **checkpoints** rather than after every plain opcode.

A **checkpoint** is any of the following:

1. A storage-gas opcode: `SSTORE`, `LOG0`–`LOG4`, `SELFDESTRUCT`.
2. A call-family opcode: `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`.
3. A create opcode: `CREATE`, `CREATE2`.
4. A volatile / detention-guarded opcode: the unconditional block-environment set (`BLOCKHASH`, `COINBASE`, `TIMESTAMP`, `NUMBER`, `DIFFICULTY` / `PREVRANDAO`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `BLOBHASH`), the beneficiary-conditional set (`BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`, `SELFBALANCE`), and oracle-conditional `SLOAD`.
5. The `GAS` opcode.
6. Frame entry, frame resume after a child returns, and frame exit.

Every other opcode is a **plain opcode** for settlement purposes.
A plain opcode MUST NOT open a compute-gas measurement window of its own and MUST NOT record compute gas when it finishes.

At each checkpoint a node MUST:

1. Settle the open plain-opcode segment as the interpreter-gas delta since the previous checkpoint (or since the frame opened / resumed), excluding storage gas and forwarded child gas that the checkpoint itself charges or forwards under the same exclusion rules as [Compute Gas Accounting](../evm/compute-gas.md).
2. Record that segment amount as compute gas and evaluate the compute-gas limit (and any latched non-compute resource-limit exceed) at that checkpoint.
3. Record the checkpoint opcode's own body compute gas under the same measurement-window rules that apply through Rex6 for that opcode class, then re-open the settlement window for the next segment.

Non-opcode recording sites (transaction intrinsic gas, precompiles, contract-creation code deposit, KeylessDeploy overhead and sandbox merge) are unchanged.

**Precision invariant.**
For every transaction that stays within every runtime resource limit, a node MUST produce the same recorded compute-gas total, the same four-dimension resource usage, the same receipt `gas_used`, the same execution result, and the same state under Rex7 as under Rex6.
The interpreter's gas counter already meters every opcode; settling by segment reproduces the per-opcode sum exactly when no limit is crossed.

**Exceptional-halt frame carve-out.**
A frame that ends in an exceptional halt — ordinary out-of-gas, memory out-of-gas, stack underflow or overflow, invalid jump, unknown opcode, and every other error result — returns none of its remaining budget.
The top-level frame's whole envelope is spent by the transaction's final gas accounting, and an inner frame's remainder is never handed back to its caller.
A node MUST settle that entire burned remainder as compute gas at frame exit: the open plain-opcode segment measured against a zero remainder, plus any gas the clamp was hiding from the interpreter.
The rule is driven by the halt classification, not by the interpreter's own counter — an inherited EVM zeroes that counter for ordinary out-of-gas only.

Under per-opcode recording through Rex6, neither the failing opcode nor the burn is attributed to compute gas.
Consequently, a transaction that halts exceptionally, or that contains an inner call frame which does, MAY report a **strictly higher** compute-gas total under Rex7 than under Rex6, while EVM gas accounting and the receipt remain identical.

A node MUST NOT evaluate any resource limit against the burned remainder.
The burn is bounded by the sender's gas envelope rather than by the compute limit, so it can carry the recorded total past that limit; halting on it would rescue gas the EVM has already burned and change a receipt this carve-out requires to stay identical.
The usage a limit is evaluated against therefore excludes the burn, while the transaction's reported compute-gas total and the block-level compute accounting include it.
Nothing is lost by that exclusion: the part of an exceptionally halted frame's tail that was actually executed is bounded either by the gas clamp or by a frame gas remainder that was already below the compute headroom, so it could not have exceeded a limit in the first place.

A clamp-induced out-of-gas is not an exceptional halt for this rule.
The crossing opcode was stopped before it executed and the remaining gas is rescued for the sender rather than burned, so the reclassification rules below apply instead.

### Gas-Clamp Enforcement

#### Previous behavior

Through Rex6, the compute-gas limit and the detained compute-gas limit are enforced when an opcode records its compute gas after it has finished.
The crossing opcode therefore executes fully, its cost is recorded, and recorded usage can land strictly above the limit (overshoot of one opcode).
Frame-local budget exceeds become frame reverts with `MegaLimitExceeded`; transaction-level and detention exceeds become transaction halts with remaining gas rescued for the sender.

#### New behavior

Under Rex7, a node MUST enforce compute-gas and detention limits inside plain-opcode segments by **clamping** the interpreter-visible remaining gas.

At each checkpoint, after settlement and after the checkpoint body has recorded its own compute gas (and after any detention cap the checkpoint installs), and again at frame entry and resume, a node MUST:

1. Compute the remaining compute headroom as the minimum of the current frame's remaining per-frame compute budget and the transaction-level remaining budget under the effective limit (including detention).
2. When the interpreter's true remaining gas is at or above that headroom, put the clamp in force for the segment that follows: hide the excess from the interpreter and remember which constraint bound the clamp — the frame-local budget or the transaction-level / detained limit — together with that constraint's own limit value. Equality is a binding clamp that hides nothing, not the absence of a clamp.
3. When the true remaining gas is below the headroom, no clamp is in force: the frame's own gas runs out ahead of the compute headroom, and an out-of-gas inside the segment is the inherited EVM's own rather than a resource-limit exceed.
4. Leave the true remaining gas available again before the next checkpoint body runs, before `GAS` is observed, before call-gas forwarding is computed, and before storage-gas charges are taken, so those sites always see the unclamped counter.

Inside a plain-opcode segment only plain opcodes run, so the inherited EVM's ordinary per-opcode gas check is the enforcement tool:

- When an opcode would cost more gas than the clamped visible remainder, the opcode MUST NOT execute.
- The frame's final result MUST restore the hidden gas into the gas counter.
- The node MUST reclassify that out-of-gas as the resource-limit exceed that the clamp stood for:
  - **Frame-local binding** → the frame reverts with `MegaLimitExceeded(uint8 kind, uint64 limit)`, and unspent gas returns to the parent through ordinary frame accounting.
  - **Transaction-level compute binding** → the transaction halts with `OutOfGas`, and remaining gas is rescued and refunded to the sender.
  - **Detained-limit binding** → the transaction halts with `VolatileDataAccessOutOfGas`, with the same gas rescue.

The reported `limit` MUST be the constraint that bound the clamp, not whichever limit is largest or most convenient: the frame's own compute budget for a frame-local binding, the effective transaction-level limit otherwise.
The revert payload is visible to the calling contract, so a frame-local exceed that reported the transaction-level limit would be a different observable return value for the same execution, not merely a different diagnostic.

Because the crossing opcode never executes, a node MUST NOT include its cost in recorded compute-gas usage.
Recorded usage at a clamp-induced halt therefore ends at the limit (or strictly below it if settlement had not yet closed a partial segment), not strictly above it.
The `actual` a transaction-level clamp halt reports MUST be that final usage — the frame-exit settlement closes the partial segment after the exceed is identified, and a node MUST NOT report the usage as it stood before that settlement.

**Top-frame headroom tie-break.**
At the top-level frame the remaining per-frame compute budget equals the transaction-level remaining budget whenever both are still governed by the same base limit.
When those two remaining amounts are equal, a node MUST bind the clamp to the transaction-level constraint (or to the detained limit when detention is the effective transaction-level bound).
A clamp-induced exceed under that binding MUST halt the transaction with gas rescue; a node MUST NOT classify the equality as frame-local.
Through Rex6, the same equality is classified by the per-opcode check as a frame-local exceed, which the top-level frame absorbs into a revert rather than a halt.

**Double-exceed preference.**
When the crossing opcode would have exhausted both the true remaining EVM gas and the compute headroom at the same point, a node MUST attribute the halt to the compute-gas (or detention) limit rather than to ordinary EVM out-of-gas, so remaining gas stays refundable under the rescue rules.
The two cases are indistinguishable once the frame has already reported out-of-gas, and the compute classification is the one that preserves the sender refund.

**Within-limit observability.**
For a transaction that never crosses a compute or detention limit, the clamp MUST be unobservable: `GAS` returns the true remaining gas, call forwarding and storage-gas charges see the true counter, and gas, receipt, and state match Rex6.

## Developer Impact

Rex7 is not scheduled on any network.
Its semantics may still change before it is frozen.

Contracts and tools that assume per-opcode compute-gas attribution for every instruction MUST treat that assumption as false under Rex7: only checkpoints settle compute gas during execution, and a plain-opcode segment has no intermediate recording.

Contracts that stay within every resource limit see no behavioral change relative to Rex6.
Contracts that trip the compute-gas or detention limit inside a plain-opcode segment halt one opcode earlier than under Rex6, with the crossing opcode excluded from recorded compute usage and with remaining gas still refundable on a transaction-level halt.

A transaction that halts exceptionally, or that calls into a child frame which does, may report a higher transaction-level compute-gas total under Rex7 than under Rex6 — for any exceptional halt, not just out-of-gas.
The receipt `gas_used`, the halt or revert reported, and the execution success or failure of the outer transaction are unchanged by that carve-out: the burned remainder is reported, never enforced.

## Safety and Compatibility

Rex7 changes nothing about how blocks under earlier specs are executed.
Every spec through Rex6 remains frozen: a node replaying historical blocks resolves each block's spec from its timestamp and applies that spec's semantics.

Because Rex7 is unstable, its semantics may change in either direction until it is frozen.
Any node, tool, or test fixture pinned to Rex7 must expect its results to move.
A deployment that needs stable semantics must select a frozen spec explicitly rather than relying on the latest one.

The gas clamp is strictly tighter than Rex6's post-opcode enforcement on the overshoot axis: the crossing opcode does not run, and recorded usage does not pass the limit by that opcode's cost.
The exceptional-halt frame carve-out is the only path on which Rex7 can report more compute gas than Rex6 for the same inputs; it over-reports rather than under-reports.

## References

- [Hardforks and Specs](../hardfork-spec.md) — how specs are versioned, frozen, and activated.
- [Rex6 Network Upgrade](rex6.md) — the frozen spec Rex7 inherits from.
- [Compute Gas Accounting](../evm/compute-gas.md) — measurement windows, metering classes, and exceed behavior (Rex7 details on that page).
- [Dual Gas Model](../evm/dual-gas-model.md) — total gas, storage gas, and metering order.
- [Gas Detention](../evm/gas-detention.md) — detained compute-gas caps.
- [Multidimensional Resource Limits](../evm/resource-limits.md) — transaction- and frame-level limit outcomes.
