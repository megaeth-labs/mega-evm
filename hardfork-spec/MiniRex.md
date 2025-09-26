# MiniRex Hardfork Specification

## Summary

| Area / Opcode | Standard EVM (Prague) | Mega EVM (MiniRex) |
|---|---|---|
| **SSTORE (0 → non-0)** | 20,000 (per EIP-2200) | **2,000,000 × bucket multiplier** |
| **SSTORE (other cases)** | EIP-2200 rules (reset, same, refund, warm read) | Same as standard |
| **New account (CALL → empty, tx callee, CREATE target)** | 25,000 (NEWACCOUNT) | **2,000,000 × bucket multiplier** |
| **Contract creation (CREATE/CREATE2)** | 25,000 (new account) + code deposit gas | **2,000,000 × multiplier (new acct)** + **2,000,000 (codehash)** + code deposit gas |
| **Code deposit (per byte)** | 200 gas/byte | **62,500 gas/byte** |
| **Initcode max size** | 49152 bytes (per EIP-3860) | **MAX_CONTRACT_SIZE (512 KiB) + 24 KiB** |
| **CREATE/CALL forwarding fraction** | 63/64 of gas left | **98/100** of gas left |
| **LOG per topic** | 375 gas/topic | **3,750 gas/topic (×10)** |
| **LOG per byte** | 8 gas/byte | **80 gas/byte (×10)** |
| **Calldata per-byte** | 4 gas (zero byte), 16 gas (non-zero byte) | **40 gas (zero byte), 160 gas (non-zero byte)** |
| **SELFDESTRUCT** | Disabled refunds post-Shanghai but still available | **Instruction removed** |
| **Per-tx data limit** | none | **3.125 MiB (25% of 12.5 MiB block limit)** across calldata + logs + return + initcode |
| **Per-tx KV update limit** | none | **12,500 updates (25% of block limit)** |

## 1. Introduction

The **MiniRex** hardfork represents a critical evolution of the MegaETH EVM designed to address the unique economic and technical challenges arising from MegaETH's distinctive architecture. Unlike traditional Ethereum networks, MegaETH operates with an extremely low minimum base fee (0.001 gwei vs Ethereum's 0.5 gwei) and exceptionally high transaction gas limits (up to 10 billion gas), creating unprecedented opportunities for both innovation and abuse.

While MegaETH's low fees make computation extremely affordable—enabling complex applications previously economically infeasible—they also create severe vulnerabilities under standard EVM semantics. Operations that impose storage costs on nodes become dramatically underpriced, potentially leading to:

- **Unsustainable state bloat** through cheap storage writes and account creation
- **Archive node data explosion** via excessive logging and transaction data
- **Reintroduced call depth attacks** due to high gas limits bypassing EIP-150 protections

To ensure network stability and sustainable growth of MegaETH, the MiniRex hardfork introduces:

1. **Multi-dimensional Resource Limits**: Novel constraints on data size and key-value updates enable safe removal of block gas limit
2. **Strategic Gas Cost Increases**: Storage operations (SSTORE, account creation, logging) see substantial gas cost increases to reflect their true burden on blockchain nodes

This document details all semantic changes, their rationale, and implementation requirements for the MiniRex hardfork activation.


## 3. Comprehensive List of Changes

### 3.1 Contract Size Limits

**Increased Maximum Contract Size:**

- **Standard EVM (EIP-170)**: 24,576 bytes (24 KB)
- **MiniRex**: 524,288 bytes (512 KB)
- **Location**: `constants::mini_rex::MAX_CONTRACT_SIZE = 512 * 1024`
- **Purpose**: Feature-rich applications require larger bytecode

**Increased Maximum Initcode Size:**

- **Standard EVM (EIP-3860)**: 49,152 bytes (48 KB)
- **MiniRex**: 536,576 bytes (512 KB + 24 KB)
- **Location**: `constants::mini_rex::MAX_INITCODE_SIZE = MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE`

### 3.2 SELFDESTRUCT Opcode Deprecation

**Complete Disabling of SELFDESTRUCT:**

- **Behavior**: SELFDESTRUCT opcode now halts execution with `InvalidFEOpcode` error
- **Implementation**: `self.inner.insert_instruction(SELFDESTRUCT, control::invalid)`

### 3.3 Gas Cost Increases

#### 3.3.1 Storage Operations (SSTORE)

**Zero-to-Non-Zero Writes:**
- **MiniRex Cost**: 2,000,000 gas (base) × bucket multiplier
- **Standard EVM**: 20,000 gas (per EIP-2200)
- **Bucket Multiplier**: Dynamic scaling factor based on SALT bucket capacity
  - **Formula**: `bucket_capacity / MIN_BUCKET_SIZE`
  - **Behavior**: Multiplier doubles when bucket capacity doubles
  - **Purpose**: Prevent key collision attacks on SALT buckets
- **Constant**: `constants::mini_rex::SSTORE_SET_GAS = 2_000_000`
- **Purpose**: Prevent unsustainable state bloat

**Other SSTORE Cases:**
- Follow standard EIP-2200 rules (reset, same value, refunds, warm reads)

#### 3.3.2 Account Creation

**New Account Gas:**
- **MiniRex Cost**: 2,000,000 gas (base) × bucket multiplier
- **Standard EVM**: 25,000 gas (NEWACCOUNT)
- **Dynamic Scaling**: Same multiplier as SSTORE operations
- **Constant**: `constants::mini_rex::NEW_ACCOUNT_GAS = 2_000_000`
- **Purpose**: Prevent unsustainable state bloat

**Contract Creation (CREATE/CREATE2):**
- **Additional Cost**: 2,000,000 gas (fixed, on top of new account cost to account for codehash)
- **Constant**: `constants::mini_rex::CREATE_GAS = 2_000_000`
- **Total**: `(NEW_ACCOUNT_GAS × multiplier) + CREATE_GAS + code_deposit_gas`
- **Purpose**: Prevent unsustainable state bloat

#### 3.3.3 Code Deposit

**Per-Byte Cost:**
- **MiniRex Cost**: 62,500 gas per byte
- **Standard EVM**: 200 gas per byte
- **Calculation**: `CREATE_GAS / 32 = 2_000_000 / 32 = 62,500`
- **Purpose**: Prevent unsustainable state bloat

#### 3.3.4 Logging Operations

**Log Data:**
- **MiniRex Cost**: 80 gas per byte (10× increase)
- **Standard EVM**: 8 gas per byte
- **Constant**: `LOG_DATA_GAS = LOGDATA × 10`

**Log Topics:**
- **MiniRex Cost**: 3,750 gas per topic (10× increase)
- **Standard EVM**: 375 gas per topic
- **Constant**: `LOG_TOPIC_GAS = LOGTOPIC × 10`
- **Purpose**: Prevent unsustainable history data growth

#### 3.3.5 Call Data

**Transaction Data:**
- **Zero Bytes**: 40 gas (10× increase from 4 gas)
- **Non-Zero Bytes**: 160 gas (10× increase from 16 gas)
- **EIP-7623 Floor Cost**: 10× increase for transaction data floor
- **Constants**:
  - `CALLDATA_STANDARD_TOKEN_ADDITIONAL_GAS`
  - `CALLDATA_STANDARD_TOKEN_ADDITIONAL_FLOOR_GAS`
- **Purpose**: Prevent unsustainable history data growth

#### 3.3.6 Gas Forwarding

MegaETH's extremely high transaction gas limits (e.g., 10 billion gas) reintroduce call depth attacks that EIP-150 solved for Ethereum. With 63/64 gas forwarding: `10^10 × (63/64)^1024 ≈ 991 gas` remains after 1,024 calls, enough to make one more call and exceed the stack depth limit.

**Gas Forwarding Rule:**
- **MiniRex**: 98/100 rule - forwards 98/100 of remaining gas to subcalls
- **Standard EVM**: 63/64 rule - forwards 63/64 of remaining gas to subcalls (per EIP-150)
- **Result**: `10^10 × (98/100)^1024 ≈ 10 gas` after 1,024 calls
- **Affected Operations**: CALL, CALLCODE, DELEGATECALL, STATICCALL, CREATE, CREATE2
- **Purpose**: Restore call depth attack protection for high-gas environments

### 3.4. Multi-dimensional Resource Limits

#### 3.4.1 Rationale

Traditional blockchain networks rely on a single block gas limit to constrain all types of resources—computation, storage operations, and network bandwidth. While this unified approach provides simplicity, it creates fundamental scaling limitations. Each resource type has different characteristics and bottlenecks, yet the gas limit forces them to scale together. When operators want to increase one resource capacity (such as computation), they must proportionally increase all others, which is not always possible.

For example, suppose developers implement a clever optimization in the EVM execution engine that makes it twice as fast. To take advantage of this computational improvement, operators would want to double the maximum computation allowed in a block. However, this requires doubling the entire block gas limit. As a result, the maximum storage operations and network bandwidth consumption per block also double. Such changes may compromise network stability, as any node meeting minimum hardware requirements must be able to keep up with the sequencer.

MegaETH's architecture exemplifies this challenge. The hyper-optimized sequencer possesses exceptionally high computation capacity, capable of processing far more transactions than traditional networks. However, replica nodes still face the same fundamental constraints: they must receive state updates over the network and apply database modifications to update their state roots accordingly. Under a traditional gas limit model, these network and storage constraints artificially bottleneck the sequencer's computational capabilities.

To solve this problem, MiniRex replaces the monolithic block gas limit with two targeted resource constraints:

- **Data Size Limit**: Constrains the amount of data that must be transmitted over the network during live synchronization, preventing history bloat and ensuring replica nodes can maintain pace with data transmission requirements.

- **KV Updates Limit**: Constrains the number of key-value updates that must be applied to the local database and incorporated into state root calculations, ensuring replica nodes can process state changes efficiently.

This multi-dimensional approach enables the sequencer to safely create blocks containing extensive computation, provided they satisfy both targeted constraints. The result is a more flexible and efficient resource allocation model that maximizes MegaETH's computational advantages while maintaining network stability.

#### 3.4.2 Block-Level Limits

These limits define the maximum resources that can be consumed across all transactions within a single block:

- **Maximum Block Data**: 12.5 MB
  - **Constant**: `BLOCK_DATA_LIMIT = 12 * 1024 * 1024 + 512 * 1024`
  - **Purpose**: Controls persistent data generation to prevent history bloat

- **Maximum Block KV Updates**: 500,000 operations
  - **Constant**: `BLOCK_KV_UPDATE_LIMIT = 500_000`
  - **Purpose**: Limits state database modification rate for sustainable growth

#### 3.4.3 Transaction-Level Limits

To prevent DoS attacks where malicious actors create transactions that can never be included in blocks, each transaction is limited to 25% of the corresponding block limit. This ensures that successfully executed transactions will likely be included in blocks:

- **Transaction Data Limit**: 3.125 MB
  - **Formula**: `TX_DATA_LIMIT = BLOCK_DATA_LIMIT × 25 / 100`
  - **Calculated**: `13,107,200 × 0.25 = 3,276,800 bytes (≈3.125 MB)`

- **Transaction KV Update Limit**: 125,000 operations
  - **Formula**: `TX_KV_UPDATE_LIMIT = BLOCK_KV_UPDATE_LIMIT × 25 / 100`
  - **Calculated**: `500,000 × 0.25 = 125,000 updates`

**Enforcement:**
- When either limit is exceeded, the transaction halts immediately with `OutOfGas` error
- **All remaining gas in the transaction is consumed as penalty**
- Limits are enforced at the EVM instruction level during execution

#### 3.4.4 Resource Accounting

MiniRex uses approximate formulas to track data size and KV updates incurred by transactions. These measurements prioritize simplicity and performance over perfect accuracy, as very few transactions are expected to approach the resource limits in practice. The limits are designed to prevent extreme abuse cases rather than precisely meter every operation, allowing for efficient implementation while maintaining effective protection against resource exhaustion attacks.

**KV Updates Counting**

| **Operation** | **KV Count** | **Discarded on Revert** | **Notes** |
|---|---|---|---|
| **Caller account update** | 1 | ❌ | Always counted at transaction start |
| **EIP-7702 authorizations** | `authorization_count` | ❌ | 1 KV update per authorization |
| **SSTORE (new write)** | 1 | ✅ | When original ≠ new AND first write to slot in transaction |
| **SSTORE (refund)** | -1 | ✅ | When slot reset to original value |
| **SSTORE (rewrite)** | 0 | ✅ | Overwrite slot with the same value |
| **CREATE/CREATE2** | 1 | ✅ | One created account |
| **CALL with transfer** | 2 | ✅ | Caller + callee accounts |
| **CALL without transfer** | 0 | ✅ | No account state changes |

**Data Size Counting**

| **Data Type** | **Size (Bytes)** | **Discarded on Revert** | **Notes** |
|---|---|---|---|
| **Base transaction data** | 110 (`BASE_TX`) | ❌ | Fixed overhead per transaction |
| **Calldata** | `tx.input().len()` | ❌ | Variable length input data |
| **Access list** | Sum of `access.size()` for each entry | ❌ | EIP-2930 access list entries |
| **EIP-7702 authorizations** | `authorization_count × 101` | ❌ |  |
| **Caller account update** | 40 | ❌ | Always counted at transaction start |
| **EIP-7702 authority updates** | `authorization_count × 40` | ❌ | Account updates for authorities |
| **Log** | `num_topics × 32 + data.len()` | ✅ |  |
| **SSTORE** | `sstore_kv_change * 40`  | ✅ | Possible values: 40, 0, or -40 |
| **Account update** | 40 | ✅ |  |
| **Deployed bytecode** | `contract_code.len()` | ✅ | Actual deployed contract size |


## 5. Implementation Details

### 5.1 Specification Mapping

The semantic of MiniRex spec is inherited and customized from:

- **MiniRex** → **Optimism Isthmus** → **Ethereum Prague**

### 5.2 Instruction Overrides

The following opcodes have custom implementations in MiniRex:

- `LOG0`, `LOG1`, `LOG2`, `LOG3`, `LOG4`: Enhanced with tx data size limit protection
- `SELFDESTRUCT`: Completely disabled (maps to `invalid` instruction)
- `SSTORE`: Increased gas cost and limit enforcement
- `CREATE`, `CREATE2`: Increased gas cost, limit enforcement, and 98/100 gas forwarding
- `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`: Enhanced new account gas cost, limit enforcement, and 98/100 gas forwarding

### 5.3 Gas Cost Oracle

- **ExternalEnvOracle**: An oracle providing external environment information is introduced to EVM to provide SALT bucket metadata.
- **Dynamic Pricing**: Gas costs scale with SALT bucket capacity

## 6. Migration Impact

### 6.1 For Contracts

- **Large Contracts**: Can now deploy contracts up to 512 KB (previously 24 KB)
- **SELFDESTRUCT**: Any contract using SELFDESTRUCT will fail after MiniRex activation

### 6.2 For Applications

- **Multi-dimensional Resource Limits**: Must respect new data and KV update limits
- **Gas Costs**: Opcodes like `SSTORE` and `LOG` become much more expensive in gas (but still much cheaper in dollar terms)
- **Gas Estimation**: Local gas estimation by tools like Foundry becomes highly inaccurate
