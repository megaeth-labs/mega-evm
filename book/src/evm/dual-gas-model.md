# Dual Gas Model

## Principle

Operations that impose storage costs on nodes (state storage, history data) are charged additional **storage gas** on top of standard EVM **compute gas**.

The **overall gas cost** reported in transaction receipts is the sum of both:

```
total gas used = compute gas + storage gas
```

This separation enables independent pricing of computational work versus storage burden.

## Storage Gas Costs (Rex)

| Operation                  | Storage Gas Formula        | Notes                                                 |
| -------------------------- | -------------------------- | ----------------------------------------------------- |
| **Transaction Intrinsic**  | 39,000 (flat)              | All transactions pay this base storage gas            |
| **SSTORE (0 → non-0)**    | 20,000 × (multiplier - 1)  | Only for zero-to-non-zero transitions                 |
| **Account Creation**       | 25,000 × (multiplier - 1)  | Value transfer to empty account                       |
| **Contract Creation**      | 32,000 × (multiplier - 1)  | CREATE/CREATE2 opcodes or creation transactions       |
| **Code Deposit**           | 10,000/byte                | Per byte when contract creation succeeds              |
| **LOG Topic**              | 3,750/topic                | Per topic, regardless of revert                       |
| **LOG Data**               | 80/byte                    | Per byte, regardless of revert                        |
| **Calldata (zero)**        | 40/byte                    | Per zero byte in transaction input                    |
| **Calldata (non-zero)**    | 160/byte                   | Per non-zero byte in transaction input                |
| **Floor (zero)**           | 100/byte                   | EIP-7623 floor cost for zero bytes                    |
| **Floor (non-zero)**       | 400/byte                   | EIP-7623 floor cost for non-zero bytes                |

## Dynamic SALT Multiplier

Storage gas costs scale dynamically based on **SALT bucket capacity**.
Each account and storage slot maps to a SALT bucket in MegaETH's blockchain state.

**Formula**: `multiplier = bucket_capacity / MIN_BUCKET_SIZE`

- When `multiplier = 1` (minimum bucket size): **zero storage gas** — no penalty for fresh storage
- When `multiplier > 1`: linear scaling based on bucket capacity expansion

This mechanism prevents state bloat by making storage more expensive in crowded state regions.

## Transaction Intrinsic Costs

All transactions pay both compute gas and storage gas as intrinsic costs:

| Spec        | Compute Gas | Storage Gas | Total  |
| ----------- | ----------- | ----------- | ------ |
| **MiniRex** | 21,000      | 0           | 21,000 |
| **Rex+**    | 21,000      | 39,000      | 60,000 |

## Historical: MiniRex vs Rex

MiniRex was the first hardfork to introduce the dual gas model.
Rex significantly refined the formulas:

| Operation                 | MiniRex Formula          | Rex Formula               |
| ------------------------- | ------------------------ | ------------------------- |
| **Transaction Intrinsic** | 0                        | 39,000 (flat)             |
| **SSTORE (0→non-0)**     | 2,000,000 × multiplier   | 20,000 × (multiplier-1)  |
| **Account Creation**      | 2,000,000 × multiplier   | 25,000 × (multiplier-1)  |
| **Contract Creation**     | 2,000,000 × multiplier   | 32,000 × (multiplier-1)  |

The Rex formula `base × (multiplier - 1)` means fresh storage (multiplier=1) is free, unlike MiniRex's formula which always charged storage gas.
