---
description: MegaETH resource accounting specification — counter semantics, revert behavior, and per-opcode metering for compute gas, data size, KV updates, and state growth.
spec: Rex4
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

| Data Type                 | Size                                                |
| ------------------------- | --------------------------------------------------- |
| Base transaction data     | `BASE_TRANSACTION_DATA_SIZE`                        |
| Calldata                  | `tx.input().len()`                                  |
| Access list               | Sum of encoded entry sizes                          |
| EIP-7702 authorizations   | `AUTHORIZATION_DATA_SIZE × authorization_count`     |
| Caller account update     | `ACCOUNT_UPDATE_DATA_SIZE`                          |
| Authority account updates | `ACCOUNT_UPDATE_DATA_SIZE × authority_update_count` |

#### Discardable Data Size

The following contributions MUST be tracked within call frames and MUST be discarded if the call frame reverts:

| Data Type                        | Size                                | Trigger                                  |
| -------------------------------- | ----------------------------------- | ---------------------------------------- |
| Log topics                       | `LOG_TOPIC_DATA_SIZE × topic_count` | `LOG0`–`LOG4`                            |
| Log data                         | `log_data.len()`                    | `LOG0`–`LOG4`                            |
| SSTORE new write                 | `ACCOUNT_UPDATE_DATA_SIZE`          | `original == present && original != new` |
| SSTORE reset                     | `-ACCOUNT_UPDATE_DATA_SIZE`         | `original != present && original == new` |
| Account update (CALL with value) | `ACCOUNT_UPDATE_DATA_SIZE`          | Balance change on CALL-like operation    |
| Account update (CREATE/CREATE2)  | `ACCOUNT_UPDATE_DATA_SIZE`          | Successful account creation path         |
| Deployed bytecode                | `code.len()`                        | Successful `CREATE` or `CREATE2`         |

<details>
<summary>Rex5 (unstable): Caller account update deduplication for data-size tracking</summary>

#### Account Update Deduplication

Within a single call frame, a node MUST count a given account update at most once for data-size tracking.
If the same account is updated multiple times within the same call frame — including the caller account across multiple value-transferring sub-calls or creates — subsequent updates in that call frame MUST NOT add additional `ACCOUNT_UPDATE_DATA_SIZE` bytes.

</details>

<details>
<summary>Rex6 (unstable): EIP-7702 authority account updates narrowed to applied authorizations</summary>

#### Applied-Authorization Narrowing for Data Size

Pre-Rex6, a node counts one `ACCOUNT_UPDATE_DATA_SIZE` authority account update for every authorization whose authority address is recoverable, including authorizations that are later skipped by the chain-id, nonce, or code application gates.

Under Rex6, a node MUST count the `ACCOUNT_UPDATE_DATA_SIZE` authority account update only for an _applied_ authorization — one that passes all application gates and therefore writes the authority account.
A node MUST NOT count a skipped authorization toward `authority_update_count`.
The per-record `AUTHORIZATION_DATA_SIZE × authorization_count` contribution is unchanged and still counts every authorization in the list.

When multiple authorizations target the same authority, a node MUST evaluate them sequentially against the authority nonce and MUST count each applied authorization independently.

</details>

<details>
<summary>Rex6 (unstable): Per-log data-size base</summary>

#### Per-Log Base Cost

Pre-Rex6, a log contributes `LOG_TOPIC_DATA_SIZE × topic_count + log_data.len()` to data size; the log address is not counted, so an empty `LOG0` (no topics, no data) contributes zero even though it produces a receipt log entry.

Under Rex6, a node MUST additionally charge a fixed `LOG_BASE_DATA_SIZE` per emitted log for the log address, so each log contributes `LOG_BASE_DATA_SIZE + LOG_TOPIC_DATA_SIZE × topic_count + log_data.len()`.

</details>

### KV Updates

#### Definition

A node MUST track KV updates as the number of state-modifying key-value updates attributable to the transaction.

#### Non-Discardable KV Updates

The following contributions MUST be counted at transaction scope and MUST NOT be reverted:

| Operation                  | Count                 |
| -------------------------- | --------------------- |
| Transaction caller update  | `1`                   |
| EIP-7702 authority updates | `authorization_count` |

#### Discardable KV Updates

The following contributions MUST be tracked within call frames and MUST be discarded if the call frame reverts:

| Operation        | Count      | Trigger                                                                                |
| ---------------- | ---------- | -------------------------------------------------------------------------------------- |
| SSTORE new write | `+1`       | `original == present && original != new`                                               |
| SSTORE reset     | `-1`       | `original != present && original == new`                                               |
| CREATE/CREATE2   | `1` or `2` | Created account plus caller update if caller not yet counted in the current call frame |
| CALL with value  | `1` or `2` | Callee update plus caller update if caller not yet counted in the current call frame   |

<details>
<summary>Rex5 (unstable): Caller account update deduplication for KV-update tracking</summary>

#### Account Update Deduplication

Within a single call frame, a node MUST deduplicate caller account updates for KV-update tracking in the same way it does for data-size tracking.
When a CALL with value or CREATE occurs, the caller's update MUST be counted only if it has not already been counted in the current call frame.

</details>

<details>
<summary>Rex6 (unstable): EIP-7702 authority updates narrowed to applied authorizations</summary>

#### Applied-Authorization Narrowing for KV Updates

Pre-Rex6, a node counts one authority KV update for every authorization with a recoverable authority, including authorizations that are skipped by the application gates.

Under Rex6, a node MUST count one authority KV update only for each _applied_ authorization — one that passes the chain-id, nonce, and code gates and writes the authority account — mirroring the data-size narrowing above.
A node MUST NOT count a skipped authorization.
When multiple authorizations target the same authority, each applied authorization MUST be counted independently.

</details>

### State Growth

#### Definition

A node MUST track state growth as the net increase in on-chain state caused by new accounts and new storage slots.

#### Storage Slot Growth Rules

For `SSTORE`, a node MUST apply the following state-growth accounting rules:

| Original | Present | New     | Growth |
| -------- | ------- | ------- | ------ |
| `0`      | `0`     | non-`0` | `+1`   |
| `0`      | non-`0` | `0`     | `-1`   |
| `0`      | non-`0` | non-`0` | `0`    |
| non-`0`  | any     | any     | `0`    |

The table above means:

- the first write to a slot that was empty at transaction start MUST increase state growth by `1`,
- clearing such a slot later in the same transaction MUST decrease state growth by `1`,
- rewriting a slot already counted within the transaction MUST NOT change state growth further,
- and slots that were already non-zero at transaction start MUST NOT contribute to state growth.

#### SELFDESTRUCT Refund

When a same-transaction-created contract is destroyed by `SELFDESTRUCT`, the node MUST apply a state-growth refund.
See [SELFDESTRUCT — State Growth Refund](selfdestruct.md#state-growth-refund) for the full specification.

#### Negative Intermediate Values

The state-growth counter MAY become negative during execution.
The reported final state growth for limit enforcement MUST be clamped to a minimum of `0`.

## Constants

| Constant                     | Value | Description                                                                       |
| ---------------------------- | ----- | --------------------------------------------------------------------------------- |
| `BASE_TRANSACTION_DATA_SIZE` | 110   | Fixed estimate of the RLP-encoded transaction envelope excluding calldata         |
| `AUTHORIZATION_DATA_SIZE`    | 101   | Bytes counted per EIP-7702 authorization                                          |
| `ACCOUNT_UPDATE_DATA_SIZE`   | 40    | Bytes counted for an account update or storage-write record in data-size tracking |
| `LOG_TOPIC_DATA_SIZE`        | 32    | Bytes counted per log topic in data-size tracking                                 |
| `LOG_BASE_DATA_SIZE`         | 32    | Rex6+ per-log base counted for the log address in data-size tracking              |

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

## Security Considerations

**If compute gas were made revertible** (scoped to call frames like data size and KV updates), an attacker could execute and revert expensive subcalls repeatedly within a single transaction, consuming negligible apparent compute gas while imposing real execution cost on nodes.

## Spec History

This page describes the current accounting behavior.

- [Rex4](../upgrades/rex4.md) — introduced per-call-frame runtime budgets for all four resource dimensions.
- [Rex5](../upgrades/rex5.md) (**unstable**) — corrected caller-account update deduplication: pre-Rex5, the caller's `ACCOUNT_UPDATE_DATA_SIZE` (data size) and KV-update count were re-charged on every value-transferring sub-call or create from the same parent frame because the `target_updated` flag was never set after the first charge; Rex5 marks the flag after the first charge so subsequent operations from the same parent frame do not re-count the caller account.
- Rex6 (**unstable**) — narrowed the EIP-7702 authority data-size and KV-update charges from every recoverable authorization to only _applied_ authorizations: pre-Rex6, the `ACCOUNT_UPDATE_DATA_SIZE` and KV update were charged for every authorization with a recoverable authority, including ones later skipped by the chain-id, nonce, or code application gates; Rex6 charges them only for authorizations that pass all gates and write the authority account.
- Rex6 (**unstable**) — corrected two `CREATE`-frame accounting errors: the creator nonce-bump account-info write is booked to the parent frame's discardable lane instead of the child's, so it survives a child-`CREATE` revert correctly; and `CREATE` records `+1` state growth only when the created address is net-new instead of unconditionally.
- Rex6 (**unstable**) — counted account materializations performed by op-revm's post-execution `reward_beneficiary` step toward resource accounting: pre-Rex6, a fee recipient that the reward step created after the `AdditionalLimit` trackers were finalized escaped state-growth and account-update accounting; Rex6 counts such materializations. The deposit-mint half was already closed in Rex5; Rex6 covers the remaining non-deposit fee-credit paths.
- Rex6 (**unstable**) — added a per-log data-size base: pre-Rex6, an empty `LOG0` contributed zero data size because the log address was not counted; Rex6 charges `LOG_BASE_DATA_SIZE` per log for the address.
