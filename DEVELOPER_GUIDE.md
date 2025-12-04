# MegaETH Developer Guide

A comprehensive guide for smart contract developers building on MegaETH using the REX specification.

## Table of Contents

1. [Introduction](#1-introduction)
2. [Key Differences at a Glance](#2-key-differences-at-a-glance)
3. [Gas Model](#3-gas-model)
4. [Resource Limits](#4-resource-limits)
5. [Volatile Data Access](#5-volatile-data-access)
6. [Contract Deployment and Destruction](#6-contract-deployment-and-destruction)
7. [System Contracts & Oracle Service](#7-system-contracts--oracle-service)
8. [Precompiles](#8-precompiles)
9. [Reference](#9-reference)

---

## 1. Introduction

MegaETH is a high-performance Ethereum Layer 2 that achieves exceptional throughput through a hyper-optimized sequencer. The **MegaEVM** is MegaETH's execution environment, fully compatible with Ethereum smart contracts while introducing optimizations for the unique characteristics of MegaETH's architecture.

### Compatibility

MegaEVM builds on established standards. The REX specification is based on Optimism Isthmus, which in turn inherits from Ethereum Prague.

This means:

- All standard Solidity contracts work on MegaETH
- Standard development tools (Hardhat, Foundry, Remix) are compatible
- Existing Ethereum libraries and patterns apply

### What's Different?

MegaETH's low fees and high gas limits create new opportunities but also require some adjustments:

- **Dual Gas Model**: Transactions pay both compute gas and storage gas
- **Resource Limits**: Four independent limits prevent abuse while enabling high throughput
- **Larger Contracts**: Deploy contracts up to 512 KB (vs 24 KB on Ethereum)
- **Modified Gas Forwarding**: Subcalls receive at most 98/100 of remaining gas (vs 63/64)

---

## 2. Key Differences at a Glance

| Feature             | Ethereum     | MegaETH (REX)                          |
| ------------------- | ------------ | -------------------------------------- |
| Max contract size   | 24 KB        | **512 KB**                             |
| Max initcode size   | 48 KB        | **536 KB**                             |
| Gas forwarding rule | 63/64        | **98/100**                             |
| SELFDESTRUCT        | Enabled      | **Disabled**                           |
| Gas model           | Single (gas) | **Dual (compute + storage)**           |
| Resource limits     | Gas only     | **4 dimensions**                       |
| Base intrinsic gas  | 21,000       | **60,000** (21K compute + 39K storage) |

---

## 3. Gas Model

### 3.1 Overview

MegaETH uses a **dual gas model** that separates costs into two categories:

```
Total Gas = Compute Gas + Storage Gas
```

- **Compute Gas**: Standard EVM execution costs (same as Ethereum)
- **Storage Gas**: Additional costs for operations that create persistent data

This separation keeps computation affordable while appropriately pricing storage-intensive operations.

### 3.2 Storage Gas Costs

| Operation                 | Storage Gas Cost | Notes                                 |
| ------------------------- | ---------------- | ------------------------------------- |
| **Transaction intrinsic** | 39,000 (flat)    | Added to every transaction            |
| **SSTORE (0→non-zero)**   | 20,000 × (m-1)   | Only for zero-to-non-zero writes      |
| **Account creation**      | 25,000 × (m-1)   | Value transfer to empty account       |
| **Contract creation**     | 32,000 × (m-1)   | CREATE/CREATE2 operations             |
| **Code deposit**          | 10,000/byte      | Per byte of deployed bytecode         |
| **LOG topic**             | 3,750/topic      | Per topic in event                    |
| **LOG data**              | 80/byte          | Per byte of event data                |
| **Calldata (zero byte)**  | 40/byte          | Per zero byte in tx input             |
| **Calldata (non-zero)**   | 160/byte         | Per non-zero byte in tx input         |
| **Floor (zero byte)**     | 100/byte         | EIP-7623 floor cost for zero byte     |
| **Floor (non-zero)**      | 400/byte         | EIP-7623 floor cost for non-zero byte |

_`m` = bucket multiplier (explained below)_

_Note: EIP-7623 defines a minimum "floor" gas cost for calldata. If the floor cost exceeds the regular calldata cost, the floor is charged instead._

### 3.3 What is the Bucket Multiplier?

Accounts and their storage are stored in `bucket`s of MegaETH's `SALT` state trie. The cost of writing new data (excluding chaning existing data) to the buckets induces varying storage gas costs depending on the capacity of the bucket measured by a **bucket multiplier**:

```
multiplier = bucket_capacity / MIN_BUCKET_SIZE
```

**In practice:**

- Fresh storage (new contracts, new slots) typically has multiplier = 1, if the bucket is not expanded to meet heavy storage needs.
- At multiplier = 1, the formula `base × (m-1)` = 0, meaning **zero additional storage gas**
- As buckets fill up, they get expanded, the multiplier increases and costs rise

**Example costs at different multipliers:**

| Operation           | m=1 | m=2    | m=4    |
| ------------------- | --- | ------ | ------ |
| SSTORE (0→non-zero) | 0   | 20,000 | 60,000 |
| Account creation    | 0   | 25,000 | 75,000 |
| Contract creation   | 0   | 32,000 | 96,000 |

### 3.4 Gas Estimation Tips

1. **Use MegaETH's native gas estimation APIs** - Local estimation may be inaccurate due to dynamic bucket multipliers

2. **Account for storage gas** - A simple transfer costs 60,000 gas minimum (21K compute + 39K storage intrinsic). This is the minimum gas cost (intrinsic gas) for any transaction.

3. **Prefer transient storage or memory over persistent storage** - Allocating new storage slots incurs storage gas and counts toward state growth limits. Use transient storage (EIP-1153: `TSTORE`/`TLOAD`) for data that only needs to persist within a transaction, or memory for data within a single call. This avoids storage gas costs entirely and keeps your contract within state growth limits.

---

## 4. Resource Limits

### 4.1 Four-Dimensional Limits

MegaETH enforces four independent resource limits per transaction in addition to Ethereum's gas limit:

| Limit Type       | Transaction Limit  |
| ---------------- | ------------------ |
| **Compute Gas**  | 200,000,000 (200M) |
| **Data Size**    | 12.5 MB            |
| **KV Updates**   | 500,000            |
| **State Growth** | 1,000              |

### 4.2 What Counts Toward Each Limit

**Compute Gas:**

- All EVM instruction execution costs (i.e., the gas cost in the vanilla EVM), including precompile costs, memory expansion costs, etc.

**Data Size:**

Data size measures the total amount of data needs to be transmitted and stored for each transaction, including the transaction itself and its execution outcome:

- Transaction calldata
- Event logs (topics + data)
- Storage writes (40 bytes per write)
- Account updates (40 bytes each)
- Deployed contract code

**KV Updates:**

KV updates measure the total amount of data entry (account or storage slot) updated by the end of the transaction execution in the MegaETH's world state:

- Storage writes (SSTORE when value changes).
- Account state changes (balance, nonce, code).

**State Growth:**

State growth measures the amount of new data entry (account or storage slot) that are created by the end of the transaction execution. These new data contribute to the monotonically-increasing world state so they are restricted:

- New storage slots (0→non-zero writes)
- New accounts created
- New contracts deployed
- _Note: Clearing slots back to zero (or reverting account creation) within the same transaction reduces the count, not clearing a slot created in previous transactions do not._

### 4.3 What Happens When Exceeded

When a transaction exceeds any limit:

1. **Execution halts** immediately
2. **Remaining gas is preserved** and refunded to sender
3. **Transaction is included** in the block with failed status (status=0)
4. **No state changes** from the transaction are applied

### 4.4 Best Practices

**Be mindful of storage slot creation:**

```solidity
// Each new mapping entry = 1 state growth
// 1,000 limit means max ~1,000 new entries per tx
mapping(address => uint256) public balances;
```

---

## 5. Volatile Data Access

### 5.1 The Problem

MegaETH achieves high throughput through parallel transaction execution. Transactions that access frequently-changing data (like block timestamps or high-frequency updated oracles) create conflicts that limit parallelism. Such frequently-changing data is called `volatile data`.

To support efficient execution, MegaETH implements **compute gas restriction** - when you access volatile data, your remaining compute gas is capped.

### 5.2 Block Environment Opcodes

Accessing these opcodes caps remaining compute gas to **20,000,000**:

| Opcode        | Description               |
| ------------- | ------------------------- |
| `NUMBER`      | Current block number      |
| `TIMESTAMP`   | Current block timestamp   |
| `COINBASE`    | Block beneficiary address |
| `DIFFICULTY`  | Block difficulty          |
| `GASLIMIT`    | Block gas limit           |
| `BASEFEE`     | Base fee per gas          |
| `PREVRANDAO`  | Previous block randomness |
| `BLOCKHASH`   | Historical block hash     |
| `BLOBBASEFEE` | Blob base fee             |
| `BLOBHASH`    | Blob hash lookup          |

### 5.3 Beneficiary Account Access

Accessing the block beneficiary (coinbase) account also caps remaining compute gas to **20,000,000**:

| Trigger                                     | Description                             |
| ------------------------------------------- | --------------------------------------- |
| `BALANCE`, `SELFBALANCE`                    | Reading beneficiary's balance           |
| `EXTCODECOPY`, `EXTCODESIZE`, `EXTCODEHASH` | Accessing beneficiary's code            |
| Transaction sender is beneficiary           | When `msg.sender == block.coinbase`     |
| Transaction recipient is beneficiary        | When call target is `block.coinbase`    |
| `DELEGATECALL` to beneficiary               | Delegated context accessing beneficiary |

### 5.4 Oracle Access

Accessing the oracle contract caps remaining compute gas to **1,000,000**:

- Oracle contract address: `0x6342000000000000000000000000000000000001`
- Applies to CALL, STATICCALL, DELEGATECALL, CALLCODE

### 5.5 Most Restrictive Limit Applies

When multiple volatile data types are accessed in the same transaction, the **most restrictive limit applies**:

- Block env access (20M) + Oracle access (1M) = **1M cap**
- The order of access doesn't matter - the lowest cap is enforced globally

### 5.6 Patterns to Avoid

```solidity
// Bad: Heavy computation after reading timestamp
function processWithTimestamp() external {
    uint256 currentTime = block.timestamp; // Triggers 20M gas cap

    // This loop might run out of gas!
    for (uint i = 0; i < 10000; i++) {
        heavyComputation(i, currentTime);
    }
}
```

---

## 6. Contract Deployment and Destruction

### 6.1 Larger Contracts Supported

MegaETH supports contracts up to **512 KB** (vs 24 KB on Ethereum):

**Note:** Larger contracts cost more gas to deploy due to code deposit storage gas (10,000 gas/byte).

### 6.2 SELFDESTRUCT is Disabled

The `SELFDESTRUCT` opcode is completely disabled and will cause transaction failure (treated as an `INVALID` opcode).

### 6.3 Gas Forwarding (98/100 Rule)

MegaETH forwards **98/100** of remaining gas to subcalls (vs 63/64 on Ethereum).

---

## 7. System Contracts & Oracle Service

### 7.1 High-Precision Timestamp Oracle

**Address:** `0x6342000000000000000000000000000000000002`

**Interface:**

```solidity
interface HighPrecisionTimestampOracle {
    function timestamp() external view returns (uint256);
}
```

Use case: microsecond timestamp precision when `block.timestamp` isn't granular enough.

Note that the oracle data are served by the sequencer on demand, only when a transaction tries to read the oracle data, will the sequencer submits system transactions to update the oracle contract.

**Volatile Data**: The High-Precision Timestamp Oracle internally reads oracle data from the core oracle contract `0x6342000000000000000000000000000000000001`. Hence, obtaining high-precision timestamp is essentially a volatile data access and is subject to the "compute gas restriction" in Section 5.1.

**Trust Assumption**: The oracle service requires trusting the sequencer to publish accurate data. Consider this when building applications.

---

## 8. Precompiles

MegaETH inherits all precompiles from Optimism Isthmus, which includes Ethereum Prague precompiles, EIP-2537 BLS12-381 precompiles, and RIP-7212 P256VERIFY.

### 8.1 MegaETH-Specific Modifications

**KZG Point Evaluation (0x0A):**

- MegaETH: **100,000 gas**
- Ethereum: 50,000 gas
- Reason: 2x increase to reflect computational cost

**ModExp (0x05):**

- Uses EIP-7883 pricing

---

## 9. Reference

### 9.1 Useful Links

- [REX Specification](../hardfork-spec/Rex.md)
- [MiniRex Specification](../hardfork-spec/MiniRex.md)
- [Dual Gas Model Details](./DUAL_GAS_MODEL.md)
- [Block and Transaction Limits](./BLOCK_AND_TX_LIMITS.md)
- [Resource Accounting](./RESOURCE_ACCOUNTING.md)
- [Oracle Service](./ORACLE_SERVICE.md)
