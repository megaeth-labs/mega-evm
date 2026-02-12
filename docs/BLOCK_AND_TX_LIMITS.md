# Block and Transaction Limits

## Overview

MegaEVM implements a comprehensive resource limit system to prevent spam attacks and ensure fair resource allocation. The system enforces **7 types of limits**, each with both **transaction-level** and **block-level** variants, using a two-phase checking strategy to optimize block construction.

For more about how these limits are accounted, see [RESOURCE_ACCOUNTING.md](./RESOURCE_ACCOUNTING.md).

## The Seven Limit Types

### Pre-execution Limits

These limits can be determined before transaction execution and are checked early to reject oversized transactions quickly:

1. **Gas Limit**

   - **Tx-level**: Maximum gas a single transaction can declare
   - **Block-level**: Total gas available in the block
   - Checks traditional EVM gas consumption

2. **Transaction Size Limit**

   - **Tx-level**: Maximum encoded size of a transaction
   - **Block-level**: Total uncompressed transaction body size in a block
   - Applies to EIP-2718 encoded transaction size

3. **Data Availability (DA) Size Limit**
   - **Tx-level**: Maximum DA size for a single transaction
   - **Block-level**: Total compressed DA size in a block
   - Represents compressed size for data availability purposes
   - **Special Rule**: Deposit transactions are exempt from DA size limit checks

### Post-execution Limits

These limits depend on actual execution results and are enforced during/after transaction execution:

4. **Compute Gas Limit**

   - **Tx-level**: Maximum compute gas a single transaction can consume
   - **Block-level**: Total compute gas available in a block
   - Tracks actual computational cost during execution, separate from standard gas

5. **Data Size Limit**

   - **Tx-level**: Maximum data a single transaction can generate
   - **Block-level**: Total execution data in a block
   - Includes: transaction data, logs, storage writes, account updates, and contract code

6. **KV Update Limit**

   - **Tx-level**: Maximum storage updates a single transaction can perform
   - **Block-level**: Total storage updates in a block
   - Tracks: SSTORE operations and account updates

7. **State Growth Limit**
   - **Tx-level**: Maximum state growth a single transaction can create
   - **Block-level**: Total state growth in a block
   - Tracks: New accounts created and new storage slots written (net growth)
   - Uses a **net growth model**: clearing storage slots back to zero reduces the count

## Two-Level Enforcement

Each limit has two enforcement levels:

### Transaction-Level

Applies to individual transactions. Violations indicate an **invalid transaction** that should be rejected or marked as failed.

### Block-Level

Applies to cumulative resource usage across all transactions in a block. Violations indicate the transaction **doesn't fit** in the current block but may fit in a future block.

## Two-Phase Checking Strategy

### Phase 1: Pre-execution Checks (Limits 1-3)

**Checked**: Before transaction execution
**Purpose**: Fast rejection without expensive execution

**Transaction-level violation:**

- Transaction is **rejected permanently** (invalid)
- Cannot ever be included in any block
- Example: Transaction declares 50M gas when limit is 30M

**Block-level violation:**

- Transaction is **skipped**, try next transaction
- May fit in future blocks
- Example: Block has 5M gas remaining, transaction needs 10M

### Phase 2: Post-execution Checks (Limits 4-7)

**Checked**: During and after transaction execution
**Purpose**: Enforce limits based on actual execution results

**Transaction-level enforcement (during execution):**

- Transaction **halts** with OutOfGas error
- Remaining gas is **preserved** (not consumed)
- Transaction **fails** (status=0) but is **still included** in block
- Rationale: Failed transactions consume resources and must be recorded on-chain

**Block-level enforcement (before next transaction):**

- The last transaction that causes the block to exceed limits is **allowed to execute and be included**
- After that transaction completes, subsequent transactions are **rejected in pre-execution** before they execute
- Check if block limit has already been exceeded before executing the next transaction
- If yes: **Reject** transaction before execution, try next one
- Example: Block has 10MB data limit. TX1 uses 6MB (included), TX2 uses 8MB (included, total 14MB exceeds limit), TX3 is rejected before execution

## Block Building Workflow

When constructing a block, iterate through transactions in the mempool:

```
For each transaction:
  ├─ Step 1: Pre-execution validation
  │  ├─ Check tx-level limits (1-3, 4-7)
  │  │  └─ Tx-level violation? → Reject permanently
  │  ├─ Check pre-execution block-level limits (1-3)
  │  │  └─ Block-level violation? → Skip, try next
  │  └─ Check post-execution block-level limits (4-7)
  │     └─ Block already exceeded? → Skip, try next
  │
  ├─ Step 2: Execute transaction
  │  └─ If tx-level limits (4-7) exceeded → Transaction fails (status=0)
  │
  ├─ Step 3: Post-execution update
  │  └─ Update block usage counters (may exceed limits)
  │
  └─ Step 4: Commit transaction
     └─ Include in block (with success or failed receipt)
```

## Transaction Outcomes

### Successful Transaction

- **Execution**: Completes successfully
- **Receipt**: status=1, includes logs
- **Block Impact**: Counts toward all block limits
- **Next Action**: Continue to next transaction

### Failed Transaction (Tx-level Limit Exceeded)

- **Execution**: Halts with MegaHaltReason (OutOfGas)
- **Receipt**: status=0, no logs
- **Gas**: Remaining gas preserved (not consumed)
- **Block Impact**: Still counts toward block limits
- **Next Action**: If passes post-execution check → include in block

### Skipped Transaction (Block-level Limit Exceeded)

- **Execution**: Not executed (rejected in pre-execution)
- **Receipt**: Not generated
- **Block Impact**: No impact on block limits
- **Next Action**: Defer to future blocks, try next transaction
- **Note**: For post-execution limits (4-7), this occurs when a previous transaction already caused the block to exceed the limit

## Error Types and Actions

### Transaction-level Errors → Reject Permanently

These indicate invalid transactions that can never be included:

- `MegaTxLimitExceededError::TransactionGasLimit` - Gas limit too high
- `MegaTxLimitExceededError::TransactionEncodeSizeLimit` - Transaction too large
- `MegaTxLimitExceededError::DataAvailabilitySizeLimit` - DA size too large

### Block-level Errors → Skip and Try Next

These indicate the transaction doesn't fit in the current block:

- `MegaBlockLimitExceededError::ComputeGasLimit` - Would exceed block compute gas
- `MegaBlockLimitExceededError::TransactionDataLimit` - Would exceed block transactions data limit
- `MegaBlockLimitExceededError::KVUpdateLimit` - Would exceed block KV updates
- `MegaBlockLimitExceededError::StateGrowthLimit` - Would exceed block state growth
- `MegaBlockLimitExceededError::TransactionEncodeSizeLimit` - Would exceed block transactions encode size
- `MegaBlockLimitExceededError::DataAvailabilitySizeLimit` - Would exceed block DA size
- `BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas` - Insufficient gas

## Configuration for Different Specifications

### EQUIVALENCE Specification (Optimism Isthmus Compatible)

The EQUIVALENCE specification maintains full compatibility with Optimism Isthmus and does not impose additional resource limits beyond standard EVM gas limits.

**Required configuration:**

- Set `block_gas_limit` from the block environment
- All other limits remain unlimited (u64::MAX)

**Purpose:** Environments that want Optimism compatibility without additional MegaETH-specific restrictions.

### MINI_REX Specification (Enhanced with Additional Limits)

The MINI_REX specification introduces additional resource limits to prevent spam attacks and ensure fair resource allocation.

**Required configuration:**

- `block_gas_limit` - From block environment
- `tx_compute_gas_limit` - 1,000,000,000 gas (1 billion)
- `tx_data_limit` - 3,276,800 bytes (3.125 MB, 25% of block limit)
- `block_txs_data_limit` - 13,107,200 bytes (12.5 MB)
- `tx_kv_update_limit` - 125,000 operations (25% of block limit)
- `block_kv_update_limit` - 500,000 operations

**Additional limits (optional):**

- `tx_gas_limit` - Maximum gas per transaction (e.g., 30M gas)
- `tx_encode_size_limit` - Maximum transaction body size
- `block_txs_encode_size_limit` - Total transaction size in block
- `tx_da_size_limit` - Maximum DA size per transaction
- `block_da_size_limit` - Total DA size in block
- `block_compute_gas_limit` - Total compute gas in block
- `tx_state_growth_limit` - Maximum state growth per transaction
- `block_state_growth_limit` - Total state growth in block

### REX Specification (Production Limits)

The REX specification is the production-ready hardfork with higher transaction-level limits and state growth tracking.

**Required configuration:**

- `block_gas_limit` - From block environment
- `tx_compute_gas_limit` - 200,000,000 gas (200 million)
- `tx_data_limit` - 13,107,200 bytes (12.5 MB, same as block limit)
- `block_txs_data_limit` - 13,107,200 bytes (12.5 MB)
- `tx_kv_update_limit` - 500,000 operations (same as block limit)
- `block_kv_update_limit` - 500,000 operations
- `tx_state_growth_limit` - 1,000 new storage slots
- `block_state_growth_limit` - 1,000 new storage slots

**Key differences from MINI_REX:**

- Higher compute gas limit per transaction (200M vs 1B)
- Transaction-level data and KV update limits match block limits (allows single large transaction)
- Introduces state growth limits to track net new storage slots created
- Uses a new dynamic storage gas model with SALT bucket capacity scaling

**Additional limits (optional):**

- `tx_gas_limit` - Maximum gas per transaction (e.g., 30M gas)
- `tx_encode_size_limit` - Maximum transaction body size
- `block_txs_encode_size_limit` - Total transaction size in block
- `tx_da_size_limit` - Maximum DA size per transaction
- `block_da_size_limit` - Total DA size in block
- `block_compute_gas_limit` - Total compute gas in block

### REX3 Specification (Increased Oracle Access Compute Gas Limit)

The REX3 specification inherits all REX limits but increases the oracle access compute gas limit from 1M to 10M.

**Changes from REX/REX1/REX2:**

- `oracle_access_compute_gas_limit` - 10,000,000 gas (10M, up from 1M)

All other limits remain the same as REX.

## Key Design Principles

### 1. Skip vs. Reject

- **Reject**: Transaction is permanently invalid (exceeds tx-level limits)
- **Skip**: Transaction doesn't fit now but may fit later (exceeds block-level limits)

### 2. Failed Transactions Are Included

Failed transactions (that exceed tx-level limits 4-6) are still included in blocks because:

- They consumed computational resources during execution
- Including them ensures attackers pay for wasted resources
- They need to be recorded on-chain with their receipts
- They occupy block space and count toward block limits

### 3. Two-Phase Optimization

- **Pre-execution**: Fast rejection of obviously oversized transactions
- **Post-execution**: Precise enforcement based on actual resource usage
- Minimizes wasted computation while ensuring accurate limit enforcement

### 4. Block Utilization and Last Transaction Exceeding

The limit checking strategy optimizes block utilization:

- **Pre-execution limits (1-3)**: Transactions that would exceed are rejected before execution
- **Post-execution limits (4-7)**: The first transaction that causes the block to exceed is **allowed and included**
  - This maximizes block utilization by not wasting the execution of a valid transaction
  - Subsequent transactions are rejected in pre-execution to prevent further exceeding

### 5. Deposit Transaction Exemptions

Deposit transactions (Optimism Layer 1 to Layer 2 deposits) receive special treatment:

- **DA Size Exemption**: Exempt from both transaction-level and block-level DA size limit checks
- **Rationale**: Deposit transactions are not posted to DA since they are already posted on L1.
- **Other Limits**: Still subject to gas, tx size, compute gas, data size, and KV update limits

## Best Practices for Block Builders

### 1. Transaction Ordering

Order transactions to maximize block utilization:

- Sort by gas price (higher priority)
- Consider estimated resource usage (prefer smaller transactions when close to limits)

### 2. Early Rejection

Pre-filter transactions before expensive execution:

- Check tx-level limits before attempting execution
- Discard permanently invalid transactions immediately

### 3. Resource Estimation

Track remaining block capacity to avoid unnecessary execution:

- Monitor cumulative usage of all limit types
- Skip transactions that obviously won't fit

### 4. Deferred Transaction Management

Maintain separate queues for different transaction states:

- High priority transactions
- Transactions waiting for more block gas
- Transactions waiting for more block resources (data/KV updates)

## Resource Tracking Details

### Compute Gas Tracking

- Tracks gas consumption from EVM instructions during execution
- Separate from standard gas limit
- Used to prevent computationally expensive transactions

### Data Size Tracking

Tracks cumulative data generated:

- Base transaction data: 110 bytes + calldata + access lists + authorizations
- Caller/authority account updates: 40 bytes each
- Log data: per log entry
- Storage writes: 40 bytes when original_value ≠ new_value
- Account updates: 40 bytes for value transfers and creates
- Contract code: size of deployed bytecode

### KV Update Tracking

Tracks cumulative storage operations:

- Transaction caller updates
- Authority updates (EIP-7702)
- Storage writes when original_value ≠ new_value
- Account updates from value transfers and creates

## Monitoring and Metrics

Track these metrics for block construction optimization:

**Transaction counts:**

- Attempted transactions
- Included successful transactions
- Included failed transactions
- Rejected invalid transactions
- Skipped transactions (block limit exceeded)

**Resource utilization:**

- Gas utilization (block_gas_used / block_gas_limit)
- Data utilization (block_data_used / block_txs_data_limit)
- KV update utilization (block_kv_updates / block_kv_update_limit)

**Performance:**

- Average execution time per transaction
- Wasted execution time (transactions that were skipped after execution)

## Summary

The MegaEVM limit system provides a robust framework for resource management:

1. **Six limit types** cover all critical resources
2. **Two-level enforcement** distinguishes between invalid transactions and capacity issues
3. **Two-phase checking** optimizes performance while ensuring accuracy
4. **Failed transactions are included** to ensure resource accountability

This design prevents spam attacks, ensures fair resource allocation, and enables efficient block construction while maintaining compatibility with existing EVM semantics.
