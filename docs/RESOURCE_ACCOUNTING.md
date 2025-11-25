# Resource Accounting

This document specifies how MegaETH tracks resource usage across four independent dimensions: compute gas, data size, key-value updates, and state growth. Each resource is tracked separately during transaction execution to enforce the multi-dimensional limits defined in [BLOCK_AND_TX_LIMITS.md](./BLOCK_AND_TX_LIMITS.md).

## Overview

MegaETH implements a **multi-dimensional resource tracking system** that monitors four independent resource types during transaction execution:

1. **Compute Gas**: Tracks computational work performed by EVM instructions
2. **Data Size**: Tracks bytes of data that must be transmitted and stored
3. **KV Updates**: Tracks key-value database operations that modify state
4. **State Growth**: Tracks new accounts and storage slots created (net growth)

Each resource is tracked independently, and when any limit is exceeded, the transaction halts with `OutOfGas` (remaining gas is preserved and refunded to sender).

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

Compute gas accumulates globally across all nested call frames within a transaction. It is never reverted, even when a subcall reverts.

### Limit Enforcement

When `compute_gas_used > TX_COMPUTE_GAS_LIMIT`:

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
| `SALT_KEY_SIZE`                      | 8 bytes   | Salt key size for SALT data structure                                   |
| `SALT_VALUE_DELTA_ACCOUNT_INFO_SIZE` | 32 bytes  | Estimated XOR delta size for account info (over-estimate for balance)   |
| `SALT_VALUE_DELTA_STORAGE_SLOT_SIZE` | 32 bytes  | Estimated XOR delta size for storage slot value                         |
| `ACCOUNT_INFO_WRITE_SIZE`            | 40 bytes  | Total size for account info update (8 + 32)                             |
| `STORAGE_SLOT_WRITE_SIZE`            | 40 bytes  | Total size for storage slot write (8 + 32)                              |

### Tracked Data Types

Data size tracking distinguishes between **non-discardable** (permanent) and **discardable** (reverted on frame revert) data:

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

These data sizes are tracked within execution frames and reverted if the frame reverts:

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

### Frame Management

Data size tracking maintains a frame stack to properly handle nested calls and reverts:

1. **Frame Push**: When a CALL or CREATE starts, a new frame is created
2. **Frame Pop (Success)**: Discardable data from child frame merges into parent frame
3. **Frame Pop (Revert)**: Discardable data from child frame is discarded (subtracted from total)

### Smart Accounting for Calls

To avoid double-counting account updates, the tracker uses a `target_updated` flag per frame:

- When a frame transfers value, both caller and callee accounts are marked as updated
- If a subcall transfers value and the caller wasn't already updated in the parent frame, the parent caller gets counted
- This ensures each account is counted at most once per unique state change

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

These operations are tracked within execution frames and reverted if the frame reverts:

| Operation                      | KV Count | Conditions                              | Notes                                                                |
| ------------------------------ | -------- | --------------------------------------- | -------------------------------------------------------------------- |
| **SSTORE (new write)**         | 1        | `original == present && original ≠ new` | First write to slot in transaction                                   |
| **SSTORE (reset to original)** | -1       | `original ≠ present && original == new` | Refund when reset to original value                                  |
| **SSTORE (rewrite)**           | 0        | `original ≠ present && original ≠ new`  | Overwriting already-changed slot                                     |
| **SSTORE (no-op)**             | 0        | `original == new`                       | Writing same value                                                   |
| **CREATE/CREATE2**             | 1 or 2   | -                                       | Created account (1) + caller account if not already updated (0 or 1) |
| **CALL with transfer**         | 1 or 2   | -                                       | Callee account (1) + caller account if not already updated (0 or 1)  |
| **CALL without transfer**      | 0        | -                                       | No state changes                                                     |

### Frame Management

KV update counting uses the same frame stack mechanism as data size tracking:

1. **Frame Push**: When a CALL or CREATE starts, a new frame is created with `target_updated` flag
2. **Frame Pop (Success)**: KV updates from child frame merge into parent frame
3. **Frame Pop (Revert)**: KV updates from child frame are discarded (subtracted from total)

### Smart Accounting for Calls

The same `target_updated` optimization applies:

- CREATE always marks the caller as updated (nonce increment)
- CALL with transfer marks both caller and callee as updated
- Subcalls only count parent caller update if not already marked as updated
- This prevents double-counting the same account within a transaction

### Limit Enforcement

When `kv_updates > TX_KV_UPDATES_LIMIT`:

- Transaction execution halts with `OutOfGas` error
- Remaining gas is preserved (not consumed)
- Gas is refunded to transaction sender

## State Growth Tracking

State growth tracks the net increase in blockchain state by counting new accounts created and new storage slots written. It uses a **net growth model** where clearing storage slots back to zero reduces the count.
Note that the net growth model only applies on transaction level, which means clearing a storage slot created in previous transactions does not decrease the state growth tracked count.

### Tracked Operations

State growth distinguishes between **permanent growth** and **frame-aware growth** (can be reverted):

#### Account Creation (Frame-Aware)

| Operation                                   | Growth Count | Notes                                          |
| ------------------------------------------- | ------------ | ---------------------------------------------- |
| **CREATE/CREATE2**                          | +1           | New contract account created                   |
| **CALL with value to empty account**        | +1           | EIP-161: value transfer creates account        |
| **CALL without value to empty account**     | 0            | EIP-161: no value transfer, no account created |
| **Transaction to empty account with value** | +1           | Transaction-level account creation             |
| **Transaction to existing account**         | 0            | Account already exists                         |
| **Account creation reverted**               | 0            | Frame revert discards the growth               |

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
- Slot starts at 0, write 5 in subcall, subcall reverts: **0** (frame revert discards)

### Net Growth Model

The tracker maintains an internal counter that can become negative during execution:

- **Creating state**: Increments the counter (+1 per account/slot)
- **Clearing state**: Decrements the counter (-1 per slot cleared to zero)
- **Reported growth**: Clamped to minimum of zero

**Example:**

```
Transaction creates 3 new storage slots:    total_growth = +3
Transaction clears 1 slot back to zero:     total_growth = +2
Transaction clears 2 more slots:            total_growth = 0
```

### Frame Management

State growth uses a frame stack to properly handle reverts:

1. **Frame Creation**: When a CALL or CREATE is made, a new frame is pushed
2. **Growth Accumulation**: Growth within a frame is tracked as "discardable"
3. **On Success**: Frame's growth is merged into parent frame
4. **On Revert**: Frame's growth is discarded (subtracted from total)

**Example:**

```
Main transaction starts:                    Frame 0: discardable = 0
Main creates 2 storage slots:               Frame 0: discardable = 2, total = 2
Main calls contract A:                      Frame 1: discardable = 0
Contract A creates 3 storage slots:         Frame 1: discardable = 3, total = 5
Contract A calls contract B:                Frame 2: discardable = 0
Contract B creates 1 storage slot:          Frame 2: discardable = 1, total = 6
Contract B reverts:                         Frame 2 discarded, total = 5
Contract A completes successfully:          Frame 1 merged to Frame 0, total = 5
Transaction completes:                      Final growth = 5
```

### EIP-161 Compliance

The tracker implements EIP-161 account clearing rules for CALL operations:

- **CALL with value to empty account**: Creates account → counts as +1 growth
- **CALL without value to empty account**: Does NOT create account → no growth
- **STATICCALL/DELEGATECALL**: Never create accounts → no growth

### Limit Enforcement

When `state_growth > TX_STATE_GROWTH_LIMIT`:

- Transaction execution halts with `OutOfGas` error
- Remaining gas is preserved (not consumed)
- Gas is refunded to transaction sender
