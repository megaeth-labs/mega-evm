# Resource Accounting

This document specifies how MegaETH tracks resource usage across four independent dimensions: compute gas, data size, key-value updates, and state growth.
Each resource is tracked separately during transaction execution to enforce the multi-dimensional limits defined in [BLOCK_AND_TX_LIMITS.md](./BLOCK_AND_TX_LIMITS.md).

## Overview

MegaETH implements a **multi-dimensional resource tracking system** that monitors four independent resource types during transaction execution:

1. **Compute Gas**: Tracks computational work performed by EVM instructions
2. **Data Size**: Tracks bytes of data that must be transmitted and stored
3. **KV Updates**: Tracks key-value database operations that modify state
4. **State Growth**: Tracks new accounts and storage slots created (net growth)

Each resource is tracked independently, and when any limit is exceeded, the transaction halts with `OutOfGas` (remaining gas is preserved and refunded to sender).

### Revert Behavior

For data size, KV updates, and state growth, resource usage is **frame-aware**: usage tracked within a subcall is discarded if the subcall reverts, and merged into the parent on success.

Compute gas is the exception: it accumulates globally and is **never** reverted, even when a subcall reverts, because CPU cycles cannot be undone.

### Account Update Deduplication

Both data size and KV update tracking deduplicate account updates within a call frame.
When a CALL with value transfer or CREATE occurs, the caller's account update is counted only if it was not already marked as updated in the current frame.
This prevents double-counting the same account's state change within a single frame.

## Compute Gas Tracking

Compute gas tracks the cumulative gas consumed during EVM instruction execution, separate from the standard gas limit.

### Tracked Operations

All gas consumed during transaction execution is tracked, including:

- **EVM instruction costs**: SSTORE, CALL, CREATE, arithmetic operations, etc.
- **Memory expansion costs**: Gas for expanding memory during execution
- **Precompile costs**: Gas consumed by precompile calls
- All other standard EVM gas costs as defined in Optimism Isthmus specification

### Not Tracked

- Gas refunds (e.g., from SELFDESTRUCT or SSTORE refunds)

### Accumulation

Compute gas accumulates globally across all nested call frames within a transaction.
It is never reverted, even when a subcall reverts.

### Gas Detention

The effective compute gas limit may be dynamically lowered when a transaction accesses **volatile data** (block environment fields, oracle storage, beneficiary account).
This mechanism, called gas detention, caps the remaining compute gas budget to reduce conflicts in parallel execution.
See the Rex spec files for the specific detention limits per volatile data category.

### Limit Enforcement

When `compute_gas_used > effective_compute_gas_limit`:

- Transaction execution halts with `OutOfGas` error
- Remaining gas is preserved (not consumed)
- Gas is refunded to transaction sender

## Data Size Tracking

Data size tracks the total bytes of data generated during transaction execution that must be transmitted over the network and stored in the database.

### Constants

The following constants define data sizes for various operations:

| Constant                             | Value     | Description                                                             |
| ------------------------------------ | --------- | ----------------------------------------------------------------------- |
| `BASE_TX_SIZE`                       | 110 bytes | Fixed overhead for each transaction (gas limit, value, signature, etc.) |
| `AUTHORIZATION_SIZE`                 | 101 bytes | Size per EIP-7702 authorization in transaction                          |
| `LOG_TOPIC_SIZE`                     | 32 bytes  | Size per log topic                                                      |
| `ACCOUNT_INFO_WRITE_SIZE`            | 40 bytes  | Total size for account info update (8-byte key + 32-byte value delta)   |
| `STORAGE_SLOT_WRITE_SIZE`            | 40 bytes  | Total size for storage slot write (8-byte key + 32-byte value delta)    |

### Tracked Data Types

Data size tracking distinguishes between **non-discardable** (permanent) and **discardable** (reverted on subcall revert) data:

#### Non-discardable Data (Permanent)

These data sizes are counted at transaction start and never reverted:

| Data Type                              | Size (Bytes)                          | Notes                                          |
| -------------------------------------- | ------------------------------------- | ---------------------------------------------- |
| **Base transaction data**              | 110                                   | Fixed overhead per transaction                 |
| **Calldata**                           | `tx.input().len()`                    | Transaction input data                         |
| **Access list**                        | Sum of `access.size()` for each entry | EIP-2930 access list entries                   |
| **EIP-7702 authorizations**            | `authorization_count × 101`           | Authorization list in transaction              |
| **Transaction caller account update**  | 40                                    | Always counted at transaction start            |
| **EIP-7702 authority account updates** | `authorization_count × 40`            | One update per authority in authorization list |

#### Discardable Data (Frame-Aware)

These data sizes are tracked within execution frames and discarded if the frame reverts:

| Data Type                      | Size (Bytes)          | Conditions                              | Notes                                    |
| ------------------------------ | --------------------- | --------------------------------------- | ---------------------------------------- |
| **Log topics**                 | `num_topics × 32`     | Per LOG operation                       | Topics data                              |
| **Log data**                   | `data.len()`          | Per LOG operation                       | Event data payload                       |
| **SSTORE (new write)**         | 40                    | `original == present && original ≠ new` | First write to slot in transaction       |
| **SSTORE (reset to original)** | -40                   | `original ≠ present && original == new` | Refund when reset to original value      |
| **SSTORE (rewrite)**           | 0                     | `original ≠ present && original ≠ new`  | Overwriting already-changed slot         |
| **SSTORE (no-op)**             | 0                     | `original == new`                       | Writing same value                       |
| **Account update from CALL**   | 40                    | Per account with balance change         | Caller and/or callee account             |
| **Account update from CREATE** | 40                    | Per account                             | Created account (caller may also update) |
| **Deployed bytecode**          | `contract_code.len()` | On successful CREATE/CREATE2            | Actual deployed contract size            |

### Limit Enforcement

When `data_size > TX_DATA_SIZE_LIMIT`:

- Transaction execution halts with `OutOfGas` error
- Remaining gas is preserved (not consumed)
- Gas is refunded to transaction sender

## KV Updates Tracking

KV updates track the number of key-value database operations that modify state during transaction execution.

### Tracked Operations

#### Non-discardable Operations (Permanent)

These operations are counted at transaction start and never reverted:

| Operation                              | KV Count              | Notes                                            |
| -------------------------------------- | --------------------- | ------------------------------------------------ |
| **Transaction caller account update**  | 1                     | Always counted at transaction start (nonce bump) |
| **EIP-7702 authority account updates** | `authorization_count` | One update per authority in authorization list   |

#### Discardable Operations (Frame-Aware)

These operations are tracked within execution frames and discarded if the frame reverts:

| Operation                      | KV Count | Conditions                              | Notes                                                                |
| ------------------------------ | -------- | --------------------------------------- | -------------------------------------------------------------------- |
| **SSTORE (new write)**         | 1        | `original == present && original ≠ new` | First write to slot in transaction                                   |
| **SSTORE (reset to original)** | -1       | `original ≠ present && original == new` | Refund when reset to original value                                  |
| **SSTORE (rewrite)**           | 0        | `original ≠ present && original ≠ new`  | Overwriting already-changed slot                                     |
| **SSTORE (no-op)**             | 0        | `original == new`                       | Writing same value                                                   |
| **CREATE/CREATE2**             | 1 or 2   | -                                       | Created account (1) + caller account if not already updated (0 or 1) |
| **CALL with transfer**         | 1 or 2   | -                                       | Callee account (1) + caller account if not already updated (0 or 1)  |
| **CALL without transfer**      | 0        | -                                       | No state changes                                                     |

### Limit Enforcement

When `kv_updates > TX_KV_UPDATES_LIMIT`:

- Transaction execution halts with `OutOfGas` error
- Remaining gas is preserved (not consumed)
- Gas is refunded to transaction sender

## State Growth Tracking

State growth tracks the net increase in blockchain state by counting new accounts created and new storage slots written.
It uses a **net growth model** where clearing storage slots back to zero reduces the count.
Note that the net growth model only applies on transaction level, which means clearing a storage slot created in previous transactions does not decrease the state growth count.

### Tracked Operations

#### Account Creation (Frame-Aware)

| Operation                                   | Growth Count | Notes                                          |
| ------------------------------------------- | ------------ | ---------------------------------------------- |
| **CREATE/CREATE2**                          | +1           | New contract account created                   |
| **CALL with value to empty account**        | +1           | EIP-161: value transfer creates account        |
| **CALL without value to empty account**     | 0            | EIP-161: no value transfer, no account created |
| **Transaction to empty account with value** | +1           | Transaction-level account creation             |
| **Transaction to existing account**         | 0            | Account already exists                         |
| **Account creation reverted**               | 0            | Subcall revert discards the growth             |

#### Storage Slot Creation (Frame-Aware)

State growth tracks transitions based on the **original value** (at transaction start), **present value** (before SSTORE), and **new value** (being written):

| Original | Present | New   | Growth Change | Reason                                     |
| -------- | ------- | ----- | ------------- | ------------------------------------------ |
| zero     | zero    | non-0 | **+1**        | First write to empty slot                  |
| zero     | non-0   | zero  | **-1**        | Clear slot that was empty at tx start      |
| zero     | non-0   | non-0 | 0             | Already counted when first written         |
| non-0    | any     | any   | 0             | Slot existed at tx start, no growth change |

**Examples:**

- Slot starts at 0, write 5: **+1** (new storage created)
- Slot starts at 0, write 5, write 10: **+1** (only counted once)
- Slot starts at 0, write 5, write 0: **0** (created then cleared in same tx)
- Slot starts at 5, write 10: **0** (slot already existed at tx start)
- Slot starts at 0, write 5 in subcall, subcall reverts: **0** (revert discards)

### Net Growth Model

The internal counter can become negative during execution:

- **Creating state**: Increments the counter (+1 per account/slot)
- **Clearing state**: Decrements the counter (-1 per slot cleared to zero)
- **Reported growth**: Clamped to minimum of zero

**Example:**

```
Transaction creates 3 new storage slots:    total_growth = +3
Transaction clears 1 slot back to zero:     total_growth = +2
Transaction clears 2 more slots:            total_growth = 0
```

### Revert Behavior

State growth within a subcall is fully discardable:

- **On success**: The subcall's growth merges into the parent.
- **On revert**: The subcall's growth is discarded.

**Example:**

```
Main creates 2 storage slots:               total = 2
Main calls contract A:
  Contract A creates 3 storage slots:       total = 5
  Contract A calls contract B:
    Contract B creates 1 storage slot:      total = 6
    Contract B reverts:                     total = 5 (B's growth discarded)
  Contract A completes successfully:        total = 5 (A's growth merged)
Transaction completes:                      Final growth = 5
```

### EIP-161 Compliance

EIP-161 account clearing rules apply for CALL operations:

- **CALL with value to empty account**: Creates account → counts as +1 growth
- **CALL without value to empty account**: Does NOT create account → no growth
- **STATICCALL/DELEGATECALL**: Never create accounts → no growth

### Limit Enforcement

When `state_growth > TX_STATE_GROWTH_LIMIT`:

- Transaction execution halts with `OutOfGas` error
- Remaining gas is preserved (not consumed)
- Gas is refunded to transaction sender

### Per-Frame State Growth Limits (Rex4+)

Starting from Rex4, state growth is also enforced at the **per-frame** level to prevent a single inner call from consuming the entire transaction's state growth budget.

#### Budget Allocation

- **Top-level frame**: Gets the full TX state growth limit (no reduction).
- **Inner frames**: Each receives `remaining * 98 / 100` of the parent's remaining budget, where `remaining` is the parent's limit minus the net growth accumulated since the parent's entry.

#### Limit Exceeding Semantics

When an inner frame exceeds its per-frame budget:

- The frame's result is changed to **Revert** (not Halt).
- Gas is returned to the parent frame.
- The child's state growth is discarded (standard revert behavior).
- The parent can continue executing after the reverted child call.

This is different from TX-level limit enforcement, which halts the entire transaction.

#### Example

```
TX state growth limit: 1000
Top-level frame limit: 1000

Top-level calls Child A:
  Child A budget = 1000 * 98/100 = 980
  Child A creates 500 slots

  Child A calls Grandchild B:
    Remaining = 980 - 500 = 480
    Grandchild B budget = 480 * 98/100 = 470
    Grandchild B creates 471 slots → exceeds 470 → reverted
    Child A continues with its remaining budget

Top-level calls Child C (after Child A):
  Growth so far = 500 (Grandchild B's growth was discarded)
  Remaining = 1000 - 500 = 500
  Child C budget = 500 * 98/100 = 490
```
