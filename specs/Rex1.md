# Rex1 Specification

## Abstract

Rex1 is the first patch to the Rex hardfork.
It fixes a bug where the compute gas limit lowered by volatile data access in one transaction persists to subsequent transactions within the same block.
All Rex semantics are preserved unless explicitly changed below.

## Changes

### 1. Compute gas limit reset between transactions

#### Motivation

When a transaction accesses volatile data (e.g., the oracle contract or block environment fields), the compute gas limit is lowered for that transaction to restrict post-access computation.
In Rex, this lowered limit persists across transactions executed on the same EVM instance within a block, causing subsequent transactions to fail unexpectedly.

#### Semantics

Previous behavior:
- The detained compute gas limit persists across transactions within the same block.
- A later transaction may inherit a lowered limit from an earlier transaction's volatile data access and halt with `ComputeGasLimitExceeded` even though it never accessed volatile data itself.

New behavior:
- The compute gas limit MUST reset to the configured transaction compute gas limit at the start of each transaction.
- The compute gas usage MUST reset to zero at the start of each transaction.
- Gas detention from volatile data access MUST be scoped to the transaction that triggered it and MUST NOT affect subsequent transactions.

## Invariants

- `I-1`: Stable Rex semantics MUST remain unchanged except for the compute gas limit reset.
- `I-2`: Volatile data access gas detention MUST NOT affect subsequent transactions in the same block.

## Inheritance

Rex1 inherits Rex except for the deltas defined in `Changes`.
Semantic lineage: `Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague`.

## References

- [Rex Specification](Rex.md)
- [Rex2 Specification](Rex2.md)
- [MiniRex Specification](MiniRex.md)
- [Rex1 Behavior Details (Informative)](impl/Rex1-Behavior-Details.md)
- [Rex1 Implementation References (Informative)](impl/Rex1-Implementation-References.md)
- [Block and Transaction Limits](../docs/BLOCK_AND_TX_LIMITS.md)
- [Oracle Service](../docs/ORACLE_SERVICE.md)
