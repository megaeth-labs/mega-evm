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

The EVM version used for `Mini-Rex` hardfork of MegaETH.

- **Features**: 
  - Quadratic LOG data cost, increased LOG topic cost.
  - Disabled SELFDESTRUCT
  - Increased contract size limits

#### LOG Opcodes with Quadratic Data Cost

**Files**: `crates/mega-evm/src/instructions.rs`

**Purpose**: Prevents spam attacks through expensive log operations by implementing quadratic cost scaling.

**Implementation Details**:
- **Linear Cost** (data ≤ 4KB): `data_length * LOGDATA`
- **Quadratic Cost** (data > 4KB): `4096 * LOGDATA + (data_length - 4096)²`

**Formula Breakdown**:
```rust
let total_data_size = previous_total_data_size + len;
if total_data_size <= 4096 {
    // Linear cost
    LOGDATA * len
} else if previous_total_data_size <= 4096 {
    // Mixed linear + quadratic cost
    let linear_cost_len = 4096 - previous_total_data_size;
    let linear_cost = LOGDATA * linear_cost_len;
    let quadratic_cost_len = len - linear_cost_len;
    let quadratic_cost = quadratic_cost_len²;
    linear_cost + quadratic_cost
} else {
    // Pure quadratic cost
    (total_data_size + previous_total_data_size) * len
}
```

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

#### Contract Size Limits

**Files**: `crates/mega-evm/src/spec.rs`

**Change**: Increased contract size limits for MINI_REX spec to support larger, more complex contracts.

**Limits**:
- `MAX_CONTRACT_SIZE`: 512 KB (vs standard 24 KB)
- `MAX_INITCODE_SIZE`: 536 KB (512 KB + 24 KB additional)
- `ADDITIONAL_INITCODE_SIZE`: 24 KB

**Constants**:
```rust
pub const MAX_CONTRACT_SIZE: usize = 512 * 1024;
pub const ADDITIONAL_INITCODE_SIZE: usize = 24 * 1024;
pub const MAX_INITCODE_SIZE: usize = MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE;
```

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