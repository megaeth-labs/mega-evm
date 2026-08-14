---
description: MegaETH gas detention specification — compute gas caps triggered by volatile data access (block environment, oracle SLOAD).
spec: Rex6
---

# Gas Detention

This page specifies the current gas-detention behavior.
Gas detention limits post-access [compute gas](../glossary.md#compute-gas) after a transaction reads [volatile data](../glossary.md#volatile-data), bounding the amount of computation that may occur after access to shared, conflict-prone inputs.

## Motivation

MegaETH executes transactions with aggressive parallelism.
Certain inputs are shared across many transactions and therefore create conflict hotspots: block-environment fields, the [beneficiary](../glossary.md#beneficiary) account, and oracle-backed data.

Without an additional constraint, a transaction could read one of these shared inputs and then continue executing an arbitrarily large amount of computation.
That pattern increases contention, reduces parallel execution efficiency, and makes worst-case execution time depend on transactions that touch conflict-prone state.

Gas detention addresses this by limiting the remaining compute budget after volatile data access.
The transaction is still permitted to read the data, but the amount of computation that can follow the access is bounded.

## Specification

The named constants referenced in this section are defined later in [Constants](#constants).

### Overview

A node MUST apply gas detention when a transaction accesses volatile data as defined on this page.
Gas detention affects only [compute gas](../glossary.md#compute-gas).
It MUST NOT directly change storage gas accounting, [data size](resource-accounting.md#data-size), [KV updates](resource-accounting.md#kv-updates), or [state growth](resource-accounting.md#state-growth).

Detention applies a **relative cap** on compute gas.
When a volatile access applies a detention cap `cap`, the effective detained limit becomes:

```
effective_detained_limit = current_compute_gas_used + cap
effective_compute_gas_limit = min(tx_compute_gas_limit, effective_detained_limit)
```

This means a transaction MAY always consume up to `cap` more compute gas after the volatile access, regardless of how much compute gas was consumed before the access.

### Volatile Data Categories

The following volatile data categories trigger detention.

#### Block Environment Access

A node MUST apply block-environment gas detention with cap `BLOCK_ENV_DETENTION_CAP` when a transaction executes any of the following opcodes:

- `NUMBER`
- `TIMESTAMP`
- `COINBASE`
- `DIFFICULTY` / `PREVRANDAO`
- `GASLIMIT`
- `BASEFEE`
- `BLOCKHASH`
- `BLOBBASEFEE`
- `BLOBHASH`

#### Beneficiary Access

A node MUST apply beneficiary gas detention with cap `BENEFICIARY_DETENTION_CAP` when a transaction accesses the [beneficiary](../glossary.md#beneficiary) account through any of the following behaviors:

- `BALANCE` on the beneficiary address
- `SELFBALANCE` when the current contract is the beneficiary
- `EXTCODECOPY` on the beneficiary address
- `EXTCODESIZE` on the beneficiary address
- `EXTCODEHASH` on the beneficiary address
- a transaction whose sender is the beneficiary
- a transaction or call frame whose recipient is the beneficiary
- beneficiary access performed through `DELEGATECALL`

`SELFDESTRUCT` targeting the beneficiary MUST also trigger beneficiary gas detention.

When a `CALL`, `CALLCODE`, `DELEGATECALL`, or `STATICCALL` loads a target account whose code is an [EIP-7702](https://eips.ethereum.org/EIPS/eip-7702) delegation designation, the node MUST resolve the delegation one hop and mark beneficiary access when either the raw target or the resolved delegate equals the beneficiary.

A node MUST also apply beneficiary gas detention when an _applied_ EIP-7702 authorization — one that passes the chain-id, nonce, and code application gates and therefore writes the authority account — has an authority address equal to the block beneficiary.
Applying such an authorization mutates beneficiary state (nonce and delegation code), so the node MUST mark beneficiary access and re-derive the effective compute-gas detention cap during transaction validation, even though no opcode in the list above was executed.
A skipped authorization whose authority equals the beneficiary MUST NOT trigger detention.

#### Oracle Access

A node MUST apply oracle gas detention with cap `ORACLE_DETENTION_CAP` when a transaction performs `SLOAD` against the storage of the [oracle contract](../system-contracts/oracle.md).

The following rules MUST apply:

- `CALL` to the oracle contract address alone MUST NOT trigger oracle detention.
- `STATICCALL` to the oracle contract address alone MUST NOT trigger oracle detention.
- Oracle detention is triggered by storage reads, not by message-call targeting alone.
- `DELEGATECALL` to the oracle contract MUST NOT trigger oracle detention solely by virtue of targeting the oracle address, because `SLOAD` in a `DELEGATECALL` context reads the caller's storage, not the oracle contract's storage.
- If the transaction sender is [`MEGA_SYSTEM_ADDRESS`](../system-contracts/system-tx.md), oracle gas detention MUST NOT be applied.

### Cap Selection

If multiple volatile-data categories are accessed during the same transaction, the node MUST apply the most restrictive effective cap.
Each volatile access produces its own effective detained limit (`current_compute_gas_used + cap` at the time of that access).
The node MUST keep the minimum across all such limits:

```
effective_compute_gas_limit = min(tx_compute_gas_limit, all effective_detained_limits)
```

Applying a later volatile access MUST NOT increase the effective detained limit.

### Execution Semantics

When a volatile-data trigger occurs, the node MUST perform the following steps in order:

1. Identify the detention category and its cap.
2. Compute the effective detained limit as `current_compute_gas_used + cap`.
3. Update the transaction's effective compute gas limit to the minimum of the current effective limit and the newly computed effective detained limit.
4. Continue execution subject to the updated limit.

After detention has been applied, any subsequent execution step that would cause `compute_gas_used` to exceed the effective detained limit MUST halt the transaction with `VolatileDataAccessOutOfGas`.

<details>
<summary>Rex7 (unstable): clamp-based detention enforcement inside plain segments</summary>

Under Rex7, after a detention cap has been installed the remaining compute headroom includes that detained limit, and the gas clamp applied at checkpoints and frame boundaries restricts interpreter-visible gas to that headroom.
A plain-opcode segment that would cross the detained limit is therefore stopped at the clamp boundary before the crossing opcode executes, reclassified as `VolatileDataAccessOutOfGas`, with remaining gas rescued for the sender — the same halt reason and refund shape as through Rex6, but without executing the crossing opcode or recording its cost.
See [Compute Gas Accounting](compute-gas.md) and the [Rex7 Network Upgrade](../upgrades/rex7.md).

</details>

The detained compute-gas limit MUST NOT halt a [system-originated transaction](../system-contracts/system-tx.md#system-originated-transaction-metering-exemption).
Volatile-data accesses by such a transaction are still tracked, but the detention cap is not enforced against it; its standard EVM `gas_limit` remains the only halting bound.

### Refund Semantics

Gas detention does not consume the detained portion of the transaction's gas budget.
If a transaction halts because the detained compute gas limit would be exceeded, the unused gas beyond actual execution MUST remain refundable under the same rules as other unused transaction gas.

Detention therefore limits execution but MUST NOT itself create an additional gas charge beyond the compute gas actually consumed.

### Transaction Boundary

The detained compute gas limit MUST be reset at the start of each transaction.
Gas detention state from one transaction MUST NOT carry over to subsequent transactions in the same block.

### Corner Cases

#### Repeated Access to Same Category

Repeated access to the same volatile-data category within the same transaction MUST NOT relax the effective detained limit.
Reapplying the same cap is idempotent.

#### Access Across Multiple Call Frames

Detention is transaction-scoped, not call-frame-scoped.
If a child call frame triggers detention, the reduced effective compute gas limit MUST apply to the remainder of the transaction, including parent and sibling call frames.

#### Reverted Call Frames

If volatile access occurs inside a call frame that later reverts, the compute gas already consumed remains consumed.
The detained compute gas limit MUST remain in effect for the rest of the transaction.

#### Frames That Run Out of Gas on the Triggering Opcode

Detention is triggered by the volatile read itself, not by the successful completion of the opcode that issues it.
`BALANCE`, `EXTCODESIZE`, `EXTCODEHASH`, `SLOAD` and `SELFDESTRUCT` consume their operands and register their volatile access before that access is charged for.
A frame that reaches one of them holding less gas than the access costs therefore still registers the access, and the registration MUST survive that frame's out-of-gas halt for the rest of the transaction.
The reduced compute gas limit binds at detention enforcement points, which are the volatile-guarded opcodes themselves: a registration made by a halting frame takes effect at the next volatile-guarded opcode the transaction executes, in any frame.
A transaction whose halting read is its final volatile access reaches no further enforcement point, and its remainder runs under the limit already in effect.
The CALL-family opcodes are excluded from this registration guarantee: their base access cost — and, for value transfers, the transfer cost — is charged before the target account is read, so a frame that cannot afford those charges halts without registering the access.
`EXTCODECOPY` is excluded for the same reason: its copy cost is charged before the target account is read, so only a frame that affords the copy cost registers the access.
An access blocked by [`disableVolatileDataAccess()`](../system-contracts/mega-access-control.md) is the exception: the blocked opcode never runs, so it reads nothing and triggers nothing.

<details>
<summary>Rex7 (unstable): detention mark at account load</summary>

Under Rex7 the CALL-family / `EXTCODECOPY` charge-before-load order is specified, not a frozen replay window.
A node MUST produce the beneficiary (or oracle) mark when the target account or slot is loaded, and MUST NOT produce that mark from a frame that cannot afford the fees charged before the load.
A CALL that exhausts the frame on its static fee or value-transfer fee, and an `EXTCODECOPY` that exhausts the frame on its copy fee, therefore halt without detaining the rest of the transaction.
See the [Rex7 Network Upgrade](../upgrades/rex7.md).

</details>

<details>
<summary>Rex7 (unstable): charge-on-reject for disabled volatile access</summary>

Under Rex7 a node MUST still revert a `disableVolatileDataAccess` rejection with `VolatileDataAccessDisabled` and MUST still leave the tracker unmarked.
A node MUST charge the rejected opcode's static fee before that revert.
The fee is ordinary EVM gas, is not refunded by the synthetic revert, and is recorded as compute gas when the open segment is settled.
A frame that cannot afford the static fee MUST halt out of gas instead of reaching the disable revert.
Through Rex6 the same reject charges nothing.
See the [Rex7 Network Upgrade](../upgrades/rex7.md).

</details>

## Constants

| Constant                       | Value      | Description                                                          |
| ------------------------------ | ---------- | -------------------------------------------------------------------- |
| `BLOCK_ENV_DETENTION_CAP`      | 20,000,000 | Relative compute gas cap after block-environment access              |
| `BENEFICIARY_DETENTION_CAP`    | 20,000,000 | Relative compute gas cap after beneficiary access                    |
| `ORACLE_DETENTION_CAP`         | 20,000,000 | Relative compute gas cap after oracle storage access                 |
| `ORACLE_DETENTION_CAP_MINIREX` | 1,000,000  | Historical absolute compute gas cap after oracle access (superseded) |

`BLOCK_ENV_DETENTION_CAP` and `BENEFICIARY_DETENTION_CAP` have the same value: block-environment and beneficiary access are detained at the same level.

## Rationale

**Why detention instead of outright prohibition?**
MegaETH must permit contracts to read shared inputs such as time, block metadata, and oracle-fed values.
Outright banning such reads would make large classes of contracts non-viable.
Detention preserves expressiveness while bounding the computation that may follow a conflict-prone read.

**Why a relative cap instead of an absolute cap?**
The original MiniRex design used an absolute cap, which guaranteed a hard upper bound on total compute gas after volatile access.
Its drawback was that late volatile access could cause immediate failure if substantial compute gas was already consumed — penalizing transactions for work done _before_ touching volatile data.
The relative model avoids this by guaranteeing a fixed budget of additional compute gas _after_ the access, regardless of prior consumption.

**Why make the most restrictive cap win?**
A transaction that touches multiple volatile sources should be governed by the strongest applicable constraint.
Allowing a less restrictive later trigger to relax an earlier cap would make detention order-dependent and harder to reason about.

**Why make detention transaction-scoped?**
The purpose of detention is to bound the remainder of execution after volatile access.
If the cap were scoped only to the triggering call frame, contracts could evade the limit by returning to a parent frame and continuing computation there.

## Security Considerations

**If detention is call-frame-scoped rather than transaction-scoped**, a contract can trigger volatile access inside a child call frame, revert the frame, and resume unbounded execution in the parent — entirely bypassing detention.
Transaction-level scoping is essential to preserve the invariant that compute gas after any volatile access is bounded.

**If detention applied in a call frame that later reverts is reversed**, an attacker can trigger volatile access inside a frame it then reverts to escape the detention cap for the rest of the transaction.

## Spec History

Gas detention semantics evolved across specs:

- [MiniRex](../upgrades/minirex.md) — introduced gas detention; block-environment cap 20M, oracle cap 1M, oracle triggering based on message-call access
- [Rex](../upgrades/rex.md) — made CALL-like opcode behavior consistent
- [Rex1](../upgrades/rex1.md) — reset detained compute gas limit between transactions in the same block
- [Rex3](../upgrades/rex3.md) — raised oracle cap to 20M and changed oracle detection from CALL-based to SLOAD-based
- [Rex4](../upgrades/rex4.md) — changes absolute detention to relative detention and adds additional beneficiary-triggered behavior
- [Rex6](../upgrades/rex6.md) — adds a beneficiary-detention trigger for an applied EIP-7702 authorization whose authority equals the block beneficiary; resolves a CALL-family target's EIP-7702 delegation one hop before the beneficiary comparison, so a call through a delegator whose delegate is the beneficiary triggers detention (through Rex5 only the raw target is compared); and stops enforcing the detention cap against system-originated transactions, whose volatile accesses are still tracked
- [Rex7](../upgrades/rex7.md) _(unstable)_ — enforces the detained limit inside plain-opcode segments by gas clamping, stopping a crossing opcode before it executes while preserving `VolatileDataAccessOutOfGas` and gas rescue; specifies that a detention mark is produced when the target account is loaded, so a frame that cannot afford the pre-load fees produces no mark; and charges the static fee of an opcode rejected by `disableVolatileDataAccess`
