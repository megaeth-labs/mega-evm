---
description: Rex7 network upgrade — checkpoint-settled compute gas accounting with gas-clamp enforcement; plain opcodes record no compute gas between checkpoints, within-limit transactions that never end a frame in an exceptional halt and never trip a disableVolatileDataAccess guard stay bit-identical to Rex6, a disabled-volatile rejection charges the opcode's static fee, and a detention mark is produced when the target account is loaded.
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

For a transaction that never crosses a compute-gas, detention, or other resource limit, in which no frame ends in an exceptional halt, and in which no `disableVolatileDataAccess` guard rejects an opcode, Rex7 is bit-identical to Rex6: the same gas, the same receipt, the same state, and the same `GAS` opcode readings.
For a transaction that does cross a compute-gas or detention limit inside a plain-opcode segment, the halt lands before the crossing opcode rather than after it, the crossing opcode's cost is excluded from recorded compute usage, and remaining gas remains refundable under the same rescue rules as other transaction-level compute-limit halts.

Rex7 also makes two guard- and detention-related choices that Rex6 does not:

- A `disableVolatileDataAccess` rejection still charges the rejected opcode's static fee.
- A detention mark is produced when the target account is loaded, so a frame that cannot afford the fees charged before that load produces no mark.

Two deliberate accounting carve-outs remain.
A frame that ends in an exceptional halt (including ordinary out-of-gas) settles its entire EVM-gas budget as compute gas, so a transaction that contains an inner out-of-gas call can report higher compute usage under Rex7 than under Rex6 even though EVM gas and the receipt are unchanged.
That budget is split — the work the frame performed enforces like any other work, while the remainder it destroyed is reported but never enforced.
The reported destroyed total is derived from a conservation law over what the transaction spent rather than summed from the sites that destroyed it, so an envelope lost anywhere lands in it whether or not a site was written to book it.
A precompile that fails is split the same way at its recording site: executed work (the KZG fixed fee when the call reached verification; zero when the input was rejected before any work, KZG's own 192-byte length check included) enforces, and the unused caller-supplied envelope is destroyed.
The generic error arm therefore stops enforcing the whole forwarded amount, which is an intentional enforcement difference from Rex6; the Rex5 forwarded-gas cap still prevents the precompile from performing more work than the remaining compute budget.

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

Non-opcode recording sites (transaction intrinsic gas, successful or reverting precompiles, contract-creation code deposit, KeylessDeploy overhead and sandbox merge) are unchanged.
A precompile that **fails** is the exception below.

**Precision invariant.**
For every transaction that stays within every runtime resource limit, in which no frame ends in an exceptional halt, and in which no `disableVolatileDataAccess` guard rejects an opcode, a node MUST produce the same recorded compute-gas total, the same four-dimension resource usage, the same receipt `gas_used`, the same execution result, and the same state under Rex7 as under Rex6.
The interpreter's gas counter already meters every opcode; settling by segment reproduces the per-opcode sum exactly when no limit is crossed, no frame ends in an exceptional halt, and no rejected guard charges a static fee.

**Exceptional-halt frame carve-out.**
A frame that ends in an exceptional halt — ordinary out-of-gas, memory out-of-gas, stack underflow or overflow, invalid jump, unknown opcode, and every other error result — returns none of its remaining budget.
The top-level frame's whole envelope is spent by the transaction's final gas accounting, and an inner frame's remainder is never handed back to its caller.
A node MUST settle that whole budget as compute gas, in two parts that are accounted differently.

The **executed** part is the open plain-opcode segment, measured as the interpreter-gas delta since the previous checkpoint, less any storage gas a checkpoint body charged before aborting.
A node MUST record it through the ordinary path, so it counts toward the transaction's reported total **and** toward the usage every resource limit is evaluated against — exactly as the same opcodes would if the frame had returned normally.
A parent frame keeps executing after it absorbs a failed child; excluding the child's work from enforcement would let the code that follows spend the same compute headroom a second time.

The **destroyed** part is the budget the frame never spent and never handed back.
A node MUST record it in the transaction's reported compute-gas total and in block-level compute accounting, and MUST NOT evaluate any resource limit against it — at transaction level or at block level, where a destroyed remainder that counted toward admission would close the block's compute capacity for the transactions behind it (see [Resource Limits](../evm/resource-limits.md)).
It is bounded by the sender's gas envelope rather than by the compute limit, so it can carry the reported total past that limit; halting on it would rescue gas the EVM has already destroyed and change a receipt this carve-out requires to stay identical.

**The destroyed total is derived, not summed.**
Rex7 does not define a transaction's destroyed compute gas as the sum of what its halted frames, precompiles and system-contract invocations booked. It defines it as a conservation law over the gas the transaction spent:

`destroyed = spent + minted_stipends − storage_gas − executed_compute`

`spent` is the EVM gas the envelope burnt, read once at the moment the envelope is final — after the transaction's gas accounting has settled and any resource-limit rescue has been returned to the sender, and before the EIP-3529 refund and the EIP-7623 floor are applied, since those move the receipt's number without anything having been burnt.
`minted_stipends` is the sum of `CALL_STIPEND` over the value-transferring `CALL` and `CALLCODE` invocations whose child frame ran: the inherited EVM grants that stipend to the child's frame budget without debiting the caller, so the frames record one stipend more work than the envelope funded per such call, and a node MUST add it back.
`storage_gas` is the MegaETH storage gas the transaction was charged, taken as a signed difference at a nested-execution boundary — negative when the nested execution's own EIP-3529 refund outgrew its storage gas — which a node MUST NOT clamp at zero.
`executed_compute` is the recorded compute total less the destroyed part: the work every resource limit is evaluated against.
A node MUST NOT report a negative result; the law cannot produce one on this spec, and a node that computes one MUST report zero rather than a wrapped value.

Enforcement is unaffected: the work a limit is evaluated against still comes from the checkpoint and per-opcode recordings, and only the reported destroyed total — and the block's enforced counter, which subtracts it — comes from the law.

The split MUST be driven by the halt classification, not by the interpreter's own counter — an inherited EVM zeroes that counter for ordinary out-of-gas only.
That zeroing has one consequence a node MUST accept rather than work around: for an ordinary out-of-gas taken with no clamp in force, the counter is already zero at frame exit, so the whole segment measures as executed and is enforced in full.
This is the one shape where Rex7 enforcement is stricter than Rex6's, which attributes the failing opcode to neither part.

A node MUST take the split from the frame's **final** result, after the create-return processing that can still turn a successful constructor into a canonical code-deposit out-of-gas, an EIP-3541 reject or a runtime code-size reject — each of which destroys the frame's remainder just as a halt from the interpreter loop does.
When a nested execution merges its usage into an outer one, which today is only the `KeylessDeploy` sandbox boundary, a node MUST carry the split across that boundary: the outer transaction reports the inner total in full and enforces only its executed part.

A precompile invocation that fails is the same split, taken at the precompile recording site rather than at interpreter-frame exit — a precompile never becomes a child EVM frame, so the frame-exit settlement cannot see it.
The **executed** part is the work the precompile performed: the KZG point-evaluation fixed cost when that precompile reached verification and returned a non-out-of-gas error, and zero when the invocation was rejected before any work (malformed input, or a wrapper out-of-gas that never reached verification).
For KZG the boundary is its own input-length check, which runs before the commitment is read: an input whose length is not 192 bytes is a rejection before any work, while every other non-out-of-gas failure is raised once verification is under way and is priced at the whole fixed cost, however far it got.
A node MUST price an unrecognised non-out-of-gas KZG failure as verification under way, so that an unfamiliar failure can only over-charge.
A node MUST record the executed part through the ordinary enforcing path.
The **destroyed** part is the rest of the call's gas limit — the caller-supplied envelope, not the Rex5-capped effective gas limit.
On a value-transferring call that envelope includes the protocol-granted call stipend, so it can exceed what the parent itself funded.
When the cap binds, the gap between the envelope and the effective limit is destroyed budget rather than work, and a node MUST include it in the destroyed part.
Through Rex6 the generic error arm recorded the effective gas limit as enforcing usage; under Rex7 that arm enforces nothing.
That is a deliberate enforcement difference.
The Rex5 forwarded-gas cap is unchanged: a precompile still MUST NOT perform more work than the remaining compute budget.

A system contract invocation a node answers without opening an EVM frame — the `KeylessDeploy` dispatch is the only one today — takes the same split at the site that produces the answer, and only when that answer is a halt which keeps the call's gas: whatever the invocation performed before failing is executed, and the rest of the call's gas limit is destroyed.
An answer that returns or reverts gives the gas back to the caller, and a halt whose remaining gas is rescued for the sender is a refund.
A node MUST NOT record either as destroyed; that gas was not lost, and counting it would report it twice.

Those sites are where a Rex7 transaction is known to lose an envelope without executing it, and they are what fixes `executed_compute` at each one — but they are not what makes the enumeration complete.
Completeness is a consequence of the law: a lost envelope is gas the transaction spent that neither the compute lanes nor the storage-gas lane accounts for, so it lands in the remainder whether or not a site above anticipated it.
Reading the two independently and requiring them to agree is what turns the list from an assumption into a checkable claim.

One further shape burns a whole envelope having executed nothing — a transaction whose intrinsic gas requirement outgrows the gas limit its sender supplied — but [Rex5](rex5.md) already rejects that transaction during validation, after every MegaETH storage-gas contribution has been folded into the intrinsic total and before the sender is debited.
It therefore produces no receipt on Rex7 and there is no envelope to split; a node MUST NOT record a rejected transaction's gas limit as a destroyed remainder.

Under per-opcode recording through Rex6, neither the failing opcode nor the destroyed remainder is attributed to compute gas.
Consequently, a transaction that halts exceptionally, or that contains an inner call frame which does, MAY report a **strictly higher** compute-gas total under Rex7 than under Rex6, while EVM gas accounting and the receipt remain identical.

A clamp-induced out-of-gas is not an exceptional halt for this rule.
The crossing opcode was stopped before it executed and the remaining gas is rescued for the sender rather than destroyed, so the reclassification rules below apply instead.
A frame whose exit latches a resource-limit exceed destroys nothing either: it reverts to its parent, or halts the transaction with its gas rescued.

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

A checkpoint that still carries a non-zero static fee — `GAS` and `LOG0` through `LOG4` — MAY itself be the crossing opcode of the preceding plain-opcode segment.
When the clamped visible remainder is less than that fee, the inherited per-opcode check stops the opcode before the body runs, and a node MUST treat that stop as a plain-segment crossing.
The CALL family is the same stop: its static fee is charged before the body, so a clamped remainder below that fee stops the opcode before the target account is read.
`CREATE` and `CREATE2` charge their inherited creation fee inside the body, after the true remaining gas has been restored, so a compute headroom below that fee MUST NOT stop them before the body.

The usage the clamp **enforces** therefore ends at or below the limit, not strictly above it.
The `actual` a transaction-level clamp halt reports MUST be the transaction's final reported compute usage — the frame-exit settlement closes the partial segment after the exceed is identified, and a node MUST NOT report the usage as it stood before that settlement.
Reported usage is not the same quantity as enforced usage: it also carries the destroyed remainders of any frame that halted exceptionally earlier in the transaction, which are reported and never enforced.
A node MUST NOT assume `actual` is at most `limit`.

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

### Charge-on-Reject for Disabled Volatile Access

#### Previous behavior

Through [Rex6](rex6.md), a node that has `disableVolatileDataAccess` active rejects a volatile-guarded opcode with a revert and the `VolatileDataAccessDisabled` payload, and charges the rejected opcode nothing.
The static gas table zeroes those opcodes so a frame holding less than the static fee still reaches the guard, and the handler charges the entry only after the check declines.
The reverting frame returns every unit of gas it held when it reached the opcode.

#### New behavior

Under Rex7, a node MUST still reject the same opcodes with the same revert payload, and MUST still leave the tracker unmarked.
A node MUST charge the rejected opcode's static fee before producing that revert.
The fee is ordinary EVM gas: it is debited from the frame, it is not refunded by the synthetic revert, and it MUST be recorded as compute gas when the open segment is settled at frame exit.
A guard that does not reject MUST charge the opcode's static fee at the same position the body charged it before this rule.
For the CALL family that position remains before the target account is read.
A frame that cannot afford the static fee MUST halt out of gas instead of reaching the disable revert.

The guarded set is unchanged from Rex6: the unconditional block-environment opcodes, the beneficiary-conditional account reads, `SELFBALANCE`, oracle-conditional `SLOAD`, the CALL family, and `SELFDESTRUCT`.

### Detention Mark at Account Load

#### Previous behavior

Through Rex6, a node documents that `BALANCE`, `EXTCODESIZE`, `EXTCODEHASH`, `SLOAD`, and `SELFDESTRUCT` register a volatile access even when the frame then runs out of gas on that opcode's own cost.
The CALL family and `EXTCODECOPY` are excluded from that guarantee because their implementation charges the base access cost — and, for a value-transferring call, the transfer cost — or the copy cost before the target account is read.
That exclusion is a frozen replay window, not a Rex6 rule: historical executions under the previous interpreter loaded first and marked first.

#### New behavior

Under Rex7, a node MUST produce a beneficiary or oracle detention mark when the target account (or oracle slot) is loaded, and MUST NOT produce that mark from a frame that cannot afford the fees charged before the load.
A CALL-family opcode whose static fee or value-transfer fee exhausts the frame, and an `EXTCODECOPY` whose copy cost exhausts the frame, therefore halt without marking, and the rest of the transaction runs undetained unless some other access has already marked.
This is specified behavior, not a replay exception.

## Developer Impact

Rex7 is not scheduled on any network.
Its semantics may still change before it is frozen.

Contracts and tools that assume per-opcode compute-gas attribution for every instruction MUST treat that assumption as false under Rex7: only checkpoints settle compute gas during execution, and a plain-opcode segment has no intermediate recording.

Contracts that stay within every resource limit, never end a frame in an exceptional halt, and never trip a `disableVolatileDataAccess` guard see no behavioral change relative to Rex6.
Contracts that trip the compute-gas or detention limit inside a plain-opcode segment halt one opcode earlier than under Rex6, with the crossing opcode excluded from recorded compute usage and with remaining gas still refundable on a transaction-level halt.
A contract that disables volatile access and then hits a guarded opcode pays that opcode's static fee under Rex7 and gets the same revert payload; through Rex6 that reject cost nothing.
A CALL or `EXTCODECOPY` that cannot afford the fees charged before the target account is loaded does not detain the rest of the transaction.

A transaction that halts exceptionally, or that calls into a child frame which does, may report a higher transaction-level compute-gas total under Rex7 than under Rex6 — for any exceptional halt, not just out-of-gas.
The receipt `gas_used`, the halt or revert reported, and the execution success or failure of the outer transaction are unchanged by the destroyed half of that carve-out: it is reported, never enforced.
The executed half does enforce, so a contract that calls into a failing child and keeps working can trip a resource limit at the same point it would under Rex6 — and, for a child that ran out of gas with no clamp in force, marginally earlier.
A contract that calls a precompile which then fails is on the same split: work the precompile performed still binds the remaining compute budget; the unused forwarded envelope does not.
Under Rex6 that unused envelope was enforcing, so the same tail work can survive under Rex7 and starve under Rex6.

## Safety and Compatibility

Rex7 changes nothing about how blocks under earlier specs are executed.
Every spec through Rex6 remains frozen: a node replaying historical blocks resolves each block's spec from its timestamp and applies that spec's semantics.

Because Rex7 is unstable, its semantics may change in either direction until it is frozen.
Any node, tool, or test fixture pinned to Rex7 must expect its results to move.
A deployment that needs stable semantics must select a frozen spec explicitly rather than relying on the latest one.

The gas clamp is strictly tighter than Rex6's post-opcode enforcement on the overshoot axis: the crossing opcode does not run, and enforced usage does not pass the limit by that opcode's cost.
Rex7 can report more compute gas than Rex6 for the same inputs on three paths: the exceptional-halt frame carve-out, which over-reports rather than under-reports; a failing precompile whose unused forwarded envelope is now reported as destroyed; and a `disableVolatileDataAccess` rejection, which now includes the rejected opcode's static fee.
The carve-out's enforcing half is never looser than Rex6's on interpreter frames, and is stricter in exactly one shape: an ordinary out-of-gas taken with no clamp in force, whose zeroed counter leaves the whole segment measuring as executed.
On a precompile that fails before performing work, Rex7 enforcement is deliberately looser than Rex6's: the unused envelope does not bind the compute limit.

## References

- [Hardforks and Specs](../hardfork-spec.md) — how specs are versioned, frozen, and activated.
- [Rex6 Network Upgrade](rex6.md) — the frozen spec Rex7 inherits from.
- [Compute Gas Accounting](../evm/compute-gas.md) — measurement windows, metering classes, and exceed behavior (Rex7 details on that page).
- [Dual Gas Model](../evm/dual-gas-model.md) — total gas, storage gas, and metering order.
- [Gas Detention](../evm/gas-detention.md) — detained compute-gas caps.
- [Multidimensional Resource Limits](../evm/resource-limits.md) — transaction- and frame-level limit outcomes.
