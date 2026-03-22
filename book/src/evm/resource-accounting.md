# Resource Accounting

This page specifies how MegaETH tracks resource usage across the four independent dimensions.

## Revert Behavior

All resource trackers are **frame-aware**: usage within a subcall is discarded if the subcall reverts, and merged into the parent on success.

**Exception**: Compute gas accumulates globally and is **never** reverted, because CPU cycles cannot be undone.

## Per-Frame Limits (Rex4+)

Starting from Rex4, per-frame budgets apply to all four dimensions: compute gas, data size, KV updates, and state growth.
The top-level frame starts with the full transaction budget for each dimension.
Each inner frame receives `remaining × 98/100` of its parent's remaining budget for each dimension.
If an inner frame exceeds its local budget, that frame reverts with `MegaLimitExceeded(uint8 kind, uint64 limit)`.
The parent frame can continue executing after a child frame reverts.
Transaction-level exceeds still halt the full transaction with `OutOfGas`.
Compute gas consumed by reverted child frames still counts toward total transaction compute gas usage.

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
In Rex4+, frame-local compute gas exceeds cause the current frame to revert according to the per-frame limit rules above.

## Data Size

Tracks the total bytes of data generated during execution.
In Rex4+, data size also uses the per-frame budget rules described above.

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
In Rex4+, KV updates also use the per-frame budget rules described above.

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
In Rex4+, state growth also uses the shared per-frame budget rules described above.
