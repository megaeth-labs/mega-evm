# Multidimensional Resource Limits

## Overview

In addition to the standard EVM gas limit (which caps total gas = compute + storage), MegaETH enforces four independent resource ceilings during execution:

1. **[Compute Gas](../glossary.md#compute-gas)** — Computational opcode cost (tracked separately from total gas)
2. **Data Size** — Calldata + logs + storage writes + code deploy + account updates
3. **KV Updates** — Storage writes + account modifications (net, with refunds)
4. **State Growth** — Net new accounts + net new storage slots

These limits are **additional constraints** on top of your transaction's `gas_limit`.
A transaction that stays within its `gas_limit` can still be halted if it exceeds the compute gas ceiling or any other resource limit.
See [Dual Gas Model](dual-gas-model.md) for how `gas_limit`, compute gas, and storage gas relate.

For detailed tracking rules, revert behavior, and what exactly counts toward each dimension, see [Resource Accounting](resource-accounting.md).

## All Resource Limits

MegaETH enforces seven resource limits, split into two phases:

| # | Resource | Phase | Transaction Limit | Block Limit |
| - | -------- | ----- | ----------------- | ----------- |
| 1 | [Gas Limit](https://ethereum.org/en/developers/docs/gas/#block-size) | Pre-execution | Sequencer-configured | `block.gasLimit` from block header |
| 2 | Transaction Size | Pre-execution | Sequencer-configured | Sequencer-configured |
| 3 | [DA Size](https://docs.optimism.io/stack/transactions/transaction-fees#the-l1-data-fee) | Pre-execution | Sequencer-configured | Sequencer-configured |
| 4 | [Compute Gas](../glossary.md#compute-gas) | Runtime | 200,000,000 (200M) | No separate limit (see note) |
| 5 | Data Size | Runtime | 13,107,200 (12.5 MB) | 13,107,200 |
| 6 | KV Updates | Runtime | 500,000 | 500,000 |
| 7 | State Growth | Runtime | 1,000 | 1,000 |

**Notes:**
- **Gas Limit** (1) — Standard [Ethereum gas limit](https://ethereum.org/en/developers/docs/gas/#block-size) semantics. The `gas_limit` field on the transaction must fit within both the per-transaction cap and the block's remaining gas budget.
- **DA Size** (3) — Inherited from the [Optimism rollup model](https://docs.optimism.io/stack/transactions/transaction-fees#the-l1-data-fee). Constrains the compressed size of transaction data posted to L1. [Deposit transactions](https://docs.optimism.io/stack/transactions/deposit-flow) (L1 → L2) are exempt since they are already posted on L1.
- **Compute Gas** (4) — No dedicated block limit because it is already constrained by the block's gas limit (#1), which caps the sum of all transactions' total gas (compute + storage) in a block.
- **Pre-execution limits** (1–3) are **sequencer-configured** — the specific numeric values are set by the sequencer, not hardcoded in the EVM spec.
- **Runtime limits** (4–7) are protocol-level constants. The values above are from the Rex spec onward. For MiniRex values, see the [MiniRex](../upgrades/minirex.md) upgrade page. The Equivalence spec imposes no MegaETH-specific resource limits — only standard Ethereum/Optimism gas limits apply.

## Two-Phase Checking

### Phase 1: Pre-Execution (Fast Reject)

The three pre-execution limits (gas limit, transaction size, DA size) are checked before execution begins.
Transactions that exceed a **transaction-level** pre-execution limit are rejected permanently.
Transactions that exceed a **block-level** pre-execution limit are skipped for the current block but may fit in a future block.

### Phase 2: Runtime Enforcement (Precise)

The four runtime limits (compute gas, data size, KV updates, state growth) are checked during and after execution.

## Enforcement Behavior

### Transaction-Level Violations

When any post-execution limit is exceeded during execution:

- Transaction halts with `OutOfGas` error
- Remaining gas is **preserved** (not consumed), refunded to sender
- Transaction **fails** (status=0) but is **still included** in the block
- Failed transactions still count toward block limits

<details>

<summary>Rex4 (unstable): Call-Frame-Level Violations</summary>

Without per-frame budgets, a single inner call can consume nearly all remaining resources, leaving parent and sibling execution unpredictable.
Rex4 adds per-[call-frame](../glossary.md#call-frame) resource budgets: each inner call frame receives `remaining × 98/100` of its parent's remaining budget.
When a call frame exceeds its local budget, it **reverts** with `MegaLimitExceeded(uint8 kind, uint64 limit)` — the parent does **not** halt.
The parent can continue executing; compute gas consumed by reverted frames still counts toward the transaction total.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.

</details>

### Block-Level Violations

- The last transaction that causes the block to exceed a limit is **allowed to complete and be included**
- Subsequent transactions are rejected before execution
- This maximizes block utilization

## Block Building Workflow

When constructing a block, the sequencer processes transactions from the mempool in order:

1. **Pre-execution check** — For each candidate transaction:
   - Check **transaction-level** limits (all 7 dimensions). If violated → **reject permanently** (transaction is invalid and can never be included in any block).
   - Check **block-level** pre-execution limits (gas, tx size, DA size). If the transaction would cause the block to exceed these limits → **skip** (transaction may fit in a future block, try next candidate).
   - Check if any **block-level** runtime limit (compute gas, data size, KV updates, state growth) was already exceeded by a previous transaction. If so → **skip**.

2. **Execute transaction** — Run the transaction. If any runtime transaction-level limit is exceeded during execution, the transaction **fails** (status=0) but is still included.

3. **Update block counters** — Add the transaction's resource usage to the block's cumulative counters. This transaction is allowed even if it causes cumulative usage to exceed a block-level runtime limit — it is the **last transaction** that pushed the block over the limit.

4. **Include transaction** — Commit the transaction to the block (whether it succeeded or failed).

5. **Continue** — Move to the next candidate. If block-level limits have been exceeded, remaining candidates will be skipped in step 1.

The key difference between pre-execution and runtime block limits: pre-execution limits are checked **before** execution, so transactions that would exceed them are never executed. Runtime limits can only be determined **after** execution, so the transaction that causes the block to exceed is always allowed and included — only subsequent transactions are skipped.

{% hint style="info" %}
**Why failed transactions are still included**: Runtime limits (compute gas, data size, KV updates, state growth) can only be checked during execution — the sequencer cannot know whether a transaction will exceed them without actually running it.
If failed transactions were excluded from the block, an attacker could submit transactions that consume expensive computation, exceed a runtime limit, and get excluded — wasting the sequencer's resources for free.
By including failed transactions on-chain and charging their gas, the attacker pays for the computation they consumed.
{% endhint %}

### Transaction Outcomes Summary

| Outcome | Cause | Receipt | Block Impact |
| ------- | ----- | ------- | ------------ |
| **Success** | Transaction completes within all limits | status=1, includes logs | Counts toward all block limits |
| **Failed** | Transaction-level runtime limit exceeded | status=0, no logs, remaining gas refunded | Still included, counts toward all block limits |
| **Skipped** | Block-level limit already reached | No receipt generated | No impact, deferred to future block |
| **Rejected** | Transaction-level pre-execution limit exceeded | No receipt generated | Permanently invalid, discarded |


