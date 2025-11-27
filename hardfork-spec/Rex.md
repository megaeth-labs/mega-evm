# Rex Hardfork Specification

## 1. Introduction

Rex is the second hardfork of MegaETH EVM. It modifies MiniRex in four areas:

1. **Storage Gas Economics**: New formulas using `base × (multiplier - 1)` instead of `base × multiplier`
2. **Transaction Intrinsic Storage Gas**: All transactions pay 39,000 additional storage gas
3. **Transaction and Block Limits**: Transaction data and KV update limits increased to match block limits; compute gas limit decreased; state growth limits added
4. **Consistent behavior among CALL-like opcodes**: DELEGATECALL and STATICCALL now enforce 98/100 gas forwarding and oracle access detection

## 2. Comprehensive List of Changes

Rex inherits all MiniRex features and modifications (see [MiniRex.md](MiniRex.md)) with the following changes:

### 2.1 Transaction Intrinsic Storage Gas

All transactions pay 39,000 additional storage gas on top of the standard 21,000 intrinsic gas.

| Spec        | Compute Gas | Storage Gas | Total  |
| ----------- | ----------- | ----------- | ------ |
| **MiniRex** | 21,000      | 0           | 21,000 |
| **Rex**     | 21,000      | 39,000      | 60,000 |

### 2.2 Storage Gas Economics

#### 2.2.1 SSTORE Storage Gas

| Spec        | Formula                     | Multiplier=1 | Multiplier=2 | Multiplier=4 |
| ----------- | --------------------------- | ------------ | ------------ | ------------ |
| **MiniRex** | `2,000,000 × multiplier`    | 2,000,000    | 4,000,000    | 8,000,000    |
| **Rex**     | `20,000 × (multiplier - 1)` | 0            | 20,000       | 60,000       |

Applied when SSTORE executes with `0 == original_value == current_value != new_value`.

#### 2.2.2 Account Creation Storage Gas

| Spec        | Formula                     | Multiplier=1 | Multiplier=2 | Multiplier=4 |
| ----------- | --------------------------- | ------------ | ------------ | ------------ |
| **MiniRex** | `2,000,000 × multiplier`    | 2,000,000    | 4,000,000    | 8,000,000    |
| **Rex**     | `25,000 × (multiplier - 1)` | 0            | 25,000       | 75,000       |

Applied when:

- Value transfer to non-existent account
- CALL or CALLCODE with non-zero value to empty account (EIP-161)
- Contract creation uses separate cost (see 2.2.3)

#### 2.2.3 Contract Creation Storage Gas

| Spec        | Formula                     | Multiplier=1 | Multiplier=2 | Multiplier=4 |
| ----------- | --------------------------- | ------------ | ------------ | ------------ |
| **MiniRex** | Same as account creation    | 2,000,000    | 4,000,000    | 8,000,000    |
| **Rex**     | `32,000 × (multiplier - 1)` | 0            | 32,000       | 96,000       |

Applied when:

- CREATE or CREATE2 opcode
- Contract creation transaction

Contract creation pays both:

1. Contract creation storage gas: `32,000 × (multiplier - 1)`
2. Account creation storage gas: `25,000 × (multiplier - 1)` (if new account)

#### 2.2.4 Storage Gas Summary

| Operation                 | MiniRex     | Rex           |
| ------------------------- | ----------- | ------------- |
| **Transaction Intrinsic** | -           | 39,000 (flat) |
| **SSTORE (0→non-0)**      | 2M × m      | 20k × (m-1)   |
| **Account Creation**      | 2M × m      | 25k × (m-1)   |
| **Contract Creation**     | 2M × m      | 32k × (m-1)   |
| **Code Deposit**          | 10k/byte    | 10k/byte      |
| **LOG Topic**             | 3,750/topic | 3,750/topic   |
| **LOG Data**              | 80/byte     | 80/byte       |
| **Calldata (zero)**       | 40/byte     | 40/byte       |
| **Calldata (non-zero)**   | 160/byte    | 160/byte      |
| **Floor (zero)**          | 100/byte    | 100/byte      |
| **Floor (non-zero)**      | 400/byte    | 400/byte      |

`m` = `bucket_capacity / MIN_BUCKET_SIZE`

### 2.3 DELEGATECALL, STATICCALL, and CALLCODE

In MiniRex, CALLCODE, DELEGATECALL, and STATICCALL bypass 98/100 gas forwarding cap and oracle access detection.

Rex unifies the behaviors of all CALL-like opcodes.

### 2.4 Transaction and Block Limits

#### 2.4.1 Transaction Data and KV Update Limits

**Data Size Limit:**

| Spec        | Transaction Limit | Block Limit |
| ----------- | ----------------- | ----------- |
| **MiniRex** | 3.125 MB (25%)    | 12.5 MB     |
| **Rex**     | **12.5 MB**       | 12.5 MB     |

**KV Update Limit:**

| Spec        | Transaction Limit | Block Limit |
| ----------- | ----------------- | ----------- |
| **MiniRex** | 125,000 (25%)     | 500,000     |
| **Rex**     | **500,000**       | 500,000     |

**Changes:**

- Transaction data limit: 3.125 MB → 12.5 MB (4× increase)
- Transaction KV update limit: 125,000 → 500,000 (4× increase)
- Transaction limits now equal block limits (1:1 ratio)

**Impact:**

- Single transaction can use full block capacity
- Block can still contain multiple small transactions
- Block limits unchanged

#### 2.4.2 Transaction Compute Gas Limit

| Spec        | Transaction Limit      | Block Limit          |
| ----------- | ---------------------- | -------------------- |
| **MiniRex** | 1,000,000,000 (1B)     | Unlimited (u64::MAX) |
| **Rex**     | **200,000,000 (200M)** | Unlimited (u64::MAX) |

**Changes:**

- Transaction compute gas limit: 1B → 200M (5× decrease)
- Block compute gas limit: Unlimited (unchanged)

#### 2.4.3 State Growth Limits

| Spec        | Transaction Limit    | Block Limit          |
| ----------- | -------------------- | -------------------- |
| **MiniRex** | Unlimited (u64::MAX) | Unlimited (u64::MAX) |
| **Rex**     | **1,000**            | **1,000**            |

**What Counts as State Growth:**

- New storage slots (SSTORE 0→non-0) after transaction execution.
- New accounts created
- New contract code deployed

**Enforcement:**

- Transaction-level: Halts with OutOfGas when exceeded
- Block-level: Last transaction exceeding limit included, subsequent rejected

#### 2.4.4 Summary

| Limit            | Level       | MiniRex              | Rex                  | Change      |
| ---------------- | ----------- | -------------------- | -------------------- | ----------- |
| **Data Size**    | Transaction | 3.125 MB             | **12.5 MB**          | 4× increase |
|                  | Block       | 12.5 MB              | 12.5 MB              | -           |
| **KV Updates**   | Transaction | 125,000              | **500,000**          | 4× increase |
|                  | Block       | 500,000              | 500,000              | -           |
| **Compute Gas**  | Transaction | 1,000,000,000        | **200,000,000**      | 5× decrease |
|                  | Block       | Unlimited (u64::MAX) | Unlimited (u64::MAX) | -           |
| **State Growth** | Transaction | Unlimited (u64::MAX) | **1,000**            | New limit   |
|                  | Block       | Unlimited (u64::MAX) | **1,000**            | New limit   |

**Notes:**

- All limits checked during/after execution
- KV updates: all storage writes (including updates to existing slots)
- State growth: only new entries (0→non-0 writes, new accounts, new code)

## 3. Specification Mapping

The semantics of Rex spec are inherited and customized from:

- **Rex** → **MiniRex** → **Optimism Isthmus** → **Ethereum Prague**

## 4. References

- [MiniRex Specification](MiniRex.md)
- [Dual Gas Model](../docs/DUAL_GAS_MODEL.md)
- [Resource Accounting](../docs/RESOURCE_ACCOUNTING.md)
- [Block and Transaction Limits](../docs/BLOCK_AND_TX_LIMITS.md)
- [Oracle Service](../docs/ORACLE_SERVICE.md)
- [Mega System Transactions](../docs/MEGA_SYSTEM_TRANSACTION.md)
