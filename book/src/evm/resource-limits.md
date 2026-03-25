# Multidimensional Resource Limits

## Overview

MegaETH enforces resource limits across multiple dimensions, checked in two phases.
Four of these are MegaETH-specific post-execution limits that go beyond standard EVM gas:

1. **[Compute Gas](resource-accounting.md#compute-gas)** — Computational opcode cost
2. **[Data Size](resource-accounting.md#data-size)** — Calldata + logs + storage writes + code deploy + account updates
3. **[KV Updates](resource-accounting.md#kv-updates)** — Storage writes + account modifications (net, with refunds)
4. **[State Growth](resource-accounting.md#state-growth)** — Net new accounts + net new storage slots

## Limit Values

| Resource         | Transaction Limit        | Block Limit     |
| ---------------- | ------------------------ | --------------- |
| Compute Gas      | 200,000,000 (200M)      | Unlimited       |
| Data Size        | 13,107,200 (12.5 MB)    | 13,107,200      |
| KV Updates       | 500,000                  | 500,000         |
| State Growth     | 1,000                    | 1,000           |

For previous limit values (MiniRex configuration), see the [MiniRex](../upgrades/minirex.md) upgrade page.

## Two-Phase Checking

### Phase 1: Pre-Execution (Fast Reject)

Checked before transaction execution:

- **Gas Limit** — Maximum gas declared
- **Transaction Size** — Encoded transaction size
- **DA Size** — Data availability size (deposit transactions exempt)

### Phase 2: Runtime Enforcement (Precise)

Checked during and after execution:

- **Compute Gas**, **Data Size**, **KV Updates**, **State Growth**

## Enforcement Behavior

### Transaction-Level Violations

When any post-execution limit is exceeded during execution:

- Transaction halts with `OutOfGas` error
- Remaining gas is **preserved** (not consumed), refunded to sender
- Transaction **fails** (status=0) but is **still included** in the block
- Failed transactions still count toward block limits

{% hint style="info" %}
**Rex4 (unstable): Call-Frame-Level Violations** — Rex4 adds per-call-frame resource budgets.
Each inner call frame receives `remaining × 98/100` of its parent's remaining budget.
When a call frame exceeds its local budget, it **reverts** with `MegaLimitExceeded(uint8 kind, uint64 limit)` — the parent does **not** halt.
The parent can continue executing; compute gas consumed by reverted frames still counts toward the transaction total.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.
{% endhint %}

### Block-Level Violations

- The last transaction that causes the block to exceed a limit is **allowed to complete and be included**
- Subsequent transactions are rejected before execution
- This maximizes block utilization

## Deposit Transaction Exemptions

Deposit transactions (L1 → L2) are exempt from DA size limits since they are already posted on L1.
They are still subject to all other limits.
