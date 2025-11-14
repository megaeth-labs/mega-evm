# Dual Gas Cost Model

## Dual Gas Model Principle

**Key Concept**: Operations that impose storage costs on nodes (state storage, history data) needs to be charged additional storage costs on top of standard EVM costs.

MiniRex introduces a **dual-gas model** that separates gas costs into two categories:

- **Compute Gas**: Standard Optimism EVM computational costs
- **Storage Gas**: Additional costs for persistent storage operations

The **overall gas cost** (i.e., the gas used reported in transaction receipt) is the sum of compute gas cost and storage gas cost.

This separation enables independent pricing of computational work versus storage burden, preventing state bloat and history data growth while maintaining computational efficiency.

## Storage Gas Specification

### Summary

1. **SSTORE** (zero → non-zero): 2M × bucket_multiplier
2. **Account creation**: 2M × bucket_multiplier
3. **Code deposit**: 10,000 gas/byte
4. **LOG operations**: 10× multiplier (3,750/topic + 80/byte)
5. **Calldata**: 10× multiplier (40/zero-byte + 160/non-zero-byte)

| Operation               | Storage Gas     | Notes                                                |
| ----------------------- | --------------- | ---------------------------------------------------- |
| **SSTORE (0 → non-0)**  | 2M × multiplier | Only for zero-to-non-zero transitions                |
| **Account creation**    | 2M × multiplier | Contract creation or value transfer to empty account |
| **Code deposit**        | 10,000/byte     | Per byte when contract creation succeeds             |
| **LOG topic**           | 3,750/topic     | Per topic, regardless of revert                      |
| **LOG data**            | 80/byte         | Per byte, regardless of revert                       |
| **Calldata (zero)**     | 40/byte         | Per zero byte in transaction input                   |
| **Calldata (non-zero)** | 160/byte        | Per non-zero byte in transaction input               |
| **Floor (zero)**        | 100/byte        | EIP-7623 floor cost for zero bytes                   |
| **Floor (non-zero)**    | 400/byte        | EIP-7623 floor cost for non-zero bytes               |

Note that these storage gas costs are added on top of the compute gas (i.e., the vanilla Optimism EVM gas cost) as the total cost of the transaction (i.e., transaction gas used = compute gas used + storage gas used).

### SSTORE Operations

Applied for an SSTORE opcode when `0 == original value == current value != new value`, where the original, current and new value in a SSTORE operation are defined in EIP-2200. Note that we currently do not give refund for setting `new value` back to `original value` in a transaction execution. A transaction repeatedly setting and resetting a storage slot may induce excessive storage gas cost. Developers rerecomended to use transient storage for such use cases.

- **Storage gas cost**: 2,000,000 gas × `bucket_multiplier`
- **Dynamic scaling**: Multiplier based on SALT bucket capacity
  - **Formula**: `bucket_multiplier = bucket_capacity / MIN_BUCKET_SIZE`
  - **Behavior**: The bucket is a low-level data store container for the storage slot in MegaETH's SALT blockchain state. The multiplier doubles when bucket capacity doubles.

### Account Creation

Applied when new account needs to be created in the blockchain state:

- When creating a contract, including contract creation transaction and `CREATE`-like opcodes, regardless of whether the contract creation succeeds or not after executing the initcode.
- When transferring non-zero ether value to an empty account (as defined in EIP-161), including both transferring on transaction and in the internal message calls.

- **Storage gas cost**: 2,000,000 gas × bucket_multiplier
- **Dynamic scaling**: Same bucket multiplier as SSTORE operations
  - **Formula**: `bucket_multiplier = bucket_capacity / MIN_BUCKET_SIZE`
  - **Behavior**: The bucket is a low-level data store container for the new account in MegaETH's SALT blockchain state. The multiplier doubles when bucket capacity doubles.

### Code Deposit

Applied when a contract creation succeeds, including both contract creation transaction and `CREATE`-like opcodes.

- **Storage gas cost**: 10,000 gas per byte, regardless of whether each byte is zero or not.

### LOG Operations

Applied when a `LOG`-like opcode excutes, regardless of whether the enclosing message call reverts or not.

- **Per topic**: 3,750 gas per topic
- **Per byte**: 80 gas per byte, regardless of whether each byte is zero or not.

### Calldata

Applied to a transaction with input data, regardless of whether the transaction reverts or not.

- **Zero bytes**: 40 gas per byte
- **Non-zero bytes**: 160 gas per byte

### EIP-7623 Transaction Floor Cost

The floor cost for calldata (per EIP-7623) is also increased with storage gas:

- **Floor storage cost**:
  - **Zero bytes**: 100 gas per byte
  - **Non-zero bytes**: 400 gas per byte
