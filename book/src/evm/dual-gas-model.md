# Dual Gas Model

## Principle

Operations that impose storage costs on nodes (state storage, history data) are charged additional **[storage gas](../glossary.md#storage-gas)** on top of standard EVM **[compute gas](../glossary.md#compute-gas)**.

The **overall gas cost** reported in transaction receipts is the sum of both:

```
total gas used = compute gas + storage gas
```

This separation enables independent pricing of computational work versus storage burden.

## Storage Gas Costs

| Operation                  | Storage Gas Formula        | Notes                                                 |
| -------------------------- | -------------------------- | ----------------------------------------------------- |
| **Transaction Intrinsic**  | 39,000 (flat)              | All transactions pay this base storage gas            |
| **SSTORE (0 → non-0)**    | 20,000 × (multiplier - 1)  | When `original == 0 AND present == 0 AND new != 0`   |
| **Account Creation**       | 25,000 × (multiplier - 1)  | Value transfer to empty account                       |
| **Contract Creation**      | 32,000 × (multiplier - 1)  | CREATE/CREATE2 opcodes or creation transactions       |
| **Code Deposit**           | 10,000/byte                | Per byte when contract creation succeeds              |
| **LOG Topic**              | 3,750/topic                | Storage gas is permanent regardless of revert         |
| **LOG Data**               | 80/byte                    | Storage gas is permanent regardless of revert         |
| **Calldata (zero)**        | 40/byte                    | 10 × standard EVM zero-byte cost (4)                  |
| **Calldata (non-zero)**    | 160/byte                   | 10 × standard EVM non-zero-byte cost (16)             |
| **Floor (zero)**           | 100/byte                   | 10 × EIP-7623 floor cost for zero bytes (10)          |
| **Floor (non-zero)**       | 400/byte                   | 10 × EIP-7623 floor cost for non-zero bytes (40)      |

## Dynamic SALT Multiplier

Storage gas costs scale dynamically based on **[SALT bucket](../glossary.md#salt-bucket) capacity**.
Each account and storage slot maps to a SALT bucket in MegaETH's blockchain state.
A SALT bucket measures how "crowded" a state region is — the more state entries in a bucket, the larger its capacity grows.

**Formula**: `multiplier = bucket_capacity /` [`MIN_BUCKET_SIZE`](../glossary.md#min_bucket_size)

- When `multiplier = 1` (minimum bucket size): **zero storage gas** — no penalty for fresh storage
- When `multiplier > 1`: linear scaling based on bucket capacity expansion

This mechanism prevents state bloat by making storage more expensive in crowded state regions.

The SALT bucket capacity depends on on-chain state and cannot be predicted from contract code alone.
Use `eth_estimateGas` on a MegaETH RPC endpoint for accurate gas estimates — the endpoint accounts for SALT multipliers and all resource dimensions.

## Transaction Intrinsic Costs

All transactions pay both compute gas and storage gas as intrinsic costs:

| Component   | Cost   |
| ----------- | ------ |
| Compute gas | 21,000 |
| Storage gas | 39,000 |
| **Total**   | 60,000 |

For the historical evolution of storage gas costs (including MiniRex's different formula), see the [MiniRex](../upgrades/minirex.md) and [Rex](../upgrades/rex.md) upgrade pages.
