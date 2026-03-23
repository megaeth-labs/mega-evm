# MiniRex Specification

## Abstract

MiniRex is the foundational MegaETH EVM spec.
It builds on Optimism Isthmus (Ethereum Prague) and introduces a dual gas model, multi-dimensional resource limits, volatile data access control for parallel execution, modified gas forwarding, system contracts, and infrastructure changes.

## Base Layer

MiniRex builds on Optimism Isthmus (Ethereum Prague).
All parent layer semantics are inherited unless explicitly overridden below.

## Specifications

### 1. Dual gas model

#### Rationale

MegaETH features extremely low base fees and high transaction gas limits.
Under standard EVM semantics, operations that impose storage costs on nodes (state writes, logs, calldata) become dramatically underpriced, leading to unsustainable state bloat and history data explosion.
Separating gas into compute and storage dimensions allows independent pricing of computational work versus storage burden.

#### Semantics

Every transaction's overall gas cost MUST be the sum of compute gas and storage gas.

- **Compute gas**: Standard Optimism EVM (Isthmus) gas costs for all opcodes and operations.
- **Storage gas**: Additional costs for operations that impose persistent storage burden on nodes.

Storage gas MUST be charged for the following operations:

| Operation | Storage Gas | Condition |
| --- | --- | --- |
| **SSTORE (0→non-0)** | 2,000,000 × multiplier | When `original == 0 AND present == 0 AND new != 0` (EIP-2200 terminology) |
| **Account creation** | 2,000,000 × multiplier | Contract creation or value transfer to empty account |
| **Code deposit** | 10,000/byte | Per byte when contract creation succeeds |
| **LOG topic** | 3,750/topic | Per topic, regardless of revert |
| **LOG data** | 80/byte | Per byte, regardless of revert |
| **Calldata (zero byte)** | 40/byte | Per zero byte in transaction input |
| **Calldata (non-zero byte)** | 160/byte | Per non-zero byte in transaction input |
| **Calldata floor (zero byte)** | 100/byte | EIP-7623 floor cost for zero bytes |
| **Calldata floor (non-zero byte)** | 400/byte | EIP-7623 floor cost for non-zero bytes |

The `multiplier` is derived from SALT bucket capacity: `multiplier = bucket_capacity / MIN_BUCKET_SIZE`.

### 2. Multi-dimensional resource limits

#### Rationale

A single block gas limit forces all resource types (computation, storage, network bandwidth) to scale together.
MegaETH's hyper-optimized sequencer can process far more computation than replica nodes can absorb in storage and network bandwidth.
Independent resource dimensions allow each to scale according to its own bottleneck.

MiniRex replaces the monolithic block gas limit with three independent resource dimensions:

- **Compute gas**: Tracks computational work performed during EVM execution, measured as the standard Optimism EVM (Isthmus) gas cost of each opcode.
- **Data size**: Constrains the amount of data transmitted over the network during live synchronization.
- **KV updates**: Constrains the number of key-value updates applied to the local database for state root calculations.

#### Semantics

Transaction-level limits:

| Limit | Value | Enforcement |
| --- | --- | --- |
| **Compute gas** | 1,000,000,000 (1B) | Halt when exceeded; remaining gas preserved and refunded |
| **Data size** | 3,276,800 bytes (3.125 MB) | Halt when exceeded; remaining gas preserved and refunded |
| **KV updates** | 125,000 operations | Halt when exceeded; remaining gas preserved and refunded |

Block-level limits:

| Limit | Value | Enforcement |
| --- | --- | --- |
| **Data size** | 13,107,200 bytes (12.5 MB) | Transaction exceeding block limit MUST NOT be included |
| **KV updates** | 500,000 operations | Transaction exceeding block limit MUST NOT be included |

- Compute gas, state growth, and total gas MUST have no block-level limit.
- The max total gas limit (storage + compute gas) for a transaction or block is not limited by the EVM spec; it is a chain-configurable parameter.

### 3. Volatile data access control (gas detention)

#### Rationale

MegaETH's parallel execution model requires minimizing transaction conflicts related to frequently accessed shared data.
Block environment fields, the beneficiary's account, and oracle contract storage change between blocks or are commonly accessed by system transactions, making them "volatile data".
Capping compute gas after volatile data access forces transactions that touch this data to terminate quickly, reducing parallel execution conflicts without banning the access outright.

#### Semantics

When volatile data is accessed, the transaction's remaining compute gas MUST be immediately capped.
The cap is absolute: total compute gas usage MUST NOT exceed `cap`.
Remaining compute gas becomes `max(0, cap − consumed)`.
If the transaction has already consumed more than the cap before the access, execution MUST halt immediately with `VolatileDataAccessOutOfGas`.
When multiple volatile data categories are accessed, the most restrictive cap MUST apply.
Detained gas (excess beyond the cap) MUST be refunded at transaction end.

**Block environment access** — cap: 20,000,000 (20M):

The following opcodes MUST trigger block environment gas detention:

| Opcode | Description |
| --- | --- |
| `NUMBER` | Current block number |
| `TIMESTAMP` | Current block timestamp |
| `COINBASE` | Block beneficiary address |
| `DIFFICULTY` | Current block difficulty |
| `GASLIMIT` | Block gas limit |
| `BASEFEE` | Base fee per gas |
| `PREVRANDAO` | Previous block randomness |
| `BLOCKHASH` | Block hash lookup |
| `BLOBBASEFEE` | Blob base fee per gas |
| `BLOBHASH` | Blob hash lookup |

**Beneficiary account access** — cap: 20,000,000 (20M):

Any operation that accesses the beneficiary account MUST trigger gas detention:
- Reading beneficiary's balance via `BALANCE` (any caller) or via `SELFBALANCE` (only when the current contract is the beneficiary)
- Accessing beneficiary's code (`EXTCODECOPY`, `EXTCODESIZE`, `EXTCODEHASH`)
- Transaction sender is the beneficiary
- Transaction recipient (CALL target) is the beneficiary
- Accessing beneficiary account via `DELEGATECALL`

The beneficiary address is obtained from the block's coinbase field.

**Oracle contract access** — cap: 1,000,000 (1M):

Oracle gas detention MUST be triggered by CALL to the oracle contract address (`0x6342000000000000000000000000000000000001`).
CALLCODE, DELEGATECALL, and STATICCALL MUST NOT trigger oracle detention in MiniRex.
Transactions from the mega system address (`MEGA_SYSTEM_ADDRESS`) MUST be exempted from oracle detention.

**Oracle storage forced cold access**:

All SLOAD operations on the oracle contract MUST use cold access gas cost (2100 gas) regardless of EIP-2929 warm/cold tracking state.
This ensures deterministic gas costs during block replay.

### 4. Modified gas forwarding (98/100 rule)

#### Rationale

MegaETH's high transaction gas limits (e.g., 10 billion gas) reintroduce call depth attacks that EIP-150 solved for Ethereum.
With 63/64 gas forwarding, `10^10 × (63/64)^1024 ≈ 991 gas` remains after 1,024 calls — enough to exceed the stack depth limit.
The 98/100 rule reduces this to `10^10 × (98/100)^1024 ≈ 10 gas`.

#### Semantics

- Subcalls MUST receive at most 98/100 of remaining gas (replacing the standard EVM's 63/64 rule).
- In MiniRex, the 98/100 rule MUST apply to CALL and CREATE/CREATE2.
- CALLCODE, DELEGATECALL, and STATICCALL are NOT subject to the 98/100 rule in MiniRex (this is fixed in Rex).

### 5. SELFDESTRUCT disabled

#### Rationale

SELFDESTRUCT complicates state management and is deprecated in the broader Ethereum ecosystem.

#### Semantics

- The `SELFDESTRUCT` opcode MUST halt execution with `InvalidFEOpcode`.

### 6. Contract size limits

#### Rationale

MegaETH's architecture supports larger contracts than standard Ethereum.

#### Semantics

- Maximum contract size MUST be 524,288 bytes (512 KB).
  Standard EVM (EIP-170): 24,576 bytes (24 KB).
- Maximum initcode size MUST be 548,864 bytes (512 KB + 24 KB).
  Standard EVM (EIP-3860): 49,152 bytes (48 KB).

### 7. Increased precompile gas costs

#### Rationale

Certain precompiles are underpriced relative to their computational complexity on MegaETH's architecture.

#### Semantics

- KZG Point Evaluation (address `0x0A`) MUST cost 100,000 gas (2× standard EVM Prague cost).
- ModExp (address `0x05`) MUST use the EIP-7883 gas schedule.
- Precompile gas costs are considered compute gas in the dual gas model.

### 8. System contracts

#### Rationale

MegaETH requires pre-deployed system contracts for oracle services and high-precision timestamps.

#### Semantics

- The oracle contract MUST be deployed at `0x6342000000000000000000000000000000000001` as a pre-execution state change in the first block of MiniRex activation.
- The high-precision timestamp oracle MUST be deployed at `0x6342000000000000000000000000000000000002` as a pre-execution state change in the first block of MiniRex activation.

## Invariants

- `I-1`: Overall gas cost MUST equal compute gas + storage gas.
- `I-2`: All three resource dimensions (compute gas, data size, KV updates) MUST be enforced independently.
- `I-3`: Transaction halted by a resource limit MUST preserve remaining gas for refund.
- `I-4`: When multiple volatile data categories are accessed, the most restrictive cap MUST apply.
- `I-5`: SLOAD on the oracle contract MUST always use cold access cost (2100 gas).
- `I-6`: Subcalls MUST receive at most 98/100 of remaining gas.

## References

- [Rex Specification](Rex.md)
- [MiniRex Behavior Details (Informative)](impl/MiniRex-Behavior-Details.md)
- [MiniRex Implementation References (Informative)](impl/MiniRex-Implementation-References.md)
- [Dual Gas Model](../docs/DUAL_GAS_MODEL.md)
- [Resource Accounting](../docs/RESOURCE_ACCOUNTING.md)
- [Block and Transaction Limits](../docs/BLOCK_AND_TX_LIMITS.md)
- [Oracle Service](../docs/ORACLE_SERVICE.md)
- [Mega System Transactions](../docs/MEGA_SYSTEM_TRANSACTION.md)
