---
description: Rex1 fixes a bug where gas detention from volatile data access leaks across transactions within the same block.
---

# Rex1 Network Upgrade

This page is an informative summary of the Rex1 specification.
For the full normative definition, see the Rex1 spec in the mega-evm repository.

## Summary

Rex1 is a patch release that fixes a single critical bug: the [compute gas](../glossary.md#compute-gas) limit lowered by [volatile data](../glossary.md#volatile-data) access in one transaction persisted to subsequent transactions within the same block.
This caused unrelated transactions to fail unexpectedly.

## What Changed

### Compute Gas Limit Reset Between Transactions

#### Previous behavior
- The [detained](../glossary.md#detained-limit) compute gas limit persists across transactions within the same block.
- A later transaction may inherit a lowered limit from an earlier transaction's volatile data access and halt with `ComputeGasLimitExceeded` even though it never accessed volatile data itself.

For example:
1. TX1 accesses the oracle contract — compute gas limit is lowered to 1M.
2. TX2 is a normal transaction requiring more than 1M compute gas.
3. TX2 fails with `ComputeGasLimitExceeded` despite never accessing volatile data — it inherited TX1's lowered limit.

#### New behavior
- The compute gas limit resets to the configured transaction compute gas limit at the start of each transaction.
- The compute gas usage counter resets to zero at the start of each transaction.
- Gas detention from volatile data access is scoped to the transaction that triggered it and does not affect subsequent transactions.

## Developer Impact

**If you experienced unexpected `ComputeGasLimitExceeded` failures**, this fix resolves the issue.
Transactions no longer inherit gas detention state from earlier transactions in the same block.

No other behavior changes — Rex1 inherits all Rex semantics.

## Safety and Compatibility

All pre-Rex1 behavior is unchanged.
The fix only affects the transaction boundary reset of the compute gas detained limit.
Storage gas economics, transaction intrinsic storage gas, resource limits, CALL-like opcode behavior, and volatile data access detection all remain the same as Rex.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Gas Detention](../evm/gas-detention.md) — background on the gas detention mechanism
