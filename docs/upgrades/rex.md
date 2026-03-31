---
description: Rex network upgrade — revised storage gas economics, 39K intrinsic storage gas, and state growth tracking.
---

# Rex Network Upgrade

This page is an informative summary of the Rex specification.
For the full normative definition, see the Rex spec in the mega-evm repository.

## Summary

Rex is the first major upgrade after MiniRex.
It significantly refines the [storage gas](../glossary.md#storage-gas) economics introduced in MiniRex, changing the formula from `base × multiplier` to `base × (multiplier − 1)` so that operations in uncrowded state regions incur **zero storage gas**.
A flat 39,000 storage gas is added to every transaction's intrinsic cost to account for per-transaction state overhead.

Rex also fixes inconsistencies in MiniRex where CALLCODE, DELEGATECALL, and STATICCALL bypassed the 98/100 gas forwarding cap and [oracle](../system-contracts/oracle.md) access detection.
All CALL-like opcodes now behave consistently.
A new **[state growth](../evm/resource-accounting.md#state-growth)** [resource dimension](../glossary.md#resource-dimension) is introduced with per-transaction and per-block limits to prevent unbounded state expansion.

## What Changed

### Transaction Intrinsic Storage Gas

#### Previous behavior
- Transaction intrinsic gas is 21,000 (compute gas only, no storage gas component).

#### New behavior
- Every transaction pays 39,000 additional storage gas on top of the standard 21,000 compute intrinsic gas.
- Total intrinsic gas becomes 60,000 (21,000 compute + 39,000 storage).

### Storage Gas Economics

#### Previous behavior
- SSTORE (0→non-0): `2,000,000 × multiplier`
- Account creation: `2,000,000 × multiplier`
- Contract creation: `2,000,000 × multiplier`
- At minimum bucket size ([multiplier](../glossary.md#multiplier) = 1), storage gas is still charged at the full base cost.

#### New behavior
- SSTORE (0→non-0): `20,000 × (multiplier − 1)`
- Account creation: `25,000 × (multiplier − 1)`
- Contract creation: `32,000 × (multiplier − 1)`
- At `multiplier = 1` (minimum bucket size), all three operations cost **zero storage gas**.
- Contract creation pays only its own storage gas (32,000 × (multiplier − 1)); the account creation storage gas (25,000) is not charged on top.
- All other storage gas operations (code deposit, LOG, calldata) remain unchanged from MiniRex.

#### Example

A contract writes a new storage slot in an uncrowded state region (bucket capacity = [MIN_BUCKET_SIZE](../glossary.md#min_bucket_size), so multiplier = 1).
Under MiniRex, this SSTORE costs 2,000,000 storage gas.
Under Rex, it costs `20,000 × (1 − 1)` = 0 storage gas.
Storage writes in uncrowded regions are effectively free from the storage gas perspective, while crowded regions still pay proportionally.

### Consistent Behavior Among CALL-Like Opcodes

#### Previous behavior
- Only CALL enforces 98/100 gas forwarding.
- Only CALL triggers oracle access detection.
- CALLCODE, DELEGATECALL, and STATICCALL bypass both.

#### New behavior
- CALLCODE, DELEGATECALL, and STATICCALL all enforce the 98/100 gas forwarding cap.
- STATICCALL triggers oracle access detection when targeting the [oracle contract](../system-contracts/oracle.md) (consistent with CALL).
- CALLCODE and DELEGATECALL do not trigger oracle access detection — their `target_address` equals the caller's address, so they never constitute a direct oracle read.

| Opcode         | 98/100 gas forwarding | Oracle access detection |
| -------------- | --------------------- | ----------------------- |
| CALL           | MiniRex+              | MiniRex+                |
| STATICCALL     | Rex+                  | Rex+                    |
| DELEGATECALL   | Rex+                  | Never                   |
| CALLCODE       | Rex+                  | Never                   |

### Transaction and Block Limits

#### Previous behavior
- Data size: 3.125 MB per transaction, 12.5 MB per block.
- KV updates: 125,000 per transaction, 500,000 per block.
- Compute gas: 1B per transaction.
- State growth: unlimited.

#### New behavior
- Data size: **12.5 MB** per transaction (4× increase, now equals block limit).
- KV updates: **500,000** per transaction (4× increase, now equals block limit).
- Compute gas: **200M** per transaction (5× decrease).
- State growth: **1,000** per transaction and per block (new limit).

| Limit          | Level       | MiniRex     | Rex          |
| -------------- | ----------- | ----------- | ------------ |
| Data size      | Transaction | 3.125 MB    | **12.5 MB**  |
| KV updates     | Transaction | 125,000     | **500,000**  |
| Compute gas    | Transaction | 1B          | **200M**     |
| State growth   | Transaction | Unlimited   | **1,000**    |
| State growth   | Block       | Unlimited   | **1,000**    |

[State growth](../evm/resource-accounting.md#state-growth) counts net new entries: new storage slots (SSTORE 0→non-0) and new accounts (CREATE, CREATE2, or CALL with value to empty account).

## Developer Impact

**Your transaction gas cost structure has changed.**
Every transaction now pays a minimum of 60,000 gas (21,000 compute + 39,000 storage) before any execution.
If your application sends many small transactions, factor in the increased base cost.

**Storage gas is much cheaper for fresh state.**
If your contracts write to uncrowded storage regions, you'll see significantly lower storage gas costs compared to MiniRex.
The `base × (multiplier − 1)` formula means fresh storage (multiplier = 1) is free.

**The compute gas limit dropped from 1B to 200M.**
Contracts that previously relied on the higher limit may need to optimize their compute usage or split operations across multiple transactions.

**State growth is now capped at 1,000 per transaction.**
If your contract creates many new accounts or storage slots in a single transaction, ensure it stays within this limit.

**All CALL-like opcodes now behave consistently.**
If your contracts used DELEGATECALL or STATICCALL expecting 63/64 gas forwarding or expecting to avoid oracle detection, they now follow the same 98/100 and oracle detection rules as CALL.

## Safety and Compatibility

All pre-Rex (MiniRex) behavior is unchanged for transactions running on MiniRex specs.

The increased transaction limits for data size and KV updates (now equal to block limits) give individual transactions more headroom, but the block-level limits remain the same.

State growth violations halt the transaction with `OutOfGas` and refund remaining gas — consistent with other resource limit violations.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Dual Gas Model](../evm/dual-gas-model.md) — storage gas formulas and SALT mechanics
- [Resource Limits](../evm/resource-limits.md) — limit values and enforcement behavior
