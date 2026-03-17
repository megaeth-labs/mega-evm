# Rex1 Behavior Details

This document is informative.
Normative semantics are defined in [Rex1 Specification](../Rex1.md).
If this document conflicts with the normative spec text, the normative spec wins.

## 1. Compute gas limit reset between transactions

### Background

MegaETH's gas detention mechanism lowers the compute gas limit when a transaction accesses volatile data:
- Oracle contract access lowers the limit to 1,000,000 (1M).
- Block environment access lowers the limit to 20,000,000 (20M).

Prior to Rex1, the EVM instance reused across transactions in a block did not reset this lowered limit.
This meant the lowered limit leaked into subsequent transactions.

### The bug scenario

1. TX1 accesses the oracle contract. Compute gas limit is lowered to 1M.
2. TX2 is a normal transaction requiring more than 1M compute gas.
3. TX2 fails with `ComputeGasLimitExceeded` despite never accessing volatile data — it inherited TX1's lowered limit.

### What gets reset

At the start of each transaction, when Rex1 is enabled:
- The detained compute gas limit resets to the configured transaction compute gas limit.
- The compute gas usage counter resets to zero.

All other limit trackers (data size, KV updates, state growth) also reset between transactions, but this was not affected by the bug — only the compute gas detained limit was problematic.

### What remains unchanged

Rex1 does not change any other behavior from Rex:
- Storage gas economics, transaction intrinsic storage gas, transaction and block limits, CALL-like opcode behavior, and volatile data access detection all remain the same.

## References

- [Rex1 Specification](../Rex1.md)
- [Rex1 Implementation References](Rex1-Implementation-References.md)
