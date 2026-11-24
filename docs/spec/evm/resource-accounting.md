---
description: MegaETH resource accounting specification — counter semantics, revert behavior, and per-opcode metering for compute gas, data size, KV updates, and state growth.
spec: Rex6
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

The sole exception is [compute gas](../glossary.md#compute-gas), which MUST accumulate globally and MUST NOT be reverted.

#### Creator Nonce-Bump Frame Attribution

A `CREATE` / `CREATE2` bumps the creator's nonce, and that account-info write is charged to the data-size and KV-update dimensions.
The creator's nonce bump survives a revert of the created frame under EVM semantics, so its accounting MUST follow the same scope.

A node MUST record the creator nonce-bump account-info write (`ACCOUNT_UPDATE_DATA_SIZE` bytes of data size and one KV update) in the **parent** frame's discardable lane, so that it is discarded only when the parent itself reverts.

A node MUST record the charge even for a creation rejected for exceeding the call-depth limit or for insufficient creator balance, where no nonce bump follows.

### Compute Gas

Compute gas accounting is specified in full on its own page: see [Compute Gas Accounting](compute-gas.md).

That page defines the measurement window that derives compute gas from inherited EVM gas, the per-opcode metering classes, the non-opcode recording sites, and the exceed behavior.

Two properties matter for this page's purposes:

- A node MUST track compute gas as the sum of the amounts recorded at the sites [Compute Gas Accounting](compute-gas.md) defines, independent of [storage gas](dual-gas-model.md).
  It is not simply all EVM gas consumed: gas consumed by an operation whose measurement window never closes is deliberately not recorded.
- Compute gas MUST accumulate globally and MUST NOT be reverted — the sole exception to the [revert behavior](#revert-behavior) that governs the other three dimensions.

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
| Log base                         | `LOG_BASE_DATA_SIZE`                | `LOG0`–`LOG4`                            |
| Log topics                       | `LOG_TOPIC_DATA_SIZE × topic_count` | `LOG0`–`LOG4`                            |
| Log data                         | `log_data.len()`                    | `LOG0`–`LOG4`                            |
| SSTORE new write                 | `ACCOUNT_UPDATE_DATA_SIZE`          | `original == present && original != new` |
| SSTORE reset                     | `-ACCOUNT_UPDATE_DATA_SIZE`         | `original != present && original == new` |
| Account update (CALL with value) | `ACCOUNT_UPDATE_DATA_SIZE`          | Balance change on CALL-like operation    |
| Account update (CREATE/CREATE2)  | `ACCOUNT_UPDATE_DATA_SIZE`          | Successful account creation path         |
| Deployed bytecode                | `code.len()`                        | Successful `CREATE` or `CREATE2`         |

#### Account Update Deduplication

Within a single call frame, a node MUST count a given account update at most once for data-size tracking.
If the same account is updated multiple times within the same call frame — including the caller account across multiple value-transferring sub-calls or creates — subsequent updates in that call frame MUST NOT add additional `ACCOUNT_UPDATE_DATA_SIZE` bytes.

#### Value Self-Transfer Deduplication for Data Size

A value-transferring call whose target equals its caller touches a single account, but the per-call accounting otherwise records a caller-side and a target-side account update.

When a value-transferring call's target equals its caller, a node MUST count the account update once: the target-side `ACCOUNT_UPDATE_DATA_SIZE` charge MUST be suppressed, leaving the caller-side charge (or, at the top level, the transaction-start caller record) as the single charge for the account.
Calls with distinct caller and target, and zero-value calls, are unchanged.

#### Applied-Authorization Narrowing for Data Size

A node MUST count the `ACCOUNT_UPDATE_DATA_SIZE` authority account update only for an _applied_ authorization — one that passes all application gates and therefore writes the authority account.
A node MUST NOT count a skipped authorization toward `authority_update_count`.
The per-record `AUTHORIZATION_DATA_SIZE × authorization_count` contribution counts every authorization in the list, applied or not.

When multiple authorizations target the same authority, a node MUST evaluate them sequentially against the authority nonce and MUST count each applied authorization independently.

#### SELFDESTRUCT Existing-Beneficiary Data Size

A node MUST record `ACCOUNT_UPDATE_DATA_SIZE` bytes of data size for a `SELFDESTRUCT` that transfers a **non-zero** balance to an existing account **distinct** from the executing contract.
A `SELFDESTRUCT` of a zero-balance contract performs no balance credit and MUST record nothing.
A `SELFDESTRUCT` whose target is the executing contract itself credits no other account and MUST record nothing — under [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) it is a balance no-op for a contract not created in the current transaction and burns the balance for one that was, and neither writes a distinct target account.

### KV Updates

#### Definition

A node MUST track KV updates as the number of state-modifying key-value updates attributable to the transaction.

#### Non-Discardable KV Updates

The following contributions MUST be counted at transaction scope and MUST NOT be reverted:

| Operation                  | Count                         |
| -------------------------- | ----------------------------- |
| Transaction caller update  | `1`                           |
| EIP-7702 authority updates | `applied_authorization_count` |

#### Discardable KV Updates

The following contributions MUST be tracked within call frames and MUST be discarded if the call frame reverts:

| Operation        | Count      | Trigger                                                                                |
| ---------------- | ---------- | -------------------------------------------------------------------------------------- |
| SSTORE new write | `+1`       | `original == present && original != new`                                               |
| SSTORE reset     | `-1`       | `original != present && original == new`                                               |
| CREATE/CREATE2   | `1` or `2` | Created account plus caller update if caller not yet counted in the current call frame |
| CALL with value  | `1` or `2` | Callee update plus caller update if caller not yet counted in the current call frame   |

#### Account Update Deduplication

Within a single call frame, a node MUST deduplicate caller account updates for KV-update tracking in the same way it does for data-size tracking.
When a CALL with value or CREATE occurs, the caller's update MUST be counted only if it has not already been counted in the current call frame.

#### Value Self-Transfer Deduplication for KV Updates

When a value-transferring call's target equals its caller, a node MUST count one KV update for the account instead of two, mirroring the data-size deduplication above.
Calls with distinct caller and target, and zero-value calls, are unchanged.

#### Applied-Authorization Narrowing for KV Updates

A node MUST count one authority KV update only for each _applied_ authorization — one that passes the chain-id, nonce, and code gates and writes the authority account — mirroring the data-size narrowing above.
A node MUST NOT count a skipped authorization.
When multiple authorizations target the same authority, each applied authorization MUST be counted independently.

#### SELFDESTRUCT Existing-Beneficiary KV Update

A node MUST record one KV update for a `SELFDESTRUCT` that transfers a **non-zero** balance to an existing account **distinct** from the executing contract, mirroring the data-size rule above.
No state growth is recorded — the account already exists.
A `SELFDESTRUCT` of a zero-balance contract, or to the executing contract itself, MUST record nothing.

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

#### Conditional CREATE State Growth

A node MUST record the `+1` state growth for a `CREATE` / `CREATE2` only when the created address is net-new — that is, the account at the derived address is empty under the state-clear rule when the frame starts.
Deploying to an address that already exists (for example, an address previously funded with a balance) MUST NOT record state growth, mirroring the value-transfer rule that counts only newly materialized accounts.

#### SELFDESTRUCT Refund

When a same-transaction-created contract is destroyed by `SELFDESTRUCT`, the node MUST apply a state-growth refund.
See [SELFDESTRUCT — State Growth Refund](selfdestruct.md#state-growth-refund) for the full specification.

#### Negative Intermediate Values

The state-growth counter MAY become negative during execution.
The reported final state growth for limit enforcement MUST be clamped to a minimum of `0`.

### Post-Execution Fee-Reward Accounting

After execution, the protocol credits transaction fees to the block beneficiary and the protocol fee vaults (the L1-fee, base-fee, and operator-fee recipients).
These writes happen after the per-transaction resource trackers have been finalized.

For each **distinct** fee-recipient account whose balance the fee-reward step changes, a node MUST record one account-info write — `ACCOUNT_UPDATE_DATA_SIZE` bytes of data size and one KV update — in the transaction-persistent lane.
If the write materializes a previously non-existent account (empty before the credit, non-empty after), the node MUST additionally record `+1` state growth.

A fee recipient that coincides with another (for example, a block beneficiary that is also a fee vault) MUST be counted once.
This usage is recorded after the transaction's execution result is final: it feeds the transaction's reported usage and the block-level cumulative counters, and it MUST NOT retroactively change the transaction's outcome — a transaction-level limit crossed only by the fee-reward writes does not fail the transaction.
Transactions that credit no fees (deposit transactions and sandboxed executions) record nothing in this step.

## Constants

| Constant                     | Value | Description                                                                       |
| ---------------------------- | ----- | --------------------------------------------------------------------------------- |
| `BASE_TRANSACTION_DATA_SIZE` | 110   | Fixed estimate of the RLP-encoded transaction envelope excluding calldata         |
| `AUTHORIZATION_DATA_SIZE`    | 101   | Bytes counted per EIP-7702 authorization                                          |
| `ACCOUNT_UPDATE_DATA_SIZE`   | 40    | Bytes counted for an account update or storage-write record in data-size tracking |
| `LOG_TOPIC_DATA_SIZE`        | 32    | Bytes counted per log topic in data-size tracking                                 |
| `LOG_BASE_DATA_SIZE`         | 32    | Bytes counted per emitted log for the log address in data-size tracking           |

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
- [Rex5](../upgrades/rex5.md) — corrected caller-account update deduplication: pre-Rex5, the caller's `ACCOUNT_UPDATE_DATA_SIZE` (data size) and KV-update count were re-charged on every value-transferring sub-call or create from the same parent frame because the caller was never marked as already counted after the first charge; Rex5 marks the caller after the first charge so subsequent operations from the same parent frame do not re-count the caller account. Rex5 also records contract-creation code-deposit compute gas atomically with the deployment commit instead of during post-execution accounting.
- [Rex6](../upgrades/rex6.md) — narrowed the EIP-7702 authority data-size and KV-update charges from every recoverable authorization to only _applied_ authorizations: through Rex5, the `ACCOUNT_UPDATE_DATA_SIZE` and KV update were charged for every authorization with a recoverable authority, including ones later skipped by the chain-id, nonce, or code application gates.
- [Rex6](../upgrades/rex6.md) — corrected two `CREATE`-frame accounting errors: the creator nonce-bump account-info write is booked to the parent frame's discardable lane instead of the child's, so it survives a child-`CREATE` revert correctly, and a creation rejected for call depth or creator balance now keeps the charge where through Rex5 it was discarded with the child's lane; and `CREATE` records `+1` state growth only when the created address is net-new instead of unconditionally.
- [Rex6](../upgrades/rex6.md) — counted the account writes performed by the post-execution fee-reward step toward resource accounting: through Rex5, fee-recipient writes performed after the resource trackers were finalized escaped accounting entirely. The deposit-mint half was already closed in Rex5; Rex6 covers the remaining non-deposit fee-credit paths.
- [Rex6](../upgrades/rex6.md) — counted the account-info write of a `SELFDESTRUCT` balance credit to an already-existing beneficiary: through Rex5 only a `SELFDESTRUCT` that created a new beneficiary was metered, so a balance credit to an existing beneficiary (which does not flow through the frame-initialization or caller-dedup path) recorded nothing.
- [Rex6](../upgrades/rex6.md) — added a per-log data-size base: through Rex5, an empty `LOG0` contributed zero data size because the log address was not counted.
- [Rex6](../upgrades/rex6.md) — deduplicated the value self-transfer account-info write: when a value-transferring call's target equals its caller, the caller-side and target-side writes refer to the same account, but through Rex5 the data-size and KV-update charges were recorded for both, over-counting the one account (it never under-charges). This extends the Rex5 caller-account deduplication above to the self-transfer case.
