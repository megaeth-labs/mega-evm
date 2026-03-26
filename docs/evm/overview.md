# Overview

This page describes the current stable MegaEVM behavior (through [Rex3](../hardfork-spec.md#rex3)) as a single reference.
For incremental changes introduced by each upgrade, see the [Network Upgrades](../upgrades/overview.md) section.
For deep dives on individual topics, see the linked concept pages below.

<details>

<summary>Rex4 (unstable)</summary>

Rex4 features are shown in expandable sections throughout this page but are not yet active on the network.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for the full unstable spec.

</details>

## Base Layer

MegaEVM builds on Optimism Isthmus (Ethereum Prague).
All standard EVM semantics are inherited unless explicitly overridden.

## Dual Gas Model

Every transaction's total gas cost is the sum of two independent components:

```
total gas used = compute gas + storage gas
```

- **[Compute gas](../glossary.md#compute-gas)** — The gas you already know from Ethereum.
- **[Storage gas](../glossary.md#storage-gas)** — An additional charge for operations that impose persistent storage burden on nodes.

### Compute Gas Is Standard EVM Gas

Compute gas in MegaETH is **identical** to gas on Ethereum (specifically, Optimism Isthmus / Ethereum Prague).
Every opcode costs exactly the same compute gas as it would on mainnet Ethereum: an `ADD` costs 3 gas, a cold `SLOAD` costs 2,100 gas, a `CALL` to a warm address costs 100 gas, and so on.
If you have existing gas intuition from Ethereum development, it applies directly to compute gas on MegaETH.

Storage gas is the only new dimension.
It is charged on top of compute gas for operations that grow on-chain state — see the [Storage Gas Schedule](#storage-gas-schedule) below.

### How Gas Limit, Compute Gas, and Storage Gas Relate

Your transaction's `gas_limit` field works the same way as on Ethereum — it sets the maximum total gas you are willing to spend.
Both compute gas and storage gas are deducted from this single `gas_limit` budget.
If total gas consumed (compute + storage) exceeds `gas_limit`, the transaction runs out of gas just like on Ethereum.

On top of the standard `gas_limit`, MegaETH enforces an additional **[compute gas limit](resource-limits.md)** (currently 200M) that caps just the compute portion.
A transaction can be halted by either ceiling: running out of total gas (`gas_limit`) or exceeding the compute gas limit — whichever is hit first.

{% hint style="success" %}
**In practice:**
- **`gas_limit`** — Set this to cover your expected total gas (compute + storage). The `eth_estimateGas` endpoint returns a value that accounts for both.
- **`gas_used` in receipts** — Reports total gas consumed (compute + storage), just like Ethereum.
- **Compute gas limit** — An invisible additional ceiling. You don't set it; the protocol enforces it. Most transactions stay well under 200M compute gas.
{% endhint %}

### Transaction Intrinsic Costs

| Component   | Cost   |
| ----------- | ------ |
| Compute gas | 21,000 |
| Storage gas | 39,000 |
| **Total**   | 60,000 |

### Storage Gas Schedule

| Operation                  | Storage Gas Formula         | Notes                                             |
| -------------------------- | --------------------------- | ------------------------------------------------- |
| **Transaction intrinsic**  | 39,000 (flat)               | All transactions pay this base storage gas         |
| **SSTORE (0 → non-0)**    | 20,000 × (multiplier − 1)  | Writing a non-zero value to a slot that was zero before this transaction |
| **Account creation**       | 25,000 × (multiplier − 1)  | Value transfer to empty account                    |
| **Contract creation**      | 32,000 × (multiplier − 1)  | CREATE/CREATE2 opcodes or creation transactions (charged regardless of whether initcode succeeds or fails) |
| **Code deposit**           | 10,000/byte                 | Per byte when contract creation succeeds           |
| **LOG topic**              | 3,750/topic                 | Per topic                                          |
| **LOG data**               | 80/byte                     | Per byte of log data                               |
| **Calldata (zero)**        | 40/byte                     | 10 × standard EVM zero-byte cost (4)               |
| **Calldata (non-zero)**    | 160/byte                    | 10 × standard EVM non-zero-byte cost (16)          |
| **Calldata floor (zero)**  | 100/byte                    | 10 × standard EVM floor cost for zero bytes (10)   |
| **Calldata floor (non-zero)** | 400/byte                 | 10 × standard EVM floor cost for non-zero bytes (40) |

**Calldata floor cost**: [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623) introduced a minimum ("floor") charge for calldata.
After execution, if the total gas consumed is less than the calldata floor cost, the transaction is charged the floor cost instead.
This prevents data-heavy transactions from underpaying for their calldata by performing minimal computation.
MegaETH applies the same 10× storage gas multiplier to the floor cost as it does to the standard calldata cost.

**Revert behavior for LOG**: LOG storage gas follows standard EVM gas semantics — gas spent in a reverted call frame is consumed and not refunded, just like compute gas.
However, the [data size](resource-accounting.md) tracked for the same LOG is rolled back on revert, since the log itself is discarded.

{% hint style="danger" %}
**No storage gas refund for SSTORE resets**: Setting a storage slot back to its original value within the same transaction does **not** refund the storage gas.
Use [transient storage](https://eips.ethereum.org/EIPS/eip-1153) (`TSTORE`/`TLOAD`) for scratch data that does not need to persist.
{% endhint %}

### Dynamic SALT Multiplier

Storage gas costs for SSTORE, account creation, and contract creation scale dynamically based on [SALT bucket](../glossary.md#salt-bucket) capacity.

**Formula**: `multiplier = bucket_capacity /` [`MIN_BUCKET_SIZE`](../glossary.md#min_bucket_size)

- At `multiplier = 1` (minimum bucket size): **zero storage gas** — fresh storage is free.
- At `multiplier > 1`: linear scaling makes crowded state regions more expensive.

{% hint style="success" %}
**Gas estimation**: Use `eth_estimateGas` on a MegaETH RPC endpoint for accurate gas estimates.
The endpoint accounts for SALT multipliers, storage gas, and all resource dimensions.
Do not attempt to compute gas costs manually — the dynamic multiplier depends on on-chain SALT bucket state.
{% endhint %}

See [Dual Gas Model](dual-gas-model.md) for details.

## Multidimensional Resource Limits

In addition to the standard gas limit, MegaETH enforces four independent resource ceilings during execution, plus three sequencer-configured pre-execution limits (gas limit cap, transaction size, DA size) that fast-reject oversized transactions before execution begins.
See [Resource Limits](resource-limits.md) for the full list.

The four runtime limits are:

| Resource         | Transaction Limit        | Block Limit                  |
| ---------------- | ------------------------ | ---------------------------- |
| Compute gas      | 200,000,000 (200M)      | No separate limit (see note) |
| Data size        | 13,107,200 (12.5 MB)    | 13,107,200                   |
| KV updates       | 500,000                  | 500,000                      |
| State growth     | 1,000                    | 1,000                        |

Compute gas has no dedicated block limit because it is already constrained by the block's standard gas limit (`block.gasLimit` from the block header), which caps the sum of all transactions' total gas (compute + storage) in a block.

When any transaction-level limit is exceeded during execution, the transaction halts with `OutOfGas` (status=0).
Gas consumed up to the halt point is charged; remaining gas is refunded to the sender.
The transaction receipt reflects `gas_used` equal to the gas actually consumed, not `gas_limit`.

Block-level limits are enforced across transactions: the last transaction that causes cumulative usage to exceed a block limit is allowed to complete and be included, but subsequent transactions are rejected before execution.

<details>

<summary>Rex4 (unstable): Per-Call-Frame Resource Budgets</summary>

Rex4 adds per-call-frame enforcement on top of transaction-level limits.
Each inner [call frame](../glossary.md#call-frame) receives `remaining × 98/100` of its parent's remaining budget for each dimension.
A [call-frame-local exceed](../glossary.md#call-frame-local-exceed) **reverts** that call frame with `MegaLimitExceeded(uint8 kind, uint64 limit)` — it does not halt the transaction.
Compute gas consumed by reverted child frames still counts toward the transaction total.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.

</details>

See [Resource Limits](resource-limits.md) and [Resource Accounting](resource-accounting.md) for details.

## Gas Forwarding (98/100 Rule)

All CALL-like opcodes (CALL, STATICCALL, DELEGATECALL, CALLCODE) and CREATE/CREATE2 forward at most **98/100** of remaining gas to subcalls, replacing the standard EVM's 63/64 rule (since [Rex](../upgrades/rex.md); [MiniRex](../upgrades/minirex.md) applied the 98/100 rule only to CALL and CREATE/CREATE2).
This prevents call-depth attacks under MegaETH's high gas limits.

{% hint style="danger" %}
**Migration note**: Contracts that compute gas forwarding amounts assuming the standard 63/64 rule ([EIP-150](https://eips.ethereum.org/EIPS/eip-150)) will see different behavior.
The parent call frame retains 2% instead of ~1.6%, so subcalls receive slightly less gas.
Review any patterns that rely on precise gas forwarding calculations.
{% endhint %}

## Gas Detention

Accessing **[volatile data](../glossary.md#volatile-data)** — block environment fields, the [beneficiary's](../glossary.md#beneficiary) account, or oracle storage — triggers a compute gas cap that forces the transaction to terminate quickly.
This reduces parallel execution conflicts without banning the access outright.
Detained gas is refunded at transaction end.

### Volatile Data Categories

| Category                    | Trigger                                               | Cap   |
| --------------------------- | ----------------------------------------------------- | ----- |
| Block env / Beneficiary     | Block environment opcodes (NUMBER, TIMESTAMP, etc.) or any access to the [beneficiary](../glossary.md#beneficiary) account (BALANCE, code inspection, tx sender/recipient, DELEGATECALL) | 20M  |
| Oracle                      | SLOAD from [oracle contract](../system-contracts/oracle.md) storage (DELEGATECALL to oracle does not trigger — see [Gas Detention](gas-detention.md)) | 20M  |

{% hint style="success" %}
The 20M cap is in [compute gas](../glossary.md#compute-gas), which is identical to standard Ethereum gas.
For reference, a typical Uniswap V3 swap costs ~150K gas and a complex multi-hop aggregation ~500K gas on Ethereum mainnet.
20M compute gas is ample headroom for most contract interactions after a volatile data read.
{% endhint %}

### Absolute Cap

The [detained limit](../glossary.md#detained-limit) is an absolute cap on total compute gas for the transaction.
If the transaction has already consumed more gas than the cap when the volatile access occurs, execution halts immediately with `VolatileDataAccessOutOfGas`.
Across multiple volatile accesses, the most restrictive cap applies.

<details>

<summary>Rex4 (unstable): Relative Cap</summary>

Rex4 changes detention from an absolute cap to a relative cap.
The effective detained limit becomes `current_usage + cap` at the time of access.
A transaction that has consumed 25M compute gas before reading TIMESTAMP gets an effective limit of 25M + 20M = 45M — it can still perform 20M more gas after the access.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.

</details>

### Oracle Forced-Cold SLOAD

All SLOAD operations on the [oracle contract](../system-contracts/oracle.md) use cold access gas cost (2100 gas) regardless of [EIP-2929](https://eips.ethereum.org/EIPS/eip-2929) warm/cold tracking state.

{% hint style="info" %}
**Why forced cold?** During live execution, oracle data may come from the [oracle service's external environment](../oracle-services/README.md) rather than on-chain storage.
Replayers cannot determine which source was used, and external environment reads are inherently cold.
Forcing all oracle reads to cold access guarantees identical gas costs in both live execution and replay.
{% endhint %}

See [Gas Detention](gas-detention.md) for details.

## SELFDESTRUCT ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780))

`SELFDESTRUCT` is enabled with post-Cancun ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780)) semantics:

- If the contract was created in the same transaction, `SELFDESTRUCT` removes code and storage and transfers the balance to the target address.
- If the contract was not created in the same transaction, `SELFDESTRUCT` only transfers the balance — code and storage are preserved.

## Contract Size Limits

| Limit          | Value                    |
| -------------- | ------------------------ |
| Max contract   | 524,288 bytes (512 KB)   |
| Max initcode   | 548,864 bytes (512 KB + 24 KB) |

The contract size limit is 21× larger than the standard Ethereum limit (24 KB); the initcode limit is ~11× larger than the standard Ethereum limit (48 KB).

## System Contracts

MegaETH pre-deploys system contracts at well-known addresses:

| Contract                 | Address                                         | Since   | Purpose                                             |
| ------------------------ | ----------------------------------------------- | ------- | --------------------------------------------------- |
| [Oracle](../system-contracts/oracle.md) | `0x6342000000000000000000000000000000000001` | [MiniRex](../hardfork-spec.md#mini_rex) | Off-chain data key-value storage |
| [High-Precision Timestamp](../oracle-services/timestamp.md) | `0x6342000000000000000000000000000000000002` | [MiniRex](../hardfork-spec.md#mini_rex) | Sub-second timestamps ([oracle service](../oracle-services/overview.md)) |
| [KeylessDeploy](../system-contracts/keyless-deploy.md) | `0x6342000000000000000000000000000000000003` | [Rex2](../hardfork-spec.md#rex2) | Deterministic cross-chain deployment (Nick's Method) |

<details>

<summary>Rex4 (unstable): New System Contracts</summary>

| Contract          | Address              | Purpose                                |
| ----------------- | -------------------- | -------------------------------------- |
| MegaAccessControl | `0x6342000000000000000000000000000000000004` | Proactive volatile data access control |
| MegaLimitControl  | `0x6342000000000000000000000000000000000005` | Query remaining compute gas budget     |

**MegaAccessControl** — Disable volatile data access for your call frame and all descendant calls via `disableVolatileDataAccess()`.
While disabled, any volatile access reverts with `VolatileDataAccessDisabled(uint8 accessType)` — no gas detention is triggered.
Call `enableVolatileDataAccess()` to lift the restriction; reverts with `DisabledByParent()` if an ancestor call frame holds the disable.
Call `isVolatileDataAccessDisabled()` to query the current state.
The restriction automatically ends when the disabling call frame returns.

**MegaLimitControl** — Query effective remaining compute gas via `remainingComputeGas()`.
Returns `min(frame_remaining, tx_detained_remaining)` at call time.

See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.

</details>

See [System Contracts Overview](../system-contracts/overview.md) for the full registry and details.

## Precompile Gas Overrides

| Precompile             | Address | Cost Override                          |
| ---------------------- | ------- | -------------------------------------- |
| KZG Point Evaluation   | `0x0A`  | 100,000 gas (2× standard Prague cost)  |
| ModExp                 | `0x05`  | [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) gas schedule (raises the cost floor, making large-exponent calls more expensive than pre-7883 pricing) |

## Concept Deep Dives

| Topic | Description |
| ----- | ----------- |
| [Hardforks and Specs](../hardfork-spec.md) | How MegaETH versions MegaEVM behavior through hardforks and a linear spec progression |
| [Dual Gas Model](dual-gas-model.md) | Compute gas vs storage gas formulas and SALT mechanics |
| [Resource Limits](resource-limits.md) | Transaction and block limit values, two-phase checking, enforcement |
| [Resource Accounting](resource-accounting.md) | How each resource dimension is tracked during execution |
| [Gas Detention](gas-detention.md) | Volatile data detection, cap mechanics, evolution across specs |
