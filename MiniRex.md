# MiniRex Hardfork Specification

This document summarizes all semantic changes introduced in the **MiniRex** hardfork for the MegaETH EVM.

## Overview

The MiniRex hardfork introduces significant changes to the MegaETH EVM to prevent data bombs, key-value update bombs, and contract destruction, while increasing limits for contract and initcode sizes. It corresponds to `MegaSpecId::MINI_REX` in the codebase.

## Major Changes

### 1. Contract Size Limits

**Increased Maximum Contract Size:**

- **Standard EVM (EIP-170)**: 24,576 bytes (24 KB)
- **MiniRex**: 524,288 bytes (512 KB)
- **Location**: `constants::mini_rex::MAX_CONTRACT_SIZE = 512 * 1024`

**Increased Maximum Initcode Size:**

- **Standard EVM (EIP-3860)**: 49,152 bytes (48 KB)
- **MiniRex**: 536,576 bytes (512 KB + 24 KB)
- **Location**: `constants::mini_rex::MAX_INITCODE_SIZE = MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE`

### 2. SELFDESTRUCT Opcode Prohibition

**Complete Disabling of SELFDESTRUCT:**

- **Behavior**: SELFDESTRUCT opcode now halts execution with `InvalidFEOpcode` error
- **Rationale**: Prevents permanent contract destruction and maintains state integrity
- **Implementation**: `self.inner.insert_instruction(SELFDESTRUCT, control::invalid)`

### 3. Gas Cost Increases

The MiniRex hardfork significantly increases gas costs for various operations to prevent abuse:

#### Storage Operations (SSTORE)

- **New Gas Cost**: 2,000,000 gas (vs. standard 20,000)
- **Dynamic Scaling**: Cost multiplier = `bucket_capacity / MIN_BUCKET_SIZE` (doubles as bucket doubles)
- **Final Cost**: `SSTORE_SET_GAS × multiplier`
- **Constant**: `constants::mini_rex::SSTORE_SET_GAS = 2_000_000`

#### Account Creation

- **New Account Creation**: 2,000,000 gas (base cost)
- **Dynamic Scaling**: Cost multiplier = `bucket_capacity / MIN_BUCKET_SIZE` (same as SSTORE)
- **Final Cost**: `NEW_ACCOUNT_GAS × multiplier`
- **Contract Creation**: Additional 2,000,000 gas on top of new account cost
- **Constants**:
  - `NEW_ACCOUNT_GAS = 2_000_000`
  - `CREATE_GAS = 2_000_000`

#### Code Deposit

- **Standard Cost**: 200 gas per byte (EVM default)
- **Additional Cost**: ~62,300 gas per byte (MiniRex addition)
- **Total Cost**: ~62,500 gas per byte (`2_000_000 / 32`)

#### Logging Operations

- **Log Data**: 100x standard cost (800 gas per byte vs. 8)
- **Log Topics**: 100x standard cost (37,500 gas per topic vs. 375)
- **Constants**:
  - `LOG_DATA_GAS = LOGDATA * 100`
  - `LOG_TOPIC_GAS = LOGTOPIC * 100`

#### Call Data

- **Standard Tokens**: 100x increase (400 gas per token vs. 4)
- **EIP-7623 Floor**: 100x increase for transaction data floor cost
- **Constants**:
  - `CALLDATA_STANDARD_TOKEN_ADDITIONAL_GAS`
  - `CALLDATA_STANDARD_TOKEN_ADDITIONAL_FLOOR_GAS`

### 4. Data and KV Update Limits

**Block-Level Limits:**

- **Maximum Block Data**: 12.5 MB (`BLOCK_DATA_LIMIT = 12 * 1024 * 1024 + 512 * 1024`)
- **Maximum Block KV Updates**: 500,000 operations (`BLOCK_KV_UPDATE_LIMIT = 500_000`)

**Transaction-Level Limits:**

- **Data Limit**: 3.125 MB (25% of block limit)
- **KV Update Limit**: 1,000 key-value operations
- **Constants**:
  - `TX_DATA_LIMIT = BLOCK_DATA_LIMIT * 25 / 100`
  - `TX_KV_UPDATE_LIMIT = 1000`

### 5. Additional Limit Enforcement

The MiniRex spec introduces comprehensive tracking and enforcement of data generation and key-value updates:

#### Data Size Tracking

**Transaction Data (Non-discardable):**

- Base transaction: 110 bytes (`BASE_TX`)
- Calldata: `tx.input().len()` bytes
- Access list: Sum of `access.size()` for each access list entry
- EIP-7702 authorizations: `authorization_count × 101` bytes (`AUTHORIZATION = 101`)
- Caller account update: 40 bytes (`ACCOUNT_INFO_WRITE_SIZE`)
- EIP-7702 authority account updates: `authorization_count × 40` bytes

**Log Data (Discardable on revert):**

- Formula: `num_topics × 32 + data.len()` bytes
- Topics: 32 bytes each
- Data: Variable length

**Storage Operations (Discardable on revert):**

- **SSTORE**: 40 bytes (`8 + 32`) - salt key + salt value delta
  - Added when: original value ≠ new value AND first write to slot in transaction
  - Refunded when: slot is reset back to its original value
  - No data when: rewriting slot to same new value multiple times

**Account Updates (Discardable on revert):**

- Formula: 40 bytes (`8 + 32`) - salt key + salt value delta
- Components: salt key (8) + account info delta (32)

**Contract Code (Discardable on revert):**

- Size: `contract_code.len()` bytes (actual deployed bytecode size)

#### KV Update Counting

**Transaction Start (Non-discardable):**

- Caller account update: 1 KV update
- EIP-7702 authorizations: `authorization_count` KV updates (1 per authorization)

**Storage Operations:**

- **SSTORE**: 1 KV update when original value ≠ new value AND first write to slot in transaction
- Refunded (-1 KV update) when slot is reset back to its original value
- No KV update when rewriting slot to same new value multiple times

**Account Operations (Discardable on revert):**

- **CREATE/CREATE2**: 1 KV update for created account
- **CALL with transfer**: 2 KV updates (caller + callee accounts)
- **CALL without transfer**: 0 KV updates

#### Enforcement Mechanism

- **Halt Condition**: When limits exceeded, transaction halts with `OutOfGas` instruction result
- **Gas Consumption**: All remaining gas is consumed when limits are exceeded

## Implementation Details

### Specification Mapping

The semantic of MiniRex spec is inherited and customized from:

- **MiniRex** → **Optimism Isthmus** → **Ethereum Prague**

### Instruction Overrides

The following opcodes have custom implementations in MiniRex:

- `LOG0`, `LOG1`, `LOG2`, `LOG3`, `LOG4`: Enhanced with tx data size limit protection
- `SELFDESTRUCT`: Completely disabled (maps to `invalid` instruction)
- `SSTORE`: Increased gas cost and limit enforcement
- `CREATE`, `CREATE2`: Increased gas cost and limit enforcement
- `CALL`, `CALLCODE`: Enhanced new account gas cost and limit enforcement

### Gas Cost Oracle

- **ExternalEnvOracle**: An oracle providing external environment information is introduced to EVM to provide SALT bucket metadata.
- **Dynamic Pricing**: Gas costs scale with SALT bucket capacity

## Migration Impact

### For Contracts

- **Large Contracts**: Can now deploy contracts up to 512 KB (previously 24 KB)
- **SELFDESTRUCT**: Any contract using SELFDESTRUCT will fail after MiniRex activation
- **Gas Costs**: Operations become significantly more expensive, especially storage writes and logging

### For Applications

- **Transaction Limits**: Must respect new data and KV update limits
- **Gas Estimation**: Need to account for dramatically increased gas costs
- **Error Handling**: Must handle new limit-exceeded error conditions

## Constants Summary

| Operation           | Standard Cost | MiniRex Cost  | Multiplier |
| ------------------- | ------------- | ------------- | ---------- |
| SSTORE_SET          | 20,000        | 2,000,000     | 100x       |
| NEW_ACCOUNT         | 25,000        | 2,000,000     | 80x        |
| CREATE              | 32,000        | 2,000,000     | ~62x       |
| LOG_DATA (per byte) | 8             | 800           | 100x       |
| LOG_TOPIC           | 375           | 37,500        | 100x       |
| CALLDATA (per byte) | 4             | 400           | 100x       |
| MAX_CONTRACT_SIZE   | 24,576 bytes  | 524,288 bytes | ~21x       |
| MAX_INITCODE_SIZE   | 49,152 bytes  | 536,576 bytes | ~11x       |

## Conclusion

The MiniRex hardfork represents a major evolution of the MegaETH EVM, prioritizing system stability and preventing abuse through comprehensive limit enforcement and strategic gas cost increases, while enabling larger contract deployments for enhanced functionality.
