---
description: MegaETH dual gas model specification — compute gas, storage gas, SALT bucket multiplier, and per-operation storage gas schedule.
spec: Rex4
---

# Dual Gas Model

MegaETH's dual gas model separates transaction gas costs into two independent dimensions: [compute gas](../glossary.md#compute-gas) (the gas charged for EVM computation, derived from standard EVM gas semantics) and [storage gas](../glossary.md#storage-gas) (an additional charge for operations that impose persistent storage burden on nodes).
A transaction's total gas is the sum of both.

## Motivation

Standard EVM gas pricing assumes a base fee high enough that storage-heavy operations (state writes, logs, calldata) are adequately priced by compute gas alone.
MegaETH breaks this assumption in two ways:

1. **Extremely low base fees** — MegaETH's base fee is 0.001 gwei (10⁶ wei), orders of magnitude lower than Ethereum mainnet. At this fee level, the compute gas cost of an SSTORE (22,100 gas) is negligible relative to the actual cost of persisting the state change.

2. **High transaction gas limits** — MegaETH allows up to 10 billion gas per block. A single transaction could write thousands of storage slots, deploy megabytes of bytecode, or emit massive logs for near-zero cost under standard gas pricing.

Without a separate storage gas dimension, a single transaction could bloat on-chain state or history data to unsustainable levels.
The dual gas model addresses this by pricing storage burden independently of computation, ensuring that state-heavy operations pay their true cost to node operators regardless of the base fee level.

## Specification

The named constants referenced in this section are defined later in [Constants](#constants).

### Total Gas

A node MUST compute total gas for every transaction as:

```
total_gas_used = compute_gas_used + storage_gas_used
```

Both compute gas and storage gas MUST be deducted from the transaction's `gas_limit` budget.
If the combined total exceeds `gas_limit`, the transaction MUST halt with `OutOfGas`.
The `gas_used` field in the transaction receipt MUST reflect the combined total.

### Compute Gas

[Compute gas](../glossary.md#compute-gas) is based on standard EVM gas semantics inherited from Optimism Isthmus / Ethereum Prague.
Unless explicitly overridden elsewhere in this specification, each opcode MUST use the same compute gas cost as in the inherited EVM semantics.
The dual gas model itself does not redefine opcode compute gas costs; it adds the storage gas dimension on top of them.

### Storage Gas

[Storage gas](../glossary.md#storage-gas) is an additional charge for operations that impose persistent storage burden on nodes.
A node MUST charge storage gas according to the following schedule:

| Operation                          | Storage Gas Formula                                       | Charging Trigger                                                                                                       |
| ---------------------------------- | --------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Transaction Intrinsic**          | `INTRINSIC_STORAGE_GAS` (39,000 flat)                     | Charged before execution begins, alongside compute intrinsic gas                                                       |
| **SSTORE (0 → non-0)**             | `SSTORE_STORAGE_GAS_BASE × (multiplier − 1)`              | Charged at the time of the SSTORE opcode when writing a non-zero value to a slot that was zero before this transaction |
| **Account Creation**               | `ACCOUNT_CREATION_STORAGE_GAS_BASE × (multiplier − 1)`    | Charged when a value transfer targets an empty account                                                                 |
| **Contract Creation**              | `CONTRACT_CREATION_STORAGE_GAS_BASE × (multiplier − 1)`   | Charged at CREATE/CREATE2 execution or creation transaction, regardless of whether initcode succeeds or fails          |
| **Code Deposit**                   | `CODE_DEPOSIT_STORAGE_GAS × code_length`                  | Charged per byte when contract creation succeeds and bytecode is stored                                                |
| **LOG Topic**                      | `LOG_TOPIC_STORAGE_GAS × topic_count`                     | Charged at the LOG opcode                                                                                              |
| **LOG Data**                       | `LOG_DATA_STORAGE_GAS × data_length`                      | Charged at the LOG opcode                                                                                              |
| **Calldata (zero byte)**           | `CALLDATA_ZERO_STORAGE_GAS × zero_byte_count`             | Charged before execution begins, alongside intrinsic gas                                                               |
| **Calldata (non-zero byte)**       | `CALLDATA_NONZERO_STORAGE_GAS × nonzero_byte_count`       | Charged before execution begins, alongside intrinsic gas                                                               |
| **Calldata floor (zero byte)**     | `CALLDATA_FLOOR_ZERO_STORAGE_GAS × zero_byte_count`       | Post-execution floor check (see below)                                                                                 |
| **Calldata floor (non-zero byte)** | `CALLDATA_FLOOR_NONZERO_STORAGE_GAS × nonzero_byte_count` | Post-execution floor check (see below)                                                                                 |

Contract creation MUST charge only `CONTRACT_CREATION_STORAGE_GAS_BASE × (multiplier − 1)`.
The account creation storage gas (`ACCOUNT_CREATION_STORAGE_GAS_BASE`) MUST NOT be charged on top of the contract creation cost.

#### Calldata Floor Cost

Per [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623), after execution completes, if the total gas consumed is less than the calldata floor cost, the transaction MUST be charged the floor cost instead.
The floor cost storage gas component uses the `CALLDATA_FLOOR_*_STORAGE_GAS` constants, which are `STORAGE_GAS_MULTIPLIER` (10×) of the standard EVM floor costs defined in EIP-7623.

#### SSTORE Storage Gas Refund

Setting a storage slot back to its original value within the same transaction MUST NOT refund the storage gas that was charged when the slot was first written to a non-zero value.
Storage gas for SSTORE is non-refundable: every zero-to-non-zero SSTORE transition accumulates storage gas even if the slot is later reset.

Standard EVM gas refunds for SSTORE (e.g., the `SSTORE_CLEARS_SCHEDULE` refund) apply only to the compute gas component and MUST NOT affect storage gas.

#### Revert Behavior

Storage gas charged within a reverted [call frame](../glossary.md#call-frame) MUST be consumed and not refunded, consistent with standard EVM gas semantics.
The [data size](resource-accounting.md) tracked for LOG operations within a reverted call frame MUST be rolled back, since the logs themselves are discarded.

### Storage Gas Stipend for Value Transfers

The 10× storage gas on LOG opcodes causes a simple `LOG1` to cost 4,500 gas (750 compute + 3,750 storage), exceeding the EVM's `CALL_STIPEND` of 2,300.

An additional **[storage gas stipend](../glossary.md#storage-gas-stipend)** of 23,000 gas is granted for internal (`depth > 0`) value-transferring `CALL` and `CALLCODE` opcodes.
`DELEGATECALL`, `STATICCALL`, top-level transaction calls, and [system contract](../system-contracts/overview.md) interceptions MUST NOT receive the stipend.
The callee's total gas becomes: `forwarded_gas + CALL_STIPEND (2,300) + STORAGE_CALL_STIPEND (23,000)`.
The callee's [compute gas](../glossary.md#compute-gas) limit MUST remain at the original level (`forwarded_gas + CALL_STIPEND`), so the extra gas can only be consumed by storage gas operations.
On return, unused storage gas stipend MUST be burned — it MUST NOT be returned to the caller, regardless of whether the callee succeeded or reverted.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.

### Dynamic SALT Multiplier

Storage gas costs for SSTORE, account creation, and contract creation scale dynamically based on [SALT bucket](../glossary.md#salt-bucket) capacity.

A node MUST compute the multiplier for each operation as:

```
multiplier = bucket_capacity / MIN_BUCKET_SIZE
```

Where `bucket_capacity` is the capacity of the SALT bucket that the target account or storage slot maps to in the parent block's state.

The following rules MUST apply:

- When `multiplier = 1` (bucket at minimum size): storage gas for the operation MUST be zero, since `base × (1 − 1) = 0`.
- When `multiplier > 1`: storage gas MUST scale linearly as `base × (multiplier − 1)`.
- The multiplier MUST be determined from the SALT bucket state of the **parent block**, not the current transaction's intermediate state.

#### Bucket ID Calculation

For storage gas calculation, a node MUST determine the SALT bucket from the target key, regardless of whether the account or storage slot already exists.

The bucket ID formula is:

```
bucket_id(key) = (ahash(key) mod NUM_KV_BUCKETS) + NUM_META_BUCKETS
```

Where `ahash` is the fixed-seed deterministic hash function defined by SALT.
The canonical bucket-mapping implementation is in the [SALT repository](https://github.com/megaeth-labs/salt/blob/main/salt/src/state/hasher.rs).

- For account creation, the bucket ID MUST be computed from the target account address.
- For an `SSTORE`-triggered new storage write, the bucket ID MUST be computed from the concatenation of the contract address and storage slot key.

Equivalently:

```
account_bucket_id = bucket_id(address_bytes)
slot_bucket_id = bucket_id(address_bytes || slot_key_bytes)
```

The multiplier MUST be computed from the capacity of that bucket in the parent-block state, before the new account or storage slot is created.

#### Bucket Capacity Determination

Bucket capacity is determined by SALT bucket metadata.
Each data bucket has a minimum capacity of `MIN_BUCKET_SIZE` (256 slots).

SALT determines capacity using the following rule:

1. A bucket starts at `MIN_BUCKET_SIZE`.
2. Let `used` be the number of occupied slots in the bucket.
3. If `used > capacity × 80%`, the bucket MUST expand.
4. Expansion doubles the bucket capacity.
5. Expansion repeats until `used / capacity ≤ 80%`.

Equivalently:

```
new_capacity = capacity
while used * 100 > new_capacity * 80:
    new_capacity = new_capacity * 2
```

The resulting capacity is a power-of-two multiple of `MIN_BUCKET_SIZE` under normal operation.
The maximum bucket capacity is `2^40` slots.

Buckets larger than `MIN_BUCKET_SIZE` are represented as multiple 256-slot segments under a local SALT bucket subtree.
As capacity grows, the bucket subtree root moves upward to cover the larger slot range.

For storage gas calculation, a node MUST use the capacity recorded for the target bucket in the parent-block state.
It MUST NOT derive capacity from the transaction's intermediate writes.

The canonical design references are:

- [SALT README — Dynamic Bucket Sizing and Bucket Management](https://github.com/megaeth-labs/salt/blob/main/salt/README.md)
- [`salt/src/constant.rs`](https://github.com/megaeth-labs/salt/blob/main/salt/src/constant.rs) for `MIN_BUCKET_SIZE`, resize threshold, and resize multiplier

### Transaction Intrinsic Costs

All transactions MUST pay both compute gas and storage gas as intrinsic costs before execution begins:

| Component   | Cost                             |
| ----------- | -------------------------------- |
| Compute gas | `INTRINSIC_COMPUTE_GAS` (21,000) |
| Storage gas | `INTRINSIC_STORAGE_GAS` (39,000) |
| **Total**   | **60,000**                       |

These costs are in addition to standard calldata gas (both compute and storage components).
A transaction with `gas_limit < 60,000 + calldata_gas` MUST be rejected as invalid.

## Constants

| Constant                                            | Value      | Description                                                 |
| --------------------------------------------------- | ---------- | ----------------------------------------------------------- |
| `INTRINSIC_COMPUTE_GAS`                             | 21,000     | Standard EVM intrinsic gas for all transactions             |
| `INTRINSIC_STORAGE_GAS`                             | 39,000     | Storage gas intrinsic for all transactions                  |
| `SSTORE_STORAGE_GAS_BASE`                           | 20,000     | Base storage gas for SSTORE (0 → non-0)                     |
| `ACCOUNT_CREATION_STORAGE_GAS_BASE`                 | 25,000     | Base storage gas for account creation                       |
| `CONTRACT_CREATION_STORAGE_GAS_BASE`                | 32,000     | Base storage gas for contract creation                      |
| `CODE_DEPOSIT_STORAGE_GAS`                          | 10,000     | Storage gas per byte of deployed bytecode                   |
| `LOG_TOPIC_STORAGE_GAS`                             | 3,750      | Storage gas per LOG topic                                   |
| `LOG_DATA_STORAGE_GAS`                              | 80         | Storage gas per byte of LOG data                            |
| `CALLDATA_ZERO_STORAGE_GAS`                         | 40         | Storage gas per zero byte of calldata                       |
| `CALLDATA_NONZERO_STORAGE_GAS`                      | 160        | Storage gas per non-zero byte of calldata                   |
| `CALLDATA_FLOOR_ZERO_STORAGE_GAS`                   | 100        | Storage gas floor per zero byte of calldata                 |
| `CALLDATA_FLOOR_NONZERO_STORAGE_GAS`                | 400        | Storage gas floor per non-zero byte of calldata             |
| `STORAGE_GAS_MULTIPLIER`                            | 10         | Ratio of calldata/LOG storage gas to standard EVM costs     |
| [`MIN_BUCKET_SIZE`](../glossary.md#min_bucket_size) | 256        | Smallest [SALT bucket](../glossary.md#salt-bucket) capacity |
| `NUM_META_BUCKETS`                                  | 65,536     | Number of SALT buckets reserved for metadata                |
| `NUM_KV_BUCKETS`                                    | 16,711,680 | Number of SALT buckets available for key-value state        |

## Rationale

**Why `base × (multiplier − 1)` instead of `base × multiplier`?**
The MiniRex spec originally used `base × multiplier`, which charged storage gas even in uncrowded state regions (multiplier = 1).
The Rex spec changed to `base × (multiplier − 1)` so that operations in minimum-sized SALT buckets incur zero storage gas, removing the penalty for writing to fresh state.
See the [MiniRex](../upgrades/minirex.md) and [Rex](../upgrades/rex.md) upgrade pages for the historical evolution.

**Why no storage gas refund for SSTORE resets?**
Allowing refunds would enable a pattern where contracts repeatedly write and clear the same slot to generate refund credits, undermining the storage gas pricing model.
The non-refundable design ensures that every state-expanding operation pays its full cost regardless of subsequent reversals within the same transaction.

**Why 10× multiplier for calldata and LOG?**
The `STORAGE_GAS_MULTIPLIER` of 10 was chosen to reflect the long-term storage and data availability costs that calldata and log operations impose on nodes, relative to their standard EVM gas costs which were designed for Ethereum's higher base fee regime.

**Why a flat intrinsic storage gas?**
Every transaction imposes a baseline storage cost on nodes regardless of its execution: the transaction itself must be stored, the receipt must be persisted, and account state (nonce, balance) must be updated.
The 39,000 flat intrinsic storage gas covers this per-transaction overhead.

## Security Considerations

**If storage gas is omitted or undercharged**, MegaETH's 0.001 gwei base fee makes every SSTORE, contract deployment, and LOG call negligible in cost.
A single transaction could write thousands of storage slots or emit megabytes of log data for nearly zero fee — the exact attack the dual gas model exists to prevent.

**If unused `STORAGE_CALL_STIPEND` is returned to the caller rather than burned**, system-granted gas leaks back to the caller, who can redirect it to non-storage operations — undermining the compute-gas cap designed to keep the stipend storage-only.

## Spec History

For the historical evolution of storage gas formulas and constants across specs:

- [MiniRex](../upgrades/minirex.md) — original `base × multiplier` formula with 2,000,000 base cost
- [Rex](../upgrades/rex.md) — revised to `base × (multiplier − 1)` with current base costs, added transaction intrinsic storage gas
- [Rex4](../upgrades/rex4.md) — storage gas stipend for value transfers
