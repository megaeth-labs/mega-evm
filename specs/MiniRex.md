# MiniRex Specification

## 1. Introduction

The **MiniRex** hardfork represents a critical evolution of the MegaETH EVM designed to address the unique economic and technical challenges arising from MegaETH's distinctive architecture. Unlike traditional Ethereum networks, MegaETH features an extremely low minimum base fee and exceptionally high transaction gas limits, creating unprecedented opportunities for both innovation and potential resource abuse.

While MegaETH's low fees make computation extremely affordable—enabling complex applications previously economically infeasible—they also create vulnerabilities under standard EVM semantics. Operations that impose storage costs on nodes become dramatically underpriced, potentially leading to:

- **Unsustainable state bloat** through cheap storage writes and account creation
- **History data explosion** via excessive logging and transaction calldata
- **Call depth attacks** reintroduced by high gas limits that bypass EIP-150 protections

MiniRex addresses these challenges through five key changes:

1. **Dual Gas Model**: Separates transaction costs into compute gas (standard Optimism EVM costs) and storage gas (additional costs for persistent storage operations), making storage operations appropriately expensive while keeping computation affordable
2. **Multi-dimensional Resource Limits**: Three independent constraints (compute gas: 1B per tx, data size: 3.125 MB per tx / 12.5 MB per block, KV updates: 125K per tx / 500K per block) enable safe removal of block gas limit while protecting replica nodes
3. **Volatile Data Access Control**: Accessing frequently contended data (block environment, beneficiary, oracle) triggers compute gas limiting (20M for block env/beneficiary, 1M for oracle) to support efficient parallel execution
4. **Modified Gas Forwarding (98/100 Rule)**: Subcalls receive at most 98/100 of remaining gas (vs. standard 63/64) to prevent call depth attacks with high gas limits
5. **System Infrastructure**: Contract size increased to 512 KB, SELFDESTRUCT disabled, oracle contract deployed at `0x6342000000000000000000000000000000000001`, and increased precompile costs

This document details all semantic changes, their rationale, and implementation requirements for the MiniRex hardfork activation.

## 2. Comprehensive List of Changes

### 2.1 Contract Size Limits

**Increased Maximum Contract Size:**
524,288 bytes (512 KB)

- In comparison, Standard EVM (EIP-170): 24,576 bytes (24 KB)

**Increased Maximum Initcode Size:**
536,576 bytes (512 KB + 24 KB)

- In comparison, Standard EVM (EIP-3860): 49,152 bytes (48 KB)

### 2.2 SELFDESTRUCT Opcode Deprecation

**Complete Disabling of SELFDESTRUCT:**
SELFDESTRUCT opcode now halts execution with `InvalidFEOpcode` error

### 2.3 Additional Storage Gas Costs

MiniRex introduces a **dual-gas model** that separates gas costs into two categories to enable independent pricing of computational work versus storage burden:

- **Compute Gas**: Standard Optimism EVM (Isthmus version) gas costs
- **Storage Gas**: Additional costs for persistent storage operations

The **overall gas cost** (i.e., the gas used reported in transaction receipt) is the sum of compute gas cost and storage gas cost. This separation prevents state bloat and history data growth while maintaining computational efficiency.

Storage gas costs are applied to operations that impose storage costs on nodes:

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

For detailed specifications, gas calculation formulas, and SALT bucket-based dynamic scaling, see [DUAL_GAS_MODEL.md](../docs/DUAL_GAS_MODEL.md).

### 2.4 Increased Precompile Gas Costs

MiniRex increases gas costs for specific precompiles to reflect their computational complexity and prevent abuse.
These precompile gas costs are considered as **compute gas** in our [dual gas model](../docs/DUAL_GAS_MODEL.md).

**KZG Point Evaluation (0x0A):**

- **Gas Cost**: 100,000 gas
  - **Increase**: 2× standard cost of the standard EVM Prague version

**ModExp (0x05):**

- **Gas Cost**: Same as EIP-7883

### 2.5 EIP-150 Gas Forwarding (98/100 Rule)

MegaETH's extremely high transaction gas limits (e.g., 10 billion gas) reintroduce call depth attacks that EIP-150 solved for Ethereum. With 63/64 gas forwarding: `10^10 × (63/64)^1024 ≈ 991 gas` remains after 1,024 calls, enough to make one more call and exceed the stack depth limit.

**Gas Forwarding Rule:**

- **98/100 rule** - forwards at most 98/100 of remaining gas to subcalls
  - **Different** from the standard EVM's 63/64 rule.
  - **Result**: `10^10 × (98/100)^1024 ≈ 10 gas` after 1,024 calls

### 2.6 Multi-dimensional Resource Limits

#### 2.6.1 Rationale

Traditional blockchain networks rely on a single block gas limit to constrain all types of resources—computation, storage operations, and network bandwidth. While this unified approach provides simplicity, it creates fundamental scaling limitations. Each resource type has different characteristics and bottlenecks, yet the gas limit forces them to scale together. When operators want to increase one resource capacity (such as computation), they must proportionally increase all others, which is not always possible.

For example, suppose developers implement a clever optimization in the EVM execution engine that makes it twice as fast. To take advantage of this computational improvement, operators would want to double the maximum computation allowed in a block. However, this requires doubling the entire block gas limit. As a result, the maximum storage operations and network bandwidth consumption per block also double. Such changes may compromise network stability, as any node meeting minimum hardware requirements must be able to keep up with the sequencer.

MegaETH's architecture exemplifies this challenge. The hyper-optimized sequencer possesses exceptionally high computation capacity, capable of processing far more transactions than traditional networks. However, replica nodes still face the same fundamental constraints: they must receive state updates over the network and apply database modifications to update their state roots accordingly. Under a traditional gas limit model, these network and storage constraints artificially bottleneck the sequencer's computational capabilities.

To solve this problem, MiniRex replaces the monolithic block gas limit with **three independent resource dimensions**:

- **Compute Gas Limit**: Tracks and limits computational work performed during EVM execution (globally across all message calls), separate from the gas limit and gas used for each message call. This enables fine-grained control over computational resources while preserving gas for other operations. In practice, the measured compute gas is the vanilla gas cost by each operation/opcode in Optimism EVM (Isthmus Spec).

- **Data Size Limit**: Constrains the amount of data that must be transmitted over the network during live synchronization, preventing history bloat and ensuring replica nodes can maintain pace with data transmission requirements.

- **KV Updates Limit**: Constrains the number of key-value updates that must be applied to the local database and incorporated into state root calculations, ensuring replica nodes can process state changes efficiently.

This multi-dimensional approach enables the sequencer to safely create blocks containing extensive computation, provided they satisfy all three independent constraints.

For detailed resource accounting formulas, two-phase checking strategy, and block construction workflow, see [BLOCK_AND_TX_LIMITS.md](../docs/BLOCK_AND_TX_LIMITS.md). For more about how resource usage is accounted, see [RESOURCE_ACCOUNTING.md](../docs/RESOURCE_ACCOUNTING.md).

#### 2.6.2 MiniRex Configuration

MiniRex enforces the following resource limits.

**Transaction-Level Limits:**

| Limit Type      | Value                      | Enforcement                                                                               |
| --------------- | -------------------------- | ----------------------------------------------------------------------------------------- |
| **Compute Gas** | 1,000,000,000 gas (1B)     | Transaction execution halts when exceeded, remaining gas preserved and refunded to sender |
| **Data Size**   | 3,276,800 bytes (3.125 MB) | Transaction execution halts when exceeded, remaining gas preserved and refunded to sender |
| **KV Updates**  | 125,000 operations         | Transaction execution halts when exceeded, remaining gas preserved and refunded to sender |

**Block-Level Limits:**

| Limit Type     | Value                      | Enforcement                                                             |
| -------------- | -------------------------- | ----------------------------------------------------------------------- |
| **Data Size**  | 13,107,200 bytes (12.5 MB) | Transaction exceeding the block limit will not be included in the block |
| **KV Updates** | 500,000 operations         | Transaction exceeding the block limit will not be included in the block |

Other limits are left unlimited.

### 2.7 Oracle Contract

The oracle contract (as defined in [ORACLE_SERVICE.md](../docs/ORACLE_SERVICE.md)) will be deployed as pre-execution state changes in the first block of MiniRex hardfork at address `0x6342000000000000000000000000000000000001`.

The periphery contract for high-precision timestamp oracle service is also deployed as pre-execution state changes in the first block of MiniRex hardfork at address `0x6342000000000000000000000000000000000002`.

The specific contract code can be found in [system-contracts](../crates/mega-evm/system-contracts/).

### 2.8 Volatile Data Access Control

MegaETH's highly optimized parallel execution model requires limiting transaction conflicts related to frequently accessed shared data (e.g., block environment data, beneficiary balance, and oracle contract storage).
This shared data is considered **volatile data** in the EVM because it can change between blocks or is commonly accessed by system transactions.

To support efficient parallel execution, MiniRex introduces comprehensive tracking and gas limiting for three categories of volatile data: block environment data, beneficiary balance, and oracle contract access.
When any volatile data is accessed during transaction execution, the remaining [compute gas limit](../docs/BLOCK_AND_TX_LIMITS.md) is immediately capped to prevent excessive computation after obtaining this time-sensitive or contended information.

#### 2.8.1 Block Environment Access

**Tracked Opcodes:**
Block environment opcodes that trigger compute gas limiting when executed:

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
- The whole transaction's remaining compute gas is limited to at most `20_000_000` immediately after the opcode executes, if the current remaining compute gas is larger.

#### 2.8.2 Beneficiary Account Access

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

- The whole transaction's remaining compute gas is limited to at most `20_000_000` immediately after any beneficiary account access, if the current remaining compute gas is larger.

#### 2.8.3 Oracle Contract Access

**Trigger Conditions:**
Oracle contract access is detected at the frame initialization level, tracking:

| Trigger Type                 | Description                                                  |
| ---------------------------- | ------------------------------------------------------------ |
| **Direct transaction call**  | Transaction's `to` address is the oracle contract            |
| **Internal CALL operations** | CALL, CALLCODE, DELEGATECALL, or STATICCALL targeting oracle |

**Detection Location:**
Frame-level detection in `frame_init` handler, ensuring comprehensive tracking of both direct and nested oracle calls.

**Exemption:**
Transactions sent from the mega system address (`0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d`) are exempted from oracle access tracking to enable system operations.

**Behavior:**

- The whole transaction's remaining compute gas is limited to at most `1_000_000` immediately after oracle contract access is detected, if the current remaining compute gas is larger.

## 3. Specification Mapping

The semantics of MiniRex spec are inherited and customized from:

- **MiniRex** → **Optimism Isthmus** → **Ethereum Prague**

## 4. Migration Impact

### 4.1 For Contracts

**Contract Size:**

- Can now deploy contracts up to **512 KB** (previously 24 KB limit)
- Enables more complex contract logic and larger applications

**Deprecated Opcodes:**

- **SELFDESTRUCT**: Any contract using SELFDESTRUCT will halt with `InvalidFEOpcode` error after MiniRex activation
- Contracts should use alternative patterns for resource cleanup

### 4.2 For Applications

**Storage Gas Costs:**

- New storage gas costs are added on top of compute gas (see [DUAL_GAS_MODEL.md](../docs/DUAL_GAS_MODEL.md))
- Operations imposing storage burden become more expensive:
  - **SSTORE** (0 → non-0): +2M × bucket_multiplier storage gas
  - **Account creation**: +2M × bucket_multiplier storage gas
  - **Code deposit**: +10,000 gas/byte storage gas
  - **LOG operations**: +3,750/topic + 80/byte storage gas
  - **Calldata**: +40/zero-byte + 160/non-zero-byte storage gas
- Total gas cost = compute gas + storage gas

**Multi-dimensional Resource Limits:**

- Transactions must respect three independent limits:
  - **Compute gas limit**: 1B gas per transaction
  - **Data size limit**: 3.125 MB per transaction, 12.5 MB per block
  - **KV updates limit**: 125,000 per transaction, 500,000 per block
- Transactions halted by limits receive refund for remaining gas

**Volatile Data Access (Compute Gas Detention):**

- Accessing volatile data triggers compute gas limiting to support parallel execution:
  - **Block environment opcodes** (20M compute gas limit): NUMBER, TIMESTAMP, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE, PREVRANDAO, BLOCKHASH, BLOBBASEFEE, BLOBHASH
  - **Beneficiary account access** (20M compute gas limit): Operations on beneficiary address including BALANCE, SELFBALANCE, EXTCODECOPY, EXTCODESIZE, EXTCODEHASH, or transactions involving beneficiary
  - **Oracle contract access** (1M compute gas limit): Direct transaction calls or CALL-family instructions targeting oracle contract at `0x6342000000000000000000000000000000000001`
  - **System exemption**: Transactions from mega system address (`0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d`) are exempt from oracle tracking
  - **Most restrictive limit applies**: When multiple volatile data types are accessed, the minimum limit (1M for oracle, 20M for block env/beneficiary) applies globally
- Excess gas beyond limit is "detained" and refunded at transaction end
- Applications should use volatile data access only for essential decision-making, not extensive computation

**Gas Estimation:**

- Local gas estimation tools may become inaccurate due to:
  - Dynamic bucket-based storage gas multipliers
  - Multi-dimensional resource limits
  - Compute gas detention for volatile data access
- Recommend using MegaETH's native gas estimation APIs for accurate results

**EIP-150 Gas Forwarding:**

- Subcalls now receive at most **98/100** of remaining gas (not 63/64)
- May affect contracts that depend on precise gas forwarding behavior
