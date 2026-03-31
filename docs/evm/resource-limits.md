---
description: MegaETH per-transaction and per-block resource limits — compute gas, data size, KV updates, and state growth ceilings with enforcement semantics.
spec: Rex3
---

# Multidimensional Resource Limits

This page specifies the limits MegaETH enforces on transaction execution and block construction.
It defines the limit set, the split between pre-execution and runtime enforcement, and the outcomes of transaction-level and block-level violations.

## Motivation

The transaction `gas_limit` alone is insufficient to protect MegaETH from resource-heavy transactions.
A transaction may remain within its total gas budget while still producing excessive state growth, too many key-value updates, or too much execution data.

MegaETH therefore enforces independent resource ceilings in addition to the standard gas limit.
Without those ceilings, a transaction could remain valid under gas accounting while still imposing disproportionate execution, storage, or networking costs on nodes.

Resource limits also need deterministic block-building semantics.
Implementations must agree on which transactions are rejected permanently, which are skipped for the current block, which are included as failed, and when the last transaction that exceeds a block-level runtime limit is still allowed.

## Specification

The named constants referenced in this section are defined later in [Constants](#constants).

### Overview

MegaETH enforces seven distinct resource limits:

1. standard gas limit,
2. transaction encoded size,
3. data-availability size,
4. [compute gas](../glossary.md#compute-gas),
5. data size,
6. KV updates,
7. state growth.

Limits 1–3 are checked before execution.
Limits 4–7 are enforced during execution and accumulated at block level after execution.

For exact counting rules of compute gas, data size, KV updates, and state growth, see [Resource Accounting](resource-accounting.md).
For the relationship between total gas, compute gas, and storage gas, see [Dual Gas Model](dual-gas-model.md).

### Limit Set

A node MUST enforce the following limits:

| Resource | Phase | Transaction Limit | Block Limit |
| -------- | ----- | ----------------- | ----------- |
| [Gas Limit](https://ethereum.org/en/developers/docs/gas/#block-size) | Pre-execution | Sequencer-configured | `block.gasLimit` from block header |
| Transaction Size | Pre-execution | Sequencer-configured | Sequencer-configured |
| [DA Size](https://docs.optimism.io/stack/transactions/transaction-fees#the-l1-data-fee) | Pre-execution | Sequencer-configured | Sequencer-configured |
| [Compute Gas](../glossary.md#compute-gas) | Runtime | `TX_COMPUTE_GAS_LIMIT` | No separate limit |
| Data Size | Runtime | `TX_DATA_LIMIT` | `BLOCK_DATA_LIMIT` |
| KV Updates | Runtime | `TX_KV_UPDATE_LIMIT` | `BLOCK_KV_UPDATE_LIMIT` |
| State Growth | Runtime | `TX_STATE_GROWTH_LIMIT` | `BLOCK_STATE_GROWTH_LIMIT` |

The absence of a separate block-level compute gas limit means that cumulative block compute gas is bounded only indirectly by the block gas limit.

### Pre-Execution Limits

#### Standard Gas Limit

A node MUST apply standard Ethereum gas-limit semantics.
The transaction's `gas_limit` field MUST fit within both the sequencer-configured per-transaction gas cap and the remaining block gas budget from the block header.

#### Transaction Size

A node MUST check the EIP-2718 encoded transaction size before execution.
Transactions that exceed the configured transaction-size limit MUST be rejected permanently.
Transactions that fit the transaction-level size limit but would cause the block's cumulative encoded transaction size to exceed the configured block-level size limit MUST be skipped for the current block.

#### DA Size

A node MUST check the compressed data-availability size of each non-deposit transaction before execution.
Transactions that exceed the configured transaction-level DA size limit MUST be rejected permanently.
Transactions that fit the transaction-level DA size limit but would cause the block's cumulative DA size to exceed the configured block-level DA size limit MUST be skipped for the current block.

Deposit transactions MUST be exempt from DA size limit checks.
Their DA size MAY still be tracked for monitoring purposes.

### Runtime Transaction-Level Limits

A node MUST enforce the following runtime transaction-level limits during execution:

- `TX_COMPUTE_GAS_LIMIT`
- `TX_DATA_LIMIT`
- `TX_KV_UPDATE_LIMIT`
- `TX_STATE_GROWTH_LIMIT`

If any runtime transaction-level limit is exceeded during execution, the transaction MUST:

1. halt,
2. preserve its remaining gas,
3. produce a failed receipt (`status = 0`),
4. and still be included in the block.

The failed transaction's actual resource usage MUST still count toward the block's cumulative resource counters.

### Runtime Block-Level Limits

A node MUST maintain cumulative block counters for:

- data size,
- KV updates,
- state growth,
- and compute gas.

The node MUST update those cumulative counters after transaction execution.

For data size, KV updates, and state growth, the first transaction that causes the cumulative block usage to meet or exceed the block-level limit MUST still be included in the block.
Subsequent candidate transactions MUST be skipped before execution once the block is already at or above the corresponding block-level runtime limit.

Although block compute gas usage MAY be tracked, the protocol does not impose a separate block-level compute gas cap.

### Two-Phase Block Building Workflow

When constructing a block, a node or sequencer MUST process candidate transactions in the following order:

1. Perform pre-execution validation.
2. If a transaction violates a transaction-level pre-execution limit, reject it permanently.
3. If a transaction would exceed a block-level pre-execution limit, skip it for the current block.
4. If any stable block-level runtime limit is already reached or exceeded from prior included transactions, skip later candidate transactions that depend on that resource category.
5. Execute the transaction.
6. If a runtime transaction-level limit is exceeded, include the transaction as failed.
7. Update cumulative block counters with the transaction's actual resource usage.
8. Include the transaction in the block.

### Outcomes

| Outcome | Cause | Receipt | Block Impact |
| ------- | ----- | ------- | ------------ |
| Success | Transaction completes within all applicable limits | `status = 1` | Counts toward all relevant block counters |
| Failed | Runtime transaction-level limit exceeded | `status = 0` | Still included and counts toward all relevant block counters |
| Skipped | Current block cannot admit the transaction without violating a block-level limit | No receipt | Not included; may be reconsidered in a later block |
| Rejected | Transaction-level pre-execution limit exceeded | No receipt | Permanently invalid |

<details>
<summary>Rex4 (unstable): Per-call-frame runtime budgets</summary>

Rex4 adds per-[call-frame](../glossary.md#call-frame) budgets for compute gas, data size, KV updates, and state growth.
Each inner call frame receives `remaining × FRAME_LIMIT_NUMERATOR / FRAME_LIMIT_DENOMINATOR` of its parent call frame's remaining budget.
If a child call frame exceeds its local budget, it MUST revert with `MegaLimitExceeded(uint8 kind, uint64 limit)`.
The parent call frame MAY continue execution.

</details>

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `TX_COMPUTE_GAS_LIMIT` | 200,000,000 | Maximum compute gas per transaction |
| `TX_DATA_LIMIT` | 13,107,200 | Maximum data size per transaction |
| `BLOCK_DATA_LIMIT` | 13,107,200 | Maximum cumulative block data size |
| `TX_KV_UPDATE_LIMIT` | 500,000 | Maximum KV updates per transaction |
| `BLOCK_KV_UPDATE_LIMIT` | 500,000 | Maximum cumulative block KV updates |
| `TX_STATE_GROWTH_LIMIT` | 1,000 | Maximum state growth per transaction |
| `BLOCK_STATE_GROWTH_LIMIT` | 1,000 | Maximum cumulative block state growth |
| `FRAME_LIMIT_NUMERATOR` | 98 | Numerator of per-call-frame budget forwarding in Rex4 |
| `FRAME_LIMIT_DENOMINATOR` | 100 | Denominator of per-call-frame budget forwarding in Rex4 |

## Rationale

**Why split limits into pre-execution and runtime phases?**
Gas limit, encoded transaction size, and DA size are known before execution and can be checked cheaply.
Compute gas, data size, KV updates, and state growth depend on actual execution and therefore cannot be known precisely in advance.

**Why include failed runtime-limited transactions?**
The node must execute a transaction to know whether it exceeds a runtime limit.
If such transactions were excluded from the block, an attacker could force repeated expensive executions at no cost.
Including failed transactions ensures that the sender pays for the resources consumed.

**Why allow the first transaction to push a block over a runtime block limit?**
Runtime block limits are known only after execution.
Allowing the first over-limit transaction to be included maximizes block utilization while preserving deterministic block-building behavior for later transactions.

**Why no separate stable block-level compute gas limit?**
Cumulative block compute gas is already indirectly constrained by the block gas limit.
The stable protocol therefore does not need a second independent block-level compute gas ceiling.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced compute gas, data size, and KV update limits, with transaction-level data and KV limits set to 25% of the corresponding block limits.
- [Rex](../upgrades/rex.md) changed the stable runtime transaction-level limits to `TX_COMPUTE_GAS_LIMIT = 200,000,000`, `TX_DATA_LIMIT = 13,107,200`, `TX_KV_UPDATE_LIMIT = 500,000`, and introduced state-growth limits.
- [Rex3](../upgrades/rex3.md) retained the stable resource-limit set.
- [Rex4](../upgrades/rex4.md) adds unstable per-call-frame runtime budgets.
