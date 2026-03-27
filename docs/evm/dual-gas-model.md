# Dual Gas Model

## Principle

Every transaction's total gas cost is the sum of two components:

```
total gas used = compute gas + storage gas
```

- **[Compute gas](../glossary.md#compute-gas)** is standard EVM gas — identical to Ethereum (Optimism Isthmus / Ethereum Prague). Every opcode costs exactly what it costs on mainnet Ethereum. If you have existing gas intuition from Ethereum development, it applies directly.
- **[Storage gas](../glossary.md#storage-gas)** is an additional charge for operations that impose persistent storage burden on nodes (state writes, logs, calldata, code deployment).

Storage gas is the only new dimension MegaETH introduces.
This separation enables independent pricing of computational work versus storage burden.

{% hint style="info" %}
**Why a dual gas model?** MegaETH features extremely low base fees and high transaction gas limits.
Under standard EVM gas pricing, operations that impose storage costs on nodes (state writes, logs, calldata) become dramatically underpriced, leading to unsustainable state bloat and history data growth.
Separating gas into compute and storage dimensions allows these costs to be priced independently.
{% endhint %}

{% hint style="success" %}
Both compute gas and storage gas are deducted from the same `gas_limit` budget that you set on your transaction — just like standard Ethereum gas accounting.
Your `gas_limit` must be large enough to cover both components, and `gas_used` in the receipt reflects the combined total.
{% endhint %}

## Storage Gas Costs

| Operation                  | Storage Gas Formula        | Notes                                                 |
| -------------------------- | -------------------------- | ----------------------------------------------------- |
| **Transaction Intrinsic**  | 39,000 (flat)              | All transactions pay this base storage gas            |
| **SSTORE (0 → non-0)**    | 20,000 × (multiplier - 1)  | Writing a non-zero value to a slot that was zero before this transaction |
| **Account Creation**       | 25,000 × (multiplier - 1)  | Value transfer to empty account                       |
| **Contract Creation**      | 32,000 × (multiplier - 1)  | CREATE/CREATE2 opcodes or creation transactions (charged regardless of whether initcode succeeds or fails) |
| **Code Deposit**           | 10,000/byte                | Per byte when contract creation succeeds              |
| **LOG Topic**              | 3,750/topic                | Per topic                                             |
| **LOG Data**               | 80/byte                    | Per byte of log data                                  |
| **Calldata (zero)**        | 40/byte                    | 10 × standard EVM zero-byte cost (4)                  |
| **Calldata (non-zero)**    | 160/byte                   | 10 × standard EVM non-zero-byte cost (16)             |
| **Calldata floor (zero)**  | 100/byte                   | 10 × standard EVM floor cost for zero bytes (10)      |
| **Calldata floor (non-zero)** | 400/byte                | 10 × standard EVM floor cost for non-zero bytes (40)  |

**Calldata floor cost**: [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623) introduced a minimum ("floor") charge for calldata.
After execution, if the total gas consumed is less than the calldata floor cost, the transaction is charged the floor cost instead.
This prevents data-heavy transactions from underpaying for their calldata by performing minimal computation.
MegaETH applies the same 10× storage gas multiplier to the floor cost as it does to the standard calldata cost.

**Revert behavior for LOG**: LOG storage gas follows standard EVM gas semantics — gas spent in a reverted call frame is consumed and not refunded, just like compute gas.
However, the [data size](resource-accounting.md) tracked for the same LOG is rolled back on revert, since the log itself is discarded.

<details>
<summary>Rex4 (unstable): Storage gas stipend for value transfers</summary>

The 10× storage gas on LOG opcodes causes even a simple `LOG1` to cost 4,500 gas, exceeding the EVM's `CALL_STIPEND` of 2,300.
This breaks `transfer()` / `send()` to contracts that emit events in `receive()`.

Rex4 introduces an additional **storage gas stipend** (23,000 gas) for value-transferring `CALL` and `CALLCODE` opcodes.
The callee's [compute gas](../glossary.md#compute-gas) limit remains unchanged, so the extra gas can only be consumed by storage gas operations.
Unused storage gas stipend is burned on return.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.

</details>

{% hint style="danger" %}
**No storage gas refund for SSTORE resets**: Setting a storage slot back to its original value within the same transaction does **not** refund the storage gas that was charged when the slot was first written.
A contract that repeatedly writes and resets the same slot will accumulate storage gas on every zero-to-non-zero transition.
Use [transient storage](https://eips.ethereum.org/EIPS/eip-1153) (`TSTORE`/`TLOAD`) for scratch data that does not need to persist across transactions.
{% endhint %}

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
