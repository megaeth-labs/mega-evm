# Dual Gas Cost Model

## Dual Gas Model Principle

**Key Concept**: Operations that impose storage costs on nodes (state storage, history data) need to be charged additional storage costs on top of standard EVM costs.

MegaETH introduces a **dual-gas model** that separates gas costs into two categories:

- **Compute Gas**: Standard Optimism EVM computational costs
- **Storage Gas**: Additional costs for persistent storage operations

The **overall gas cost** (i.e., the gas used reported in transaction receipt) is the sum of compute gas cost and storage gas cost.

This separation enables independent pricing of computational work versus storage burden, preventing state bloat and history data growth while maintaining computational efficiency.

## Storage Gas Specification (Rex Hardfork)

### Summary

| Operation                 | Storage Gas Formula       | Notes                                                |
| ------------------------- | ------------------------- | ---------------------------------------------------- |
| **Transaction Intrinsic** | 39,000 (flat)             | All transactions pay this base storage gas           |
| **SSTORE (0 → non-0)**    | 20,000 × (multiplier - 1) | Only for zero-to-non-zero transitions                |
| **Account Creation**      | 25,000 × (multiplier - 1) | Value transfer to empty account                      |
| **Contract Creation**     | 32,000 × (multiplier - 1) | CREATE/CREATE2 opcodes or creation transactions      |
| **Code Deposit**          | 10,000/byte               | Per byte when contract creation succeeds             |
| **LOG Topic**             | 3,750/topic               | Per topic, regardless of revert                      |
| **LOG Data**              | 80/byte                   | Per byte, regardless of revert                       |
| **Calldata (zero)**       | 40/byte                   | Per zero byte in transaction input                   |
| **Calldata (non-zero)**   | 160/byte                  | Per non-zero byte in transaction input               |
| **Floor (zero)**          | 100/byte                  | EIP-7623 floor cost for zero bytes                   |
| **Floor (non-zero)**      | 400/byte                  | EIP-7623 floor cost for non-zero bytes               |

Note that these storage gas costs are added on top of the compute gas (i.e., the vanilla Optimism EVM gas cost) as the total cost of the transaction (i.e., transaction gas used = compute gas used + storage gas used).

### Transaction Intrinsic Storage Gas

All transactions pay 39,000 storage gas on top of the standard 21,000 compute gas for intrinsic cost.

| Spec        | Compute Gas | Storage Gas | Total  |
| ----------- | ----------- | ----------- | ------ |
| **MiniRex** | 21,000      | 0           | 21,000 |
| **Rex**     | 21,000      | 39,000      | 60,000 |

### SSTORE Operations

Applied for an SSTORE opcode when `0 == original value == current value != new value`, where the original, current and new value in a SSTORE operation are defined in EIP-2200. Note that we currently do not give refund for setting `new value` back to `original value` in a transaction execution. A transaction repeatedly setting and resetting a storage slot may induce excessive storage gas cost. Developers are recommended to use transient storage for such use cases.

**Storage gas formula**: `20,000 × (multiplier - 1)`

| Spec        | Formula                     | Multiplier=1 | Multiplier=2 | Multiplier=4 |
| ----------- | --------------------------- | ------------ | ------------ | ------------ |
| **MiniRex** | `2,000,000 × multiplier`    | 2,000,000    | 4,000,000    | 8,000,000    |
| **Rex**     | `20,000 × (multiplier - 1)` | 0            | 20,000       | 60,000       |

**Dynamic scaling**: Multiplier based on SALT bucket capacity
- **Formula**: `multiplier = bucket_capacity / MIN_BUCKET_SIZE`
- **Behavior**: The bucket is a low-level data store container for the storage slot in MegaETH's SALT blockchain state. The multiplier doubles when bucket capacity doubles.

### Account Creation

Applied when an account needs to be created in the blockchain state through value transfer:

- When transferring non-zero ether value to an empty account (as defined in EIP-161), including both transferring on transaction and in the internal message calls via CALL or CALLCODE with non-zero value.

**Storage gas formula**: `25,000 × (multiplier - 1)`

| Spec        | Formula                     | Multiplier=1 | Multiplier=2 | Multiplier=4 |
| ----------- | --------------------------- | ------------ | ------------ | ------------ |
| **MiniRex** | `2,000,000 × multiplier`    | 2,000,000    | 4,000,000    | 8,000,000    |
| **Rex**     | `25,000 × (multiplier - 1)` | 0            | 25,000       | 75,000       |

**Dynamic scaling**: Same bucket multiplier as SSTORE operations
- **Formula**: `multiplier = bucket_capacity / MIN_BUCKET_SIZE`
- **Behavior**: The bucket is a low-level data store container for the new account in MegaETH's SALT blockchain state. The multiplier doubles when bucket capacity doubles.

### Contract Creation

Applied when creating a contract through CREATE/CREATE2 opcodes or contract creation transactions, regardless of whether the contract creation succeeds or not after executing the initcode.

**Storage gas formula**: `32,000 × (multiplier - 1)`

| Spec        | Formula                     | Multiplier=1 | Multiplier=2 | Multiplier=4 |
| ----------- | --------------------------- | ------------ | ------------ | ------------ |
| **MiniRex** | `2,000,000 × multiplier`    | 2,000,000    | 4,000,000    | 8,000,000    |
| **Rex**     | `32,000 × (multiplier - 1)` | 0            | 32,000       | 96,000       |

**Note**: In Rex, contract creation pays both:
1. Contract creation storage gas: `32,000 × (multiplier - 1)`
2. Account creation storage gas: `25,000 × (multiplier - 1)` (if creating a new account)

In MiniRex, contract creation used the same formula as account creation.

### Code Deposit

Applied when a contract creation succeeds, including both contract creation transaction and `CREATE`-like opcodes.

- **Storage gas cost**: 10,000 gas per byte, regardless of whether each byte is zero or not.

This cost is unchanged between MiniRex and Rex.

### LOG Operations

Applied when a `LOG`-like opcode executes, regardless of whether the enclosing message call reverts or not.

- **Per topic**: 3,750 gas per topic
- **Per byte**: 80 gas per byte, regardless of whether each byte is zero or not.

This cost is unchanged between MiniRex and Rex.

### Calldata

Applied to a transaction with input data, regardless of whether the transaction reverts or not.

- **Zero bytes**: 40 gas per byte
- **Non-zero bytes**: 160 gas per byte

This cost is unchanged between MiniRex and Rex.

### EIP-7623 Transaction Floor Cost

The floor cost for calldata (per EIP-7623) is also increased with storage gas:

- **Floor storage cost**:
  - **Zero bytes**: 100 gas per byte
  - **Non-zero bytes**: 400 gas per byte

This cost is unchanged between MiniRex and Rex.

## Historical Reference: MiniRex Hardfork

MiniRex was the first hardfork to introduce the dual gas model. Key differences from Rex:

| Operation                 | MiniRex Formula          | Rex Formula               |
| ------------------------- | ------------------------ | ------------------------- |
| **Transaction Intrinsic** | 0                        | 39,000 (flat)             |
| **SSTORE (0→non-0)**      | `2,000,000 × multiplier` | `20,000 × (multiplier-1)` |
| **Account Creation**      | `2,000,000 × multiplier` | `25,000 × (multiplier-1)` |
| **Contract Creation**     | `2,000,000 × multiplier` | `32,000 × (multiplier-1)` |

The Rex formula `base × (multiplier - 1)` means:
- **When multiplier = 1**: Zero storage gas (standard bucket size, no penalty)
- **When multiplier > 1**: Linear scaling based on bucket capacity expansion

This is a significant change from MiniRex's `base × multiplier` formula, which always charged storage gas regardless of bucket size.
