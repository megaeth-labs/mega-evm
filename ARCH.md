# MegaETH EVM Architecture

This document provides detailed technical specifications and implementation details for the MegaETH EVM.

## Table of Contents

- [EVM Specifications](#evm-specifications)
- [Instruction Set Modifications](#instruction-set-modifications)
- [Contract Size Limits](#contract-size-limits)
- [Block Environment Access Tracking](#block-environment-access-tracking)
- [Context and Handler Extensions](#context-and-handler-extensions)
- [Project Structure](#project-structure)
- [Advanced Usage](#advanced-usage)
- [Dependencies](#dependencies)

## EVM Versions 

The implementation introduces two EVM versions (`SpecId`):

### EQUIVALENCE

Default spec that maintains equivalence with Optimism Isthmus EVM.

### MINI_REX

The EVM version used for `Mini-Rex` hardfork of MegaETH. **See [MiniRex.md](./MiniRex.md) for complete specification.**

**Major Features**:
- **Dynamic Gas Costs**: SALT bucket-based scaling for storage and account operations
- **100x Gas Increases**: LOG operations, calldata costs dramatically increased
- **SELFDESTRUCT Prohibition**: Complete disabling of SELFDESTRUCT opcode
- **Contract Size Increases**: 512 KB contracts, 536 KB initcode
- **Data/KV Limits**: Transaction limits of 3.125 MB data and 1,000 KV updates

#### Dynamic Gas Cost System

**Files**: `crates/mega-evm/src/gas.rs`, `crates/mega-evm/src/instructions.rs`

**Purpose**: Prevents state bloat by scaling gas costs based on SALT bucket capacity.

**Implementation**:
- **Storage Operations**: `SSTORE_SET_GAS × (bucket_capacity / MIN_BUCKET_SIZE)`
- **Account Creation**: `NEW_ACCOUNT_GAS × (bucket_capacity / MIN_BUCKET_SIZE)`
- **Bucket Mapping**: Storage uses `address || slot_key`, accounts use `address`

**Affected Operations**: SSTORE, CREATE, CREATE2, CALL (to new accounts), transaction validation

#### LOG Opcodes with 100x Gas Cost Increase

**Files**: `crates/mega-evm/src/instructions.rs`

**Purpose**: Prevents spam attacks through dramatically increased log costs and data limit enforcement.

**Implementation Details**:
- **LOG Topics**: 37,500 gas per topic (vs 375) - 100x increase
- **LOG Data**: 800 gas per byte (vs 8) - 100x increase
- **Data Limit**: Enforces 3.125 MB transaction data limit
- **Enforcement**: Halts with OutOfGas when limit exceeded

**Affected Opcodes**: LOG0, LOG1, LOG2, LOG3, LOG4

#### SELFDESTRUCT Opcode Disabled

**Files**: `crates/mega-evm/src/instructions.rs`

**Purpose**: Prevents permanent contract destruction in MINI_REX spec.

**Behavior**: 
- Returns `InvalidFEOpcode` when executed
- Maintains contract state integrity
- Prevents malicious contract destruction

**Implementation**:
```rust
self.inner.insert_instruction(SELFDESTRUCT, control::invalid);
```

#### Enhanced Transaction Processing

**Files**: `crates/mega-evm/src/handler.rs`, `crates/mega-evm/src/limit/`

**Features**:
- **Calldata Gas**: 400 gas per token (vs 4) - 100x increase
- **Data Size Tracking**: Comprehensive tracking of transaction data generation
- **KV Update Tracking**: Sophisticated counting of state changes with refund logic
- **Limit Enforcement**: Halts with OutOfGas when limits exceeded

#### Contract Size Limits

**Files**: `crates/mega-evm/src/constants.rs`, `crates/mega-evm/src/spec.rs`

**Change**: Dramatically increased contract size limits for MINI_REX spec.

**Limits**:
- `MAX_CONTRACT_SIZE`: 512 KB (vs standard 24 KB) - ~21x increase
- `MAX_INITCODE_SIZE`: 536 KB (512 KB + 24 KB buffer) - ~11x increase
- `CODEDEPOSIT_COST`: ~62,500 gas per byte (vs 200) - ~312x increase

#### Data and KV Update Limits

**Files**: `crates/mega-evm/src/limit/`

**Limits**:
- **Transaction Data**: 3.125 MB maximum
- **KV Updates**: 1,000 operations maximum
- **Block Data**: 12.5 MB maximum

**Tracking**:
- Frame-aware tracking for proper revert handling
- Sophisticated logic tracks net changes, not all operations
- Separate discardable vs non-discardable data categories

## General Features

Features that are avaialble regardless of EVM versions.

## Block Environment Access Tracking

**Files**: `crates/mega-evm/src/block.rs`, `crates/mega-evm/src/evm.rs`

**Purpose**: Tracks which block environment fields are accessed during execution to enable runtime conflict detection in parallel execution.

**Tracked Fields**:
- Block number (`NUMBER` opcode)
- Timestamp (`TIMESTAMP` opcode)
- Base fee (`BASEFEE` opcode)
- Difficulty (`DIFFICULTY` opcode)
- Gas limit (`GASLIMIT` opcode)
- Chain ID (`CHAINID` opcode)
- Coinbase (`COINBASE` opcode)

**Usage Example**:
```rust
// Check which block environment fields were accessed
let accesses = evm.get_block_env_accesses();
println!("Accessed fields: {:?}", accesses);

// Reset tracking for next transaction
evm.reset_block_env_access();
```

**Benefits**:
- Enables selective block data fetching
- Reduces unnecessary data access
- Improves performance for contracts that don't use block data

## Beneficiary Access Tracking

**Files**: `crates/mega-evm/src/context.rs`, `crates/mega-evm/src/host.rs`

**Purpose**: Tracks when a transaction accesses the block beneficiary's balance or account state. Any action that causes `ResultAndState` to contain the beneficiary will be marked as beneficiary access.

**Tracked Operations**:
- Balance queries (`BALANCE` opcode)
- Code access (`EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`)
- Beneficiary as transaction caller or recipient

**Usage Example**:
```rust
// Check if beneficiary was accessed
if evm.ctx_ref().has_accessed_beneficiary_balance() {
    println!("Transaction accessed block beneficiary");
}

// Reset for next transaction
evm.ctx_mut().reset_block_env_access();
```

**Benefits**:
- Enables parallel execution optimization by identifying transactions that access the beneficiary, which can block other transactions and cause longer execution times