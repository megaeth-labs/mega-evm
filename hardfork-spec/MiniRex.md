# MiniRex Hardfork Specification

## Summary

| Area / Opcode                                            | Standard EVM (Prague)                              | Mega EVM (MiniRex)                                                                                                                                                                                                                      |
| -------------------------------------------------------- | -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **SSTORE (0 → non-0)**                                   | 20,000 (per EIP-2200)                              | **2,000,000 × bucket multiplier**                                                                                                                                                                                                       |
| **SSTORE (other cases)**                                 | EIP-2200 rules (reset, same, refund, warm read)    | Same as standard                                                                                                                                                                                                                        |
| **New account (CALL → empty, tx callee, CREATE target)** | 25,000 (NEWACCOUNT)                                | **2,000,000 × bucket multiplier**                                                                                                                                                                                                       |
| **Contract creation (CREATE/CREATE2)**                   | 25,000 (new account) + code deposit gas            | **2,000,000 × multiplier (new acct)** + **2,000,000 (codehash)** + code deposit gas                                                                                                                                                     |
| **Code deposit (per byte)**                              | 200 gas/byte                                       | **10,000 gas/byte**                                                                                                                                                                                                                     |
| **Initcode max size**                                    | 49152 bytes (per EIP-3860)                         | **MAX_CONTRACT_SIZE (512 KiB) + 24 KiB**                                                                                                                                                                                                |
| **CREATE/CALL forwarding fraction**                      | 63/64 of gas left                                  | **98/100** of gas left                                                                                                                                                                                                                  |
| **LOG per topic (compute gas)**                          | 375 gas/topic                                      | **375 gas/topic (same)**                                                                                                                                                                                                                |
| **LOG per topic (storage gas)**                          | N/A                                                | **3,750 gas/topic (10× multiplier)**                                                                                                                                                                                                    |
| **LOG per byte (compute gas)**                           | 8 gas/byte                                         | **8 gas/byte (same)**                                                                                                                                                                                                                   |
| **LOG per byte (storage gas)**                           | N/A                                                | **80 gas/byte (10× multiplier)**                                                                                                                                                                                                        |
| **Calldata per-byte**                                    | 4 gas (zero byte), 16 gas (non-zero byte)          | **40 gas (zero byte), 160 gas (non-zero byte)**                                                                                                                                                                                         |
| **SELFDESTRUCT**                                         | Disabled refunds post-Shanghai but still available | **Instruction removed**                                                                                                                                                                                                                 |
| **Volatile data access**                                 | No restrictions                                    | **Gas limited based on access type:** Block env/beneficiary → **20M gas (`BLOCK_ENV_ACCESS_REMAINING_GAS`)**, Oracle contract → **1M gas (`ORACLE_ACCESS_REMAINING_GAS`)**, most restrictive limit applies when multiple types accessed |
| **Per-tx compute gas limit**                             | none                                               | **1,000,000,000 gas** for compute operations (separate from standard gas limit)                                                                                                                                                         |
| **Per-tx data limit**                                    | none                                               | **3.125 MiB (25% of 12.5 MiB block limit)** across calldata + logs + return + initcode                                                                                                                                                  |
| **Per-tx KV update limit**                               | none                                               | **125,000 updates (25% of block limit)**                                                                                                                                                                                                |

## 1. Introduction

The **MiniRex** hardfork represents a critical evolution of the MegaETH EVM designed to address the unique economic and technical challenges arising from MegaETH's distinctive architecture. Unlike traditional Ethereum networks, MegaETH operates with an extremely low minimum base fee (0.001 gwei vs Ethereum's 0.5 gwei) and exceptionally high transaction gas limits (up to 10 billion gas), creating unprecedented opportunities for both innovation and abuse.

While MegaETH's low fees make computation extremely affordable—enabling complex applications previously economically infeasible—they also create severe vulnerabilities under standard EVM semantics. Operations that impose storage costs on nodes become dramatically underpriced, potentially leading to:

- **Unsustainable state bloat** through cheap storage writes and account creation
- **Archive node data explosion** via excessive logging and transaction data
- **Reintroduced call depth attacks** due to high gas limits bypassing EIP-150 protections

To ensure network stability and sustainable growth of MegaETH, the MiniRex hardfork introduces:

1. **Multi-dimensional Resource Limits**: Novel constraints on compute gas (1B), data size (3.125 MB), and key-value updates (125K) enable safe removal of block gas limit while preventing resource exhaustion
2. **Compute Gas Tracking**: Separate tracking for computational costs with immediate enforcement when compute gas limit is exceeded, halting execution while preserving remaining gas
3. **Strategic Gas Cost Increases**: Storage operations (SSTORE, account creation) see substantial gas cost increases while LOG operations split costs into compute gas (standard) and storage gas (10× multiplier)
4. **Volatile Data Access Control**: Block environment data, beneficiary balance, and oracle contract access trigger immediate gas detention with type-specific limits (20M gas for block env/beneficiary, 1M gas for oracle), with excess gas detained and refunded

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

**Cold Storage Access (EIP-2929):**

- **Cold Access Penalty**: Additional 2,100 gas on first access to storage slot per transaction
- **Applies to**: Both SSTORE and SLOAD operations
- **Behavior**: First access to an `(address, storage_key)` pair charges the cold access cost; subsequent accesses in the same transaction use warm pricing
- **Purpose**: Properly price storage access operations and prevent DoS attacks

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

- **MiniRex Cost**: 10,000 gas per byte (200 standard + 9,800 additional)
- **Standard EVM**: 200 gas per byte
- **Constant**: `CODEDEPOSIT_ADDITIONAL_GAS = 10_000 - CODEDEPOSIT` where CODEDEPOSIT = 200
- **Purpose**: Prevent unsustainable state bloat

#### 3.3.4 Logging Operations

MiniRex introduces a dual-gas model for LOG operations, separating the cost into compute gas (for EVM execution) and storage gas (for persistent storage). This enables independent pricing of computational work versus storage burden.

**Compute Gas (tracked in compute gas limit):**

- **Base cost**: 375 gas (LOG0) + 375 gas per topic (same as standard EVM)
- **Data cost**: 8 gas per byte (same as standard EVM)
- **Total compute**: `375 + (375 × num_topics) + (8 × data_length)`

**Storage Gas (tracked in standard gas limit):**

- **Topic storage**: 3,750 gas per topic (10× standard topic cost)
- **Data storage**: 80 gas per byte (10× standard data cost)
- **Total storage**: `(3,750 × num_topics) + (80 × data_length)`

**Total Gas Cost**: Compute gas + Storage gas

**Constants**:

- Base and topic costs remain at standard 375 gas each for compute tracking
- Storage multiplier: 10× for both topics and data

**Purpose**:

- Separate computational work from storage burden
- Prevent unsustainable history data growth through storage gas
- Enable independent resource pricing for compute vs storage

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

### 3.4 Volatile Data Access Control

MiniRex introduces comprehensive tracking and gas limiting for three categories of volatile information: block environment data, beneficiary balance, and oracle contract access. When any volatile data is accessed during transaction execution, the remaining gas is immediately limited to prevent excessive computation after obtaining privileged information.

#### 3.4.1 Gas Detention Mechanism

**Type-Specific Gas Limits:**

- **Block Environment and Beneficiary Access**: `BLOCK_ENV_ACCESS_REMAINING_GAS` = 20,000,000 gas (20M)
- **Oracle Contract Access**: `ORACLE_ACCESS_REMAINING_GAS` = 1,000,000 gas (1M)
- **Most Restrictive Limit Wins**: When multiple volatile data types are accessed, the minimum (most restrictive) limit applies, regardless of access order

**Global Gas Limitation:**

- **Scope**: Once triggered, applies globally to:
  - The current call frame where volatile data was accessed
  - All parent call frames after the call returns
  - All subsequent operations in the transaction
- **Gas Detention**: Gas above the applicable limit is "detained" (tracked separately) and refunded at transaction end
- **Fair Billing**: Users only pay for actual work; detained gas is refunded automatically
- **Purpose**: Ensure volatile data access is used only for essential decision-making, not extensive computation

**Example Flows:**

1. **Block env then oracle**: Transaction with 1B gas → TIMESTAMP (20M limit, 980M detained) → CALL(oracle) (1M limit applied, 999M total detained)
2. **Oracle then block env**: Transaction with 1B gas → CALL(oracle) (1M limit, 999M detained) → TIMESTAMP (still 1M limit, same detention)
3. **Result**: Both orders produce the same final state with 1M gas limit and 999M gas detained

#### 3.4.2 Block Environment Access

**Tracked Opcodes:**
Block environment opcodes that trigger gas limiting when executed:

| Opcode        | Access Type   | Description               |
| ------------- | ------------- | ------------------------- |
| `NUMBER`      | BLOCK_NUMBER  | Current block number      |
| `TIMESTAMP`   | TIMESTAMP     | Current block timestamp   |
| `COINBASE`    | COINBASE      | Block beneficiary address |
| `DIFFICULTY`  | DIFFICULTY    | Current block difficulty  |
| `GASLIMIT`    | GAS_LIMIT     | Block gas limit           |
| `BASEFEE`     | BASE_FEE      | Base fee per gas          |
| `PREVRANDAO`  | PREV_RANDAO   | Previous block randomness |
| `BLOCKHASH`   | BLOCK_HASH    | Block hash lookup         |
| `BLOBBASEFEE` | BLOB_BASE_FEE | Blob base fee per gas     |
| `BLOBHASH`    | BLOB_HASH     | Blob hash lookup          |

**Behavior:**

- Accessing any of these opcodes marks the corresponding access type
- Gas is limited immediately after the opcode executes
- Multiple accesses to different block environment opcodes share the same global `VOLATILE_DATA_ACCESS_REMAINING_GAS` limit

#### 3.4.3 Beneficiary Account Access

**Trigger Conditions:**
Any operation that accesses the beneficiary account triggers gas limiting:

| Operation              | Opcodes                                     | Description                                            |
| ---------------------- | ------------------------------------------- | ------------------------------------------------------ |
| **Account balance**    | `BALANCE`, `SELFBALANCE`                    | Reading beneficiary's balance                          |
| **Account code**       | `EXTCODECOPY`, `EXTCODESIZE`, `EXTCODEHASH` | Accessing beneficiary's code                           |
| **Transaction caller** | N/A                                         | Transaction sender is the beneficiary                  |
| **Call recipient**     | N/A                                         | Transaction recipient (CALL target) is the beneficiary |
| **Delegated access**   | `DELEGATECALL`                              | Accessing beneficiary account in delegated context     |

**Note:** The beneficiary address is obtained from the block's coinbase field.

**Behavior:**

- Gas is limited immediately after any beneficiary account access
- Shares the same global `VOLATILE_DATA_ACCESS_REMAINING_GAS` limit with other volatile data accesses
- All account-related operations (balance, code, code hash) on the beneficiary trigger this protection

#### 3.4.4 Oracle Contract Access

**Oracle Contract Details:**

- **Address**: `0x4200000000000000000000000000000000000101` (decimal: 790)
- **Trigger Condition**: Any CALL, CALLCODE, DELEGATECALL, or STATICCALL instruction targeting the oracle contract address
- **Tracking Limitation**: Direct transaction calls to the oracle contract do NOT trigger gas limiting or tracking

**Storage Access:**

- SLOAD operations on the oracle contract use the OracleEnv to provide storage values
- If OracleEnv returns `None` for a storage slot, the operation falls back to standard database lookup
- This allows the oracle contract to serve volatile data from an external source while maintaining compatibility with standard storage operations

**Example Flow:**

```
Transaction (1B gas) → Contract A → TIMESTAMP (block env accessed)
                                         ↓
                      Gas limited to 20M (BLOCK_ENV_ACCESS_REMAINING_GAS), 980M detained
                                         ↓
                          → Contract B → BALANCE(beneficiary)
                                         ↓
                      Still limited to 20M (beneficiary also uses 20M limit)
                                         ↓
                          → Oracle Contract (CALL)
                                         ↓
                      Gas further limited to 1M (ORACLE_ACCESS_REMAINING_GAS), 999M detained
                                         ↓
                          → Returns to Contract A (gas still limited to 1M)
                                         ↓
                          Transaction ends, 999M detained gas refunded
```

### 3.5. Multi-dimensional Resource Limits

#### 3.5.1 Rationale

Traditional blockchain networks rely on a single block gas limit to constrain all types of resources—computation, storage operations, and network bandwidth. While this unified approach provides simplicity, it creates fundamental scaling limitations. Each resource type has different characteristics and bottlenecks, yet the gas limit forces them to scale together. When operators want to increase one resource capacity (such as computation), they must proportionally increase all others, which is not always possible.

For example, suppose developers implement a clever optimization in the EVM execution engine that makes it twice as fast. To take advantage of this computational improvement, operators would want to double the maximum computation allowed in a block. However, this requires doubling the entire block gas limit. As a result, the maximum storage operations and network bandwidth consumption per block also double. Such changes may compromise network stability, as any node meeting minimum hardware requirements must be able to keep up with the sequencer.

MegaETH's architecture exemplifies this challenge. The hyper-optimized sequencer possesses exceptionally high computation capacity, capable of processing far more transactions than traditional networks. However, replica nodes still face the same fundamental constraints: they must receive state updates over the network and apply database modifications to update their state roots accordingly. Under a traditional gas limit model, these network and storage constraints artificially bottleneck the sequencer's computational capabilities.

To solve this problem, MiniRex replaces the monolithic block gas limit with three independent resource constraints:

- **Compute Gas Limit**: Tracks and limits computational work performed during EVM execution, separate from the standard gas limit. This enables fine-grained control over computational resources while preserving gas for other operations.

- **Data Size Limit**: Constrains the amount of data that must be transmitted over the network during live synchronization, preventing history bloat and ensuring replica nodes can maintain pace with data transmission requirements.

- **KV Updates Limit**: Constrains the number of key-value updates that must be applied to the local database and incorporated into state root calculations, ensuring replica nodes can process state changes efficiently.

This multi-dimensional approach enables the sequencer to safely create blocks containing extensive computation, provided they satisfy all three independent constraints. The result is a more flexible and efficient resource allocation model that maximizes MegaETH's computational advantages while maintaining network stability.

#### 3.5.2 Block-Level Limits

These limits define the maximum resources that can be consumed across all transactions within a single block:

- **Maximum Block Data**: 12.5 MB

  - **Constant**: `BLOCK_DATA_LIMIT = 12 * 1024 * 1024 + 512 * 1024`
  - **Purpose**: Controls persistent data generation to prevent history bloat

- **Maximum Block KV Updates**: 500,000 operations
  - **Constant**: `BLOCK_KV_UPDATE_LIMIT = 500_000`
  - **Purpose**: Limits state database modification rate for sustainable growth

#### 3.5.3 Transaction-Level Limits

To prevent DoS attacks where malicious actors create transactions that can never be included in blocks, each transaction enforces independent limits for compute gas, data size, and KV updates:

- **Transaction Compute Gas Limit**: 1,000,000,000 gas

  - **Constant**: `TX_COMPUTE_GAS_LIMIT = 1_000_000_000`
  - **Purpose**: Limits computational work during EVM execution, tracked separately from standard gas

- **Transaction Data Limit**: 3.125 MB

  - **Formula**: `TX_DATA_LIMIT = BLOCK_DATA_LIMIT × 25 / 100`
  - **Calculated**: `13,107,200 × 0.25 = 3,276,800 bytes (≈3.125 MB)`

- **Transaction KV Update Limit**: 125,000 operations
  - **Formula**: `TX_KV_UPDATE_LIMIT = BLOCK_KV_UPDATE_LIMIT × 25 / 100`
  - **Calculated**: `500,000 × 0.25 = 125,000 updates`

**Enforcement:**

- When any limit is exceeded, the transaction halts immediately with `OutOfGas` error
- **Remaining gas in the transaction is preserved (not consumed) and refunded to the sender** - this applies to all three limit types
- Limits are enforced at the EVM instruction level during execution
- Each limit is tracked independently and enforced separately

#### 3.5.4 Resource Accounting

MiniRex uses approximate formulas to track compute gas, data size, and KV updates incurred by transactions. These measurements prioritize simplicity and performance over perfect accuracy, as very few transactions are expected to approach the resource limits in practice. The limits are designed to prevent extreme abuse cases rather than precisely meter every operation, allowing for efficient implementation while maintaining effective protection against resource exhaustion attacks.

**Compute Gas Tracking**

Compute gas is tracked separately from the standard gas limit and monitors the cumulative gas consumed during EVM instruction execution:

- **Tracked Operations**: All gas consumed during frame execution, including:
  - EVM instruction costs (SSTORE, CALL, CREATE, arithmetic, etc.)
  - Memory expansion costs
  - LOG operations (only the compute portion: 375 base + 375/topic + 8/byte)
  - Code deposit costs during contract creation
- **Not Tracked**: Gas refunds, gas detention from volatile data access
- **Accumulation**: Compute gas accumulates across all nested call frames within a transaction
- **Limit Enforcement**: When `compute_gas_used > TX_COMPUTE_GAS_LIMIT`, transaction halts with `OutOfGas` and remaining gas is preserved

**KV Updates Counting**

| **Operation**                         | **KV Count**          | **Discarded on Revert** | **Notes**                                                                                             |
| ------------------------------------- | --------------------- | ----------------------- | ----------------------------------------------------------------------------------------------------- |
| **Transaction sender account update** | 1                     | ❌                      | Always counted at transaction start                                                                   |
| **EIP-7702 authorizations**           | `authorization_count` | ❌                      | 1 KV update per authorization regardless of its validity                                              |
| **SSTORE (new write)**                | 1                     | ✅                      | When original ≠ new AND first write to slot in transaction                                            |
| **SSTORE (refund)**                   | -1                    | ✅                      | When slot reset to original value                                                                     |
| **SSTORE (rewrite)**                  | 0                     | ✅                      | Overwrite slot with the same value                                                                    |
| **CREATE/CREATE2**                    | 1 or 2                | ✅                      | One created account + caller account (optinal, if caller is not updated in the _parent_ message call) |
| **CALL with transfer**                | 1 or 2                | ✅                      | Callee account + caller Account (optional, if caller is not updated in the _parent_ message call)     |
| **CALL without transfer**             | 0                     | ✅                      | No account state changes                                                                              |

**Data Size Counting**

| **Data Type**                  | **Size (Bytes)**                      | **Discarded on Revert** | **Notes**                           |
| ------------------------------ | ------------------------------------- | ----------------------- | ----------------------------------- |
| **Base transaction data**      | 110 (`BASE_TX`)                       | ❌                      | Fixed overhead per transaction      |
| **Calldata**                   | `tx.input().len()`                    | ❌                      | Variable length input data          |
| **Access list**                | Sum of `access.size()` for each entry | ❌                      | EIP-2930 access list entries        |
| **EIP-7702 authorizations**    | `authorization_count × 101`           | ❌                      |                                     |
| **Caller account update**      | 40                                    | ❌                      | Always counted at transaction start |
| **EIP-7702 authority updates** | `authorization_count × 40`            | ❌                      | Account updates for authorities     |
| **Log**                        | `num_topics × 32 + data.len()`        | ✅                      |                                     |
| **SSTORE**                     | `sstore_kv_change * 40`               | ✅                      | Possible values: 40, 0, or -40      |
| **Account update**             | 40                                    | ✅                      |                                     |
| **Deployed bytecode**          | `contract_code.len()`                 | ✅                      | Actual deployed contract size       |

## 5. Implementation Details

### 5.1 Specification Mapping

The semantic of MiniRex spec is inherited and customized from:

- **MiniRex** → **Optimism Isthmus** → **Ethereum Prague**

### 5.2 Instruction Overrides

The following opcodes have custom implementations in MiniRex:

- `LOG0`, `LOG1`, `LOG2`, `LOG3`, `LOG4`: Split into compute gas (standard costs) and storage gas (10× multiplier), with compute gas tracked in compute gas limit and tx data size limit protection
- `SELFDESTRUCT`: Completely disabled (maps to `invalid` instruction)
- `SSTORE`: Increased gas cost, compute gas tracking, limit enforcement, and oracle storage access support
- `CREATE`, `CREATE2`: Increased gas cost, compute gas tracking, limit enforcement, and 98/100 gas forwarding
- `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`: Enhanced new account gas cost, compute gas tracking, limit enforcement, 98/100 gas forwarding, and oracle contract access detection with gas detention (1M gas limit)
- `NUMBER`, `TIMESTAMP`, `COINBASE`, `DIFFICULTY`, `GASLIMIT`, `BASEFEE`, `PREVRANDAO`, `BLOCKHASH`, `BLOBBASEFEE`, `BLOBHASH`: Enhanced with block env access detection and gas detention (20M gas limit)
- `BALANCE`, `SELFBALANCE`: Enhanced with beneficiary account access detection and gas detention (20M gas limit)
- `EXTCODECOPY`, `EXTCODESIZE`, `EXTCODEHASH`: Enhanced with beneficiary account access detection and gas detention (20M gas limit)
- All other instructions: Compute gas costs are tracked in the compute gas limit

### 5.3 External Environment

- **SaltEnv**: An external data provider providing external environment information to the EVM for SALT bucket metadata, which is used to calculate dynamic gas cost for account creation and `SSTORE`.
- **OracleEnv**: An external data provider providing storage values for the designated oracle contract (address `0x316`).
  - Enables external data sources to provide sensitive information to smart contracts
  - Falls back to standard database lookup if oracle returns `None`
  - Only active during SLOAD operations targeting the oracle contract address

## 6. Migration Impact

### 6.1 For Contracts

- **Large Contracts**: Can now deploy contracts up to 512 KB (previously 24 KB)
- **SELFDESTRUCT**: Any contract using SELFDESTRUCT will fail after MiniRex activation

### 6.2 For Applications

- **Multi-dimensional Resource Limits**: Must respect new data and KV update limits
- **Gas Costs**: Opcodes like `SSTORE` and `LOG` become much more expensive in gas (but still much cheaper in dollar terms)
- **Gas Estimation**: Local gas estimation by tools like Foundry becomes highly inaccurate
- **Volatile Data Access**: Applications accessing volatile data will have their remaining gas limited based on access type, with excess gas detained and refunded:
  - **Block environment opcodes** (20M gas limit): NUMBER, TIMESTAMP, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE, PREVRANDAO, BLOCKHASH, BLOBBASEFEE, BLOBHASH
  - **Beneficiary account access** (20M gas limit): Any operation on the beneficiary address including:
    - Account balance (BALANCE, SELFBALANCE)
    - Account code (EXTCODECOPY, EXTCODESIZE, EXTCODEHASH)
    - Transaction involving beneficiary (as caller or recipient)
  - **Oracle contract** (1M gas limit): CALL-family instructions targeting address `0x4200000000000000000000000000000000000101`
  - **Most restrictive limit applies**: When multiple volatile data types are accessed, the minimum limit (1M for oracle, 20M for block env/beneficiary) applies globally
- Applications should use volatile data access only for essential decision-making, not extensive computation
