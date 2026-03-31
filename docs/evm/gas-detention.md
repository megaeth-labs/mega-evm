---
description: MegaETH gas detention specification — compute gas caps triggered by volatile data access (block environment, oracle SLOAD).
spec: Rex3
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

Detention is an **absolute cap** on total compute gas used by the transaction.
If a volatile access applies a detention cap `cap`, the transaction's effective compute gas limit becomes:

```
effective_compute_gas_limit = min(tx_compute_gas_limit, cap)
```

If `compute_gas_used > effective_compute_gas_limit` at the moment detention is applied, the transaction MUST halt immediately with `VolatileDataAccessOutOfGas`.

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

<details>
<summary>Rex4 (unstable): Additional beneficiary trigger</summary>

In Rex4, `SELFDESTRUCT` targeting the beneficiary MUST also trigger beneficiary gas detention.

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
This means:

```
effective_compute_gas_limit = min(tx_compute_gas_limit, all_applied_detention_caps)
```

Applying a later volatile access MUST NOT increase the effective detained limit.

### Execution Semantics

When a volatile-data trigger occurs, the node MUST perform the following steps in order:

1. Identify the detention category and its cap.
2. Update the transaction's effective compute gas limit to the minimum of the current effective limit and the detention cap.
3. Compare the current `compute_gas_used` against the updated effective limit.
4. If `compute_gas_used` already exceeds the updated effective limit, halt the transaction immediately with `OutOfGas`.
5. Otherwise continue execution subject to the updated limit.

After detention has been applied, any subsequent execution step that would cause `compute_gas_used` to exceed the effective detained limit MUST halt the transaction with `VolatileDataAccessOutOfGas`.

### Refund Semantics

Gas detention does not consume the detained portion of the transaction's gas budget.
If a transaction halts because the detained compute gas limit would be exceeded, the unused gas beyond actual execution MUST remain refundable under the same rules as other unused transaction gas.

Detention therefore limits execution but MUST NOT itself create an additional gas charge beyond the compute gas actually consumed.

### Transaction Boundary

The detained compute gas limit MUST be reset at the start of each transaction.
Gas detention state from one transaction MUST NOT carry over to subsequent transactions in the same block.

### Corner Cases

#### Immediate Exceed on Access

If a transaction has already consumed more compute gas than the detention cap at the moment volatile access occurs, the node MUST halt the transaction immediately with `VolatileDataAccessOutOfGas`.

#### Repeated Access to Same Category

Repeated access to the same volatile-data category within the same transaction MUST NOT relax the effective detained limit.
Reapplying the same cap is idempotent.

#### Access Across Multiple Call Frames

Detention is transaction-scoped, not call-frame-scoped.
If a child call frame triggers detention, the reduced effective compute gas limit MUST apply to the remainder of the transaction, including parent and sibling call frames.

#### Reverted Call Frames

If volatile access occurs inside a call frame that later reverts, the compute gas already consumed remains consumed.
The detained compute gas limit MUST remain in effect for the rest of the transaction.

<details>
<summary>Rex4 (unstable): Relative detention</summary>

Rex4 replaces absolute detention caps with relative detention caps.
When volatile access applies a detention cap `cap`, the effective detained limit becomes:

```
effective_detained_limit = current_compute_gas_used + cap
effective_compute_gas_limit = min(tx_compute_gas_limit, effective_detained_limit)
```

This means a transaction MAY always consume up to `cap` more compute gas after the volatile access, regardless of how much compute gas was consumed before the access.
Across multiple volatile accesses, the node MUST keep the most restrictive resulting effective limit.

</details>

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `BLOCK_ENV_DETENTION_CAP` | 20,000,000 | Absolute compute gas cap after block-environment access |
| `BENEFICIARY_DETENTION_CAP` | 20,000,000 | Absolute compute gas cap after beneficiary access |
| `ORACLE_DETENTION_CAP` | 20,000,000 | Absolute compute gas cap after oracle storage access (Rex3+) |
| `ORACLE_DETENTION_CAP_MINIREX` | 1,000,000 | Absolute compute gas cap after oracle access in MiniRex through Rex2 |

## Rationale

**Why detention instead of outright prohibition?**
MegaETH must permit contracts to read shared inputs such as time, block metadata, and oracle-fed values.
Outright banning such reads would make large classes of contracts non-viable.
Detention preserves expressiveness while bounding the computation that may follow a conflict-prone read.

**Why an absolute cap?**
The absolute model is simple to enforce and guarantees a hard upper bound on computation after volatile access.
Its main drawback is that late volatile access can cause immediate failure if substantial compute gas was already consumed.
Rex4 changes this to a relative model to avoid penalizing late access.

**Why make the most restrictive cap win?**
A transaction that touches multiple volatile sources should be governed by the strongest applicable constraint.
Allowing a less restrictive later trigger to relax an earlier cap would make detention order-dependent and harder to reason about.

**Why make detention transaction-scoped?**
The purpose of detention is to bound the remainder of execution after volatile access.
If the cap were scoped only to the triggering call frame, contracts could evade the limit by returning to a parent frame and continuing computation there.

## Spec History

Gas detention semantics evolved across specs:

- [MiniRex](../upgrades/minirex.md) — introduced gas detention; block-environment cap 20M, oracle cap 1M, oracle triggering based on message-call access
- [Rex](../upgrades/rex.md) — made CALL-like opcode behavior consistent
- [Rex1](../upgrades/rex1.md) — reset detained compute gas limit between transactions in the same block
- [Rex3](../upgrades/rex3.md) — raised oracle cap to 20M and changed oracle detection from CALL-based to SLOAD-based
- [Rex4](../upgrades/rex4.md) *(unstable)* — changes absolute detention to relative detention and adds additional beneficiary-triggered behavior
