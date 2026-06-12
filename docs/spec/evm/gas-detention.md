---
description: MegaETH gas detention specification — compute gas caps triggered by volatile data access (block environment, oracle SLOAD).
spec: Rex5
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

<details>
<summary>Rex6 (unstable): EIP-7702 delegate resolution for CALL-family beneficiary access</summary>

Under Rex6, when a `CALL`, `CALLCODE`, `DELEGATECALL`, or `STATICCALL` loads a target account whose code is an [EIP-7702](https://eips.ethereum.org/EIPS/eip-7702) delegation designation, the node MUST resolve the delegation one hop and mark beneficiary access when either the raw target or the resolved delegate equals the beneficiary.
Pre-Rex6, only the raw target address is compared, so a call routed through a delegator whose delegate is the beneficiary reads the beneficiary's account without triggering detention.

</details>

<details>
<summary>Rex6 (unstable): applied EIP-7702 authority equal to the beneficiary triggers detention</summary>

Under Rex6, a node MUST apply beneficiary gas detention when an _applied_ EIP-7702 authorization — one that passes the chain-id, nonce, and code application gates and therefore writes the authority account — has an authority address equal to the block beneficiary.
Applying such an authorization mutates beneficiary state (nonce and delegation code), so the node MUST mark beneficiary access and re-derive the effective compute-gas detention cap during transaction validation, even though no opcode in the list above was executed.
A skipped authorization whose authority equals the beneficiary MUST NOT trigger detention.

</details>

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
<summary>Rex6 (unstable): system-originated transactions are not halted by detention</summary>

Under Rex6, the detained compute-gas limit MUST NOT halt a [system-originated transaction](../system-contracts/system-tx.md#system-originated-transaction-metering-exemption).
Volatile-data accesses by such a transaction are still tracked, but the detention cap is not enforced against it; its standard EVM `gas_limit` remains the only halting bound.

</details>

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

## Constants

| Constant                       | Value      | Description                                                          |
| ------------------------------ | ---------- | -------------------------------------------------------------------- |
| `BLOCK_ENV_DETENTION_CAP`      | 20,000,000 | Relative compute gas cap after block-environment access              |
| `BENEFICIARY_DETENTION_CAP`    | 20,000,000 | Relative compute gas cap after beneficiary access                    |
| `ORACLE_DETENTION_CAP`         | 20,000,000 | Relative compute gas cap after oracle storage access                 |
| `ORACLE_DETENTION_CAP_MINIREX` | 1,000,000  | Historical absolute compute gas cap after oracle access (superseded) |

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
- [Rex6](../upgrades/rex6.md) (**unstable**) — adds a beneficiary-detention trigger for an applied EIP-7702 authorization whose authority equals the block beneficiary
- [Rex6](../upgrades/rex6.md) (**unstable**) — resolves a CALL-family target's EIP-7702 delegation one hop before the beneficiary comparison, so a call through a delegator whose delegate is the beneficiary triggers detention
- [Rex6](../upgrades/rex6.md) (**unstable**) — stops enforcing the detention cap against system-originated transactions; volatile accesses are still tracked
