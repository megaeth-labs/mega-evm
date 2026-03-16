# Resource Accounting

This page specifies how MegaETH tracks resource usage across the four independent dimensions.

## Revert Behavior

All resource trackers are **frame-aware**: usage within a subcall is discarded if the subcall reverts, and merged into the parent on success.

**Exception**: Compute gas accumulates globally and is **never** reverted, because CPU cycles cannot be undone.

## Compute Gas

Tracks cumulative gas consumed during EVM instruction execution, separate from the standard gas limit.

### What is Tracked

- All gas consumed during instruction execution (SSTORE, CALL, CREATE, arithmetic, etc.)
- Memory expansion costs
- Precompile costs

### What is Not Tracked

- Gas refunds (e.g., from SSTORE refunds)

### Enforcement

When `compute_gas_used > effective_compute_gas_limit`, the transaction halts with `OutOfGas`.
The effective limit may be reduced by [gas detention](gas-detention.md).

## Data Size

Tracks the total bytes of data generated during execution.

### Non-Discardable (Permanent)

Counted at transaction start, never reverted:

| Data Type                      | Size                            |
| ------------------------------ | ------------------------------- |
| Base transaction data          | 110 bytes                       |
| Calldata                       | `tx.input().len()`              |
| Access list                    | Sum of entry sizes              |
| EIP-7702 authorizations        | 101 bytes each                  |
| Caller account update          | 40 bytes                        |
| Authority account updates      | 40 bytes each                   |

### Discardable (Frame-Aware)

Tracked within frames, discarded on revert:

| Data Type              | Size                | Trigger                                      |
| ---------------------- | ------------------- | -------------------------------------------- |
| Log topics             | 32 bytes/topic      | LOG operations                               |
| Log data               | `data.len()`        | LOG operations                               |
| SSTORE (new write)     | 40 bytes            | `original == present && original != new`     |
| SSTORE (reset)         | -40 bytes           | `original != present && original == new`     |
| Account update (CALL)  | 40 bytes            | Balance change from CALL                     |
| Account update (CREATE)| 40 bytes            | Contract creation                            |
| Deployed bytecode      | `code.len()`        | Successful CREATE/CREATE2                    |

## KV Updates

Tracks the number of state-modifying key-value operations.

### Non-Discardable

| Operation                      | Count                 |
| ------------------------------ | --------------------- |
| Transaction caller update      | 1                     |
| EIP-7702 authority updates     | `authorization_count` |

### Discardable

| Operation              | Count | Trigger                                      |
| ---------------------- | ----- | -------------------------------------------- |
| SSTORE (new write)     | +1    | `original == present && original != new`     |
| SSTORE (reset)         | -1    | `original != present && original == new`     |
| CREATE/CREATE2         | 1–2   | Created account + caller if not yet counted  |
| CALL with transfer     | 1–2   | Callee + caller if not yet counted           |

### Account Update Deduplication

Both data size and KV update tracking deduplicate account updates within a call frame.
When a CALL with value or CREATE occurs, the caller's update is counted only if not already marked as updated in the current frame.

## State Growth

Tracks net increase in blockchain state: new accounts and new storage slots.

### Storage Slot Growth

| Original | Present | New   | Growth | Reason                                |
| -------- | ------- | ----- | ------ | ------------------------------------- |
| 0        | 0       | non-0 | +1     | First write to empty slot             |
| 0        | non-0   | 0     | -1     | Clear slot empty at tx start          |
| 0        | non-0   | non-0 | 0      | Already counted when first written    |
| non-0    | any     | any   | 0      | Slot existed at tx start              |

### Net Growth Model

The counter can go negative during execution.
Reported growth is clamped to minimum of zero.

### Per-Frame Limits (Rex4+)

Starting from Rex4, state growth is also enforced at the per-frame level:

- **Top-level frame**: Gets the full TX state growth limit
- **Inner frames**: Each receives `remaining × 98/100` of the parent's remaining budget

When an inner frame exceeds its budget:
- The frame **reverts** (not halt)
- Gas is returned to the parent
- The parent can continue executing
- Revert data is ABI-encoded as `MegaLimitExceeded(uint8 kind, uint64 limit)`
