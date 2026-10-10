# MegaETH EVM Architecture

This document provides detailed technical specifications and implementation details for the MegaETH EVM.

## Table of Contents

- [EVM Versions](#evm-versions)
  - [EQUIVALENCE](#equivalence)
  - [MINI_REX](#mini_rex)
    - [Dynamic Gas Cost System](#dynamic-gas-cost-system)
    - [Compute Gas Tracking and Limiting](#compute-gas-tracking-and-limiting)
    - [LOG Opcodes with Dual Gas Model](#log-opcodes-with-dual-gas-model)
    - [SELFDESTRUCT Opcode Disabled](#selfdestruct-opcode-disabled)
    - [Enhanced Transaction Processing](#enhanced-transaction-processing)
    - [Contract Size Limits](#contract-size-limits)
    - [Multidimensional Resource Limits](#multidimensional-resource-limits)
- [General Features](#general-features)
- [Block Environment Access Tracking](#block-environment-access-tracking)
- [Beneficiary Access Tracking](#beneficiary-access-tracking)

## EVM Versions

The implementation exposes multiple EVM versions (`MegaSpecId`). String names and hardfork-to-spec
mapping live in `crates/mega-evm/src/evm/spec.rs` and `crates/mega-evm/src/block/hardfork.rs`.

Available specs: `EQUIVALENCE`, `MINI_REX`, `MINI_REX_1`, `MINI_REX_2`, `REX`, `REX1`, `REX2`, `REX3`, `REX4`, `REX5`, `REX6` (`MINI_REX_1` and `MINI_REX_2` are alias rungs executing `EQUIVALENCE` and `MINI_REX` behavior respectively).

This page details only the first few; the authoritative per-spec behavior is the specification under
`docs/spec/`, whose upgrade pages cover every spec.

### EQUIVALENCE

Baseline spec that maintains equivalence with Optimism Isthmus EVM.
It is the oldest spec, not the default one — `MegaSpecId::default()` is the latest spec.

### MINI_REX

The EVM version used for `Mini-Rex` hardfork of MegaETH.

**Major Features**:

- **Multidimensional Gas Model**: Independent limits for compute gas (1B), data size (3.125 MB), and KV updates (125K)
- **Compute Gas Tracking**: Separate tracking for computational costs with gas detention for volatile data access
- **Dynamic Gas Costs**: SALT bucket-based scaling for storage and account operations
- **Split LOG Costs**: Compute gas (standard) + storage gas (10× multiplier) for independent resource pricing
- **SELFDESTRUCT Prohibition**: Complete disabling of SELFDESTRUCT opcode
- **Contract Size Increases**: 512 KB contracts, 536 KB initcode
- **Gas Detention**: Block env, beneficiary, and oracle access trigger gas limiting (20M/1M) with refunds

#### Dynamic Gas Cost System

**Files**: `crates/mega-evm/src/external/gas.rs`, `crates/mega-evm/src/evm/instructions.rs`

**Purpose**: Prevents state bloat by scaling gas costs based on SALT bucket capacity.

**Implementation**:

- **Storage Operations**: `SSTORE_SET_GAS × (bucket_capacity / MIN_BUCKET_SIZE)`
- **Account Creation**: `NEW_ACCOUNT_GAS × (bucket_capacity / MIN_BUCKET_SIZE)`
- **Bucket Mapping**: Storage uses `address || slot_key`, accounts use `address`

**Affected Operations**: SSTORE, CREATE, CREATE2, CALL (to new accounts), transaction validation

#### Compute Gas Tracking and Limiting

**Files**: `crates/mega-evm/src/limit/limit.rs`, `crates/mega-evm/src/limit/compute_gas.rs`

**Purpose**: Separate tracking for computational work to enable independent resource pricing and gas detention for volatile data access.

**Implementation Details**:

- **Compute Gas Limit**: 1,000,000,000 gas per transaction (separate from standard gas limit)
- **Tracking**: Monitors all gas consumed during EVM instruction execution across nested calls
- **LOG Operations**: Only compute portion tracked (375 base + 375/topic + 8/byte)
- **Enforcement**: Halts with OutOfGas when limit exceeded, remaining gas preserved
- **Gas Detention**: Volatile data access triggers immediate gas limiting:
  - Block environment/beneficiary: 20M gas limit
  - Oracle contract: 1M gas limit
  - Most restrictive limit applies when multiple types accessed
  - Excess gas detained and refunded at transaction end

**Affected Operations**: All EVM instructions contribute to compute gas tracking

#### LOG Opcodes with Dual Gas Model

**Files**: `crates/mega-evm/src/evm/instructions.rs`

**Purpose**: Split LOG costs into compute gas (for EVM execution) and storage gas (for persistence) to enable independent resource pricing.

**Implementation Details**:

- **Compute Gas** (tracked in compute gas limit):
  - Base: 375 gas, Topics: 375 gas/topic, Data: 8 gas/byte
- **Storage Gas** (tracked in standard gas limit):
  - Topics: 3,750 gas/topic (10× multiplier), Data: 80 gas/byte (10× multiplier)
- **Total Cost**: Compute gas + Storage gas
- **Data Limit**: Enforces 3.125 MB transaction data limit
- **Enforcement**: Halts with OutOfGas when either limit exceeded

**Affected Opcodes**: LOG0, LOG1, LOG2, LOG3, LOG4

#### SELFDESTRUCT Opcode Disabled

**Files**: `crates/mega-evm/src/evm/instructions.rs`

**Purpose**: Prevents permanent contract destruction in MINI_REX spec.

**Behavior**:

- Halts the frame and consumes all its remaining gas (`InvalidFEOpcode`, or `OutOfGas` when the frame cannot pay SELFDESTRUCT's static gas)
- Maintains contract state integrity
- Prevents malicious contract destruction

**Implementation**:

- `crates/mega-evm/src/evm/instructions.rs` (`mini_rex::instruction_table` maps `SELFDESTRUCT` to
  `control::invalid`; `rex2::instruction_table` re-enables it later)

#### Enhanced Transaction Processing

**Files**: `crates/mega-evm/src/evm/execution.rs`, `crates/mega-evm/src/evm/instructions.rs`, `crates/mega-evm/src/limit/data_size.rs`, `crates/mega-evm/src/limit/kv_update.rs`

**Features**:

- **Calldata Storage Gas**: 10× multiplier on standard token and floor costs (see
  `constants::mini_rex::CALLDATA_STANDARD_TOKEN_STORAGE_GAS` and
  `constants::mini_rex::CALLDATA_STANDARD_TOKEN_STORAGE_FLOOR_GAS`)
- **Data Size Tracking**: Comprehensive tracking of transaction data generation
- **KV Update Tracking**: Sophisticated counting of state changes with refund logic
- **Limit Enforcement**: Halts with OutOfGas when limits exceeded

#### Contract Size Limits

**Files**: `crates/mega-evm/src/constants.rs`, `crates/mega-evm/src/evm/context.rs`

**Change**: Dramatically increased contract size limits for MINI_REX spec.

**Limits**:

- `MAX_CONTRACT_SIZE`: 512 KB (vs standard 24 KB) - ~21x increase
- `MAX_INITCODE_SIZE`: 536 KB (512 KB + 24 KB buffer) - ~11x increase
- `constants::mini_rex::CODEDEPOSIT_STORAGE_GAS`: 10,000 storage gas per byte of deployed code, charged on top of the standard 200 gas per byte

#### Multidimensional Resource Limits

**Files**: `crates/mega-evm/src/limit/limit.rs`, `crates/mega-evm/src/limit/compute_gas.rs`, `crates/mega-evm/src/limit/data_size.rs`, `crates/mega-evm/src/limit/kv_update.rs`

**Transaction Limits**:

- **Compute Gas**: 1,000,000,000 gas maximum (separate from standard gas limit)
- **Data Size**: 3.125 MB maximum (25% of 12.5 MB block limit)
- **KV Updates**: 125,000 operations maximum (25% of 500K block limit)

**Block Limits**:

- **Block Data**: 12.5 MB maximum
- **Block KV Updates**: 500,000 operations maximum

**Tracking**:

- **Compute Gas**: Cumulative gas consumed during EVM execution across all frames
- **Data Size**: Frame-aware tracking for proper revert handling with discardable vs non-discardable categories
- **KV Updates**: Sophisticated logic tracks net changes, not all operations
- **Enforcement**: When any limit exceeded, transaction halts with OutOfGas and remaining gas is preserved

## General Features

Features that are available regardless of EVM versions.

## Block Environment Access Tracking

**Files**: `crates/mega-evm/src/access/`, `crates/mega-evm/src/evm/context.rs`, `crates/mega-evm/src/evm/host.rs`

**Purpose**: Tracks which block environment fields are accessed during execution to enable runtime conflict detection in parallel execution.

**Tracked Fields**:

- Block number (`NUMBER` opcode)
- Timestamp (`TIMESTAMP` opcode)
- Base fee (`BASEFEE` opcode)
- Difficulty (`DIFFICULTY` opcode)
- Gas limit (`GASLIMIT` opcode)
- Coinbase (`COINBASE` opcode)
- Prevrandao (`PREVRANDAO` opcode)
- Block hash (`BLOCKHASH` opcode)
- Blob base fee (`BLOBBASEFEE` opcode)
- Blob hash (`BLOBHASH` opcode)

**Usage Example**:

```rust
// Check which block environment fields were accessed
let accesses = context.get_block_env_accesses();
println!("Accessed fields: {:?}", accesses);

// Reset tracking for next transaction
context.reset_volatile_data_access();
```

**Benefits**:

- Enables selective block data fetching
- Reduces unnecessary data access
- Improves performance for contracts that don't use block data

## Beneficiary Access Tracking

**Files**: `crates/mega-evm/src/evm/context.rs`, `crates/mega-evm/src/evm/host.rs`, `crates/mega-evm/src/access/tracker.rs`

**Purpose**: Tracks when a transaction accesses the block beneficiary's balance or account state
(balance/code reads, or when caller/recipient is the beneficiary).

**Tracked Operations**:

- Balance queries (`BALANCE` opcode)
- Code access (`EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`)
- Beneficiary as transaction caller or recipient

**Usage Example**:

```rust
// Check if beneficiary was accessed
if context.volatile_data_tracker.borrow().has_accessed_beneficiary_balance() {
    println!("Transaction accessed block beneficiary");
}

// Reset for next transaction
context.reset_volatile_data_access();
```

**Benefits**:

- Enables parallel execution optimization by identifying transactions that access the beneficiary, which can block other transactions and cause longer execution times
