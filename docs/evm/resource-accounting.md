---
description: MegaETH resource accounting specification — counter semantics, revert behavior, and per-opcode metering for compute gas, data size, KV updates, and state growth.
spec: Rex3
---

# Resource Accounting

This page specifies how MegaETH accounts for usage across the four runtime resource dimensions: [compute gas](../glossary.md#compute-gas), data size, KV updates, and state growth.
It defines what each dimension tracks, when counters are updated, and how revert behavior affects the counters.

## Motivation

MegaETH enforces multiple runtime resource limits in addition to the transaction gas limit.
Those limits are meaningful only if every node computes the same resource usage for the same transaction.

Without a precise accounting specification, different implementations could disagree on questions such as:

- whether reverted subcalls still count toward a resource dimension,
- whether repeated account updates should be counted once or multiple times,
- whether new storage writes and later resets within the same transaction cancel out,
- and whether logs or deployed bytecode should count before or after success is known.

Resource accounting exists to make runtime-limit enforcement deterministic across implementations.

## Specification

The named constants referenced in this section are defined later in [Constants](#constants).

### Overview

MegaETH defines four runtime resource dimensions:

1. [Compute gas](../glossary.md#compute-gas)
2. Data size
3. KV updates
4. State growth

A node MUST track each dimension independently.
Runtime limit enforcement for these dimensions is defined in [Multidimensional Resource Limits](resource-limits.md).
This page defines only how usage is counted.

### Revert Behavior

Unless explicitly stated otherwise on this page, resource trackers MUST be [call-frame](../glossary.md#call-frame)-aware:

- usage created within a child call frame MUST be discarded if that child frame reverts,
- and usage created within a child call frame MUST be merged into the parent call frame if that child call frame succeeds.

The sole stable exception is [compute gas](../glossary.md#compute-gas), which MUST accumulate globally and MUST NOT be reverted.

<details>
<summary>Rex4 (unstable): Per-call-frame limits</summary>

Rex4 adds per-call-frame budgets for all four dimensions.
The top-level call frame starts with the full transaction budget.
Each inner call frame receives `remaining × 98 / 100` of its parent call frame's remaining budget.
If an inner call frame exceeds its local budget, it MUST revert with `MegaLimitExceeded(uint8 kind, uint64 limit)`.
The parent call frame MAY continue execution.
Compute gas consumed by reverted call frames MUST still count toward the transaction total.

</details>

### Compute Gas

#### Definition

A node MUST track compute gas as the cumulative gas consumed by EVM execution, independent of [storage gas](dual-gas-model.md).

#### Included Usage

A node MUST include the following in compute gas usage:

- gas consumed by EVM instruction execution,
- memory expansion costs,
- and precompile costs.

#### Excluded Usage

A node MUST NOT subtract gas refunds from compute gas usage.
Refunds affect final gas settlement but do not reduce the tracked compute gas consumed during execution.

#### Revert Behavior

Compute gas usage MUST NOT be reverted when a child call frame reverts.
All compute gas spent by all executed call frames contributes to the transaction's total compute gas usage.

#### Enforcement Reference

If `compute_gas_used > effective_compute_gas_limit`, the transaction MUST halt.
The effective limit MAY be reduced by [gas detention](gas-detention.md).

### Data Size

#### Definition

A node MUST track data size as the total number of bytes of execution-related data attributable to the transaction.

#### Non-Discardable Data Size

The following contributions MUST be counted at transaction start and MUST NOT be reverted:

| Data Type | Size |
| --------- | ---- |
| Base transaction data | `BASE_TRANSACTION_DATA_SIZE` |
| Calldata | `tx.input().len()` |
| Access list | Sum of encoded entry sizes |
| EIP-7702 authorizations | `AUTHORIZATION_DATA_SIZE × authorization_count` |
| Caller account update | `ACCOUNT_UPDATE_DATA_SIZE` |
| Authority account updates | `ACCOUNT_UPDATE_DATA_SIZE × authority_update_count` |

#### Discardable Data Size

The following contributions MUST be tracked within call frames and MUST be discarded if the call frame reverts:

| Data Type | Size | Trigger |
| --------- | ---- | ------- |
| Log topics | `LOG_TOPIC_DATA_SIZE × topic_count` | `LOG0`–`LOG4` |
| Log data | `log_data.len()` | `LOG0`–`LOG4` |
| SSTORE new write | `ACCOUNT_UPDATE_DATA_SIZE` | `original == present && original != new` |
| SSTORE reset | `-ACCOUNT_UPDATE_DATA_SIZE` | `original != present && original == new` |
| Account update (CALL with value) | `ACCOUNT_UPDATE_DATA_SIZE` | Balance change on CALL-like operation |
| Account update (CREATE/CREATE2) | `ACCOUNT_UPDATE_DATA_SIZE` | Successful account creation path |
| Deployed bytecode | `code.len()` | Successful `CREATE` or `CREATE2` |

#### Account Update Deduplication

Within a single call frame, a node MUST count a given account update at most once for data-size tracking.
If the same account is updated multiple times within the same call frame, subsequent updates in that call frame MUST NOT add additional `ACCOUNT_UPDATE_DATA_SIZE` bytes.

### KV Updates

#### Definition

A node MUST track KV updates as the number of state-modifying key-value updates attributable to the transaction.

#### Non-Discardable KV Updates

The following contributions MUST be counted at transaction scope and MUST NOT be reverted:

| Operation | Count |
| --------- | ----- |
| Transaction caller update | `1` |
| EIP-7702 authority updates | `authorization_count` |

#### Discardable KV Updates

The following contributions MUST be tracked within call frames and MUST be discarded if the call frame reverts:

| Operation | Count | Trigger |
| --------- | ----- | ------- |
| SSTORE new write | `+1` | `original == present && original != new` |
| SSTORE reset | `-1` | `original != present && original == new` |
| CREATE/CREATE2 | `1` or `2` | Created account plus caller update if caller not yet counted in the current call frame |
| CALL with value | `1` or `2` | Callee update plus caller update if caller not yet counted in the current call frame |

#### Account Update Deduplication

Within a single call frame, a node MUST deduplicate caller account updates for KV-update tracking in the same way it does for data-size tracking.
When a CALL with value or CREATE occurs, the caller's update MUST be counted only if it has not already been counted in the current call frame.

### State Growth

#### Definition

A node MUST track state growth as the net increase in on-chain state caused by new accounts and new storage slots.

#### Storage Slot Growth Rules

For `SSTORE`, a node MUST apply the following state-growth accounting rules:

| Original | Present | New | Growth |
| -------- | ------- | --- | ------ |
| `0` | `0` | non-`0` | `+1` |
| `0` | non-`0` | `0` | `-1` |
| `0` | non-`0` | non-`0` | `0` |
| non-`0` | any | any | `0` |

The table above means:

- the first write to a slot that was empty at transaction start MUST increase state growth by `1`,
- clearing such a slot later in the same transaction MUST decrease state growth by `1`,
- rewriting a slot already counted within the transaction MUST NOT change state growth further,
- and slots that were already non-zero at transaction start MUST NOT contribute to state growth.

#### Negative Intermediate Values

The state-growth counter MAY become negative during execution.
The reported final state growth for limit enforcement MUST be clamped to a minimum of `0`.

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `BASE_TRANSACTION_DATA_SIZE` | 110 | Fixed estimate of the RLP-encoded transaction envelope excluding calldata |
| `AUTHORIZATION_DATA_SIZE` | 101 | Bytes counted per EIP-7702 authorization |
| `ACCOUNT_UPDATE_DATA_SIZE` | 40 | Bytes counted for an account update or storage-write record in data-size tracking |
| `LOG_TOPIC_DATA_SIZE` | 32 | Bytes counted per log topic in data-size tracking |

## Rationale

**Why make most resource dimensions call-frame-aware?**
Data size, KV updates, and state growth represent effects that should match the surviving transaction outcome.
If a child call frame reverts, its discarded logs, writes, and transient growth should not count toward the final resource totals.

**Why is compute gas the exception?**
Compute gas measures work already performed by the node.
That work cannot be undone merely because a child call frame reverted.
Making compute gas non-revertible prevents implementations from undercounting resource consumption in transactions that repeatedly attempt and revert expensive subcalls.

**Why deduplicate account updates within a call frame?**
Repeated writes to the same account within one call frame do not represent distinct independent account objects in state.
Deduplication prevents artificial inflation of data-size and KV-update counts from repeated modifications to the same account within a single call frame.

**Why allow negative intermediate state growth?**
During execution, a transaction may first create new state and later remove it.
Allowing the counter to go negative during intermediate steps keeps the accounting locally composable across nested call frames, while clamping the final reported value prevents negative net state growth from being treated as a meaningful resource credit.

## Spec History

This page describes the current accounting behavior.
Per-call-frame runtime budgets are introduced only in [Rex4](../upgrades/rex4.md), and therefore remain unstable.
