# Overview

This page describes the current stable MegaEVM behavior (through [Rex3](spec-system.md#rex3)) as a single reference.
For incremental changes introduced by each upgrade, see the [Network Upgrades](../upgrades/README.md) section.
For deep dives on individual topics, see the linked concept pages below.

{% hint style="info" %}
Rex4 features are shown in highlighted boxes throughout this page but are not yet active on the network.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for the full unstable spec.
{% endhint %}

## Base Layer

MegaEVM builds on Optimism Isthmus (Ethereum Prague).
All standard EVM semantics are inherited unless explicitly overridden.

## Dual Gas Model

Every transaction's total gas cost is the sum of two independent components:

```
total gas used = compute gas + storage gas
```

- **[Compute gas](../glossary.md#compute-gas)** — Standard Optimism EVM (Isthmus) gas costs for all opcodes and operations.
- **[Storage gas](../glossary.md#storage-gas)** — Additional costs for operations that impose persistent storage burden on nodes.

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
| **SSTORE (0 → non-0)**    | 20,000 × (multiplier − 1)  | When `original == 0 AND present == 0 AND new != 0` |
| **Account creation**       | 25,000 × (multiplier − 1)  | Value transfer to empty account                    |
| **Contract creation**      | 32,000 × (multiplier − 1)  | CREATE/CREATE2 opcodes or creation transactions    |
| **Code deposit**           | 10,000/byte                 | Per byte when contract creation succeeds           |
| **LOG topic**              | 3,750/topic                 | Storage gas is permanent regardless of revert; data size usage is rolled back on revert |
| **LOG data**               | 80/byte                     | Storage gas is permanent regardless of revert; data size usage is rolled back on revert |
| **Calldata (zero)**        | 40/byte                     | 10 × standard EVM zero-byte cost (4)               |
| **Calldata (non-zero)**    | 160/byte                    | 10 × standard EVM non-zero-byte cost (16)          |
| **Floor (zero)**           | 100/byte                    | 10 × EIP-7623 floor cost for zero bytes (10)       |
| **Floor (non-zero)**       | 400/byte                    | 10 × EIP-7623 floor cost for non-zero bytes (40)   |

### Dynamic SALT Multiplier

Storage gas costs for SSTORE, account creation, and contract creation scale dynamically based on [SALT bucket](../glossary.md#salt-bucket) capacity.

**Formula**: `multiplier = bucket_capacity /` [`MIN_BUCKET_SIZE`](../glossary.md#min_bucket_size)

- At `multiplier = 1` (minimum bucket size): **zero storage gas** — fresh storage is free.
- At `multiplier > 1`: linear scaling makes crowded state regions more expensive.

{% hint style="info" %}
**Gas estimation**: Use `eth_estimateGas` on a MegaETH RPC endpoint for accurate gas estimates.
The endpoint accounts for SALT multipliers, storage gas, and all resource dimensions.
Do not attempt to compute gas costs manually — the dynamic multiplier depends on on-chain SALT bucket state.
{% endhint %}

See [Dual Gas Model](dual-gas-model.md) for details.

## Multidimensional Resource Limits

MegaETH enforces four independent post-execution resource limits beyond standard gas:

| Resource         | Transaction Limit        | Block Limit     |
| ---------------- | ------------------------ | --------------- |
| Compute gas      | 200,000,000 (200M)      | Unlimited       |
| Data size        | 13,107,200 (12.5 MB)    | 13,107,200      |
| KV updates       | 500,000                  | 500,000         |
| State growth     | 1,000                    | 1,000           |

When any transaction-level limit is exceeded during execution, the transaction halts with `OutOfGas` (status=0).
Gas consumed up to the halt point is charged; remaining gas is refunded to the sender.
The transaction receipt reflects `gas_used` equal to the gas actually consumed, not `gas_limit`.

Block-level limits are enforced across transactions: the last transaction that causes cumulative usage to exceed a block limit is allowed to complete and be included, but subsequent transactions are rejected before execution.

{% hint style="info" %}
**Rex4 (unstable): Per-Call-Frame Resource Budgets** — Rex4 adds per-call-frame enforcement on top of transaction-level limits.
Each inner call frame receives `remaining × 98/100` of its parent's remaining budget for each dimension.
A call-frame-local exceed **reverts** that call frame with `MegaLimitExceeded(uint8 kind, uint64 limit)` — it does not halt the transaction.
Compute gas consumed by reverted child frames still counts toward the transaction total.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.
{% endhint %}

See [Resource Limits](resource-limits.md) and [Resource Accounting](resource-accounting.md) for details.

## Gas Forwarding (98/100 Rule)

All CALL-like opcodes (CALL, STATICCALL, DELEGATECALL, CALLCODE) and CREATE/CREATE2 forward at most **98/100** of remaining gas to subcalls, replacing the standard EVM's 63/64 rule.
This prevents call-depth attacks under MegaETH's high gas limits.

{% hint style="warning" %}
**Migration note**: Contracts that compute gas forwarding amounts assuming the standard 63/64 rule (EIP-150) will see different behavior.
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
| Block env / Beneficiary     | NUMBER, TIMESTAMP, COINBASE, etc. or beneficiary access | 20M  |
| Oracle                      | SLOAD from [oracle contract](../system-contracts/oracle.md) storage | 20M  |

### Absolute Cap

The [detained limit](../glossary.md#detained-limit) is an absolute cap on total compute gas for the transaction.
If the transaction has already consumed more gas than the cap when the volatile access occurs, execution halts immediately with `VolatileDataAccessOutOfGas`.
Across multiple volatile accesses, the most restrictive cap applies.

{% hint style="info" %}
**Rex4 (unstable): Relative Cap** — Rex4 changes detention from an absolute cap to a relative cap.
The effective detained limit becomes `current_usage + cap` at the time of access.
A transaction that has consumed 25M compute gas before reading TIMESTAMP gets an effective limit of 25M + 20M = 45M — it can still perform 20M more gas after the access.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.
{% endhint %}

### Oracle Forced-Cold SLOAD

All SLOAD operations on the [oracle contract](../system-contracts/oracle.md) use cold access gas cost (2100 gas) regardless of EIP-2929 warm/cold tracking state.
This ensures deterministic gas costs during block replay.

See [Gas Detention](gas-detention.md) for details.

## SELFDESTRUCT (EIP-6780)

`SELFDESTRUCT` is enabled with post-Cancun (EIP-6780) semantics:

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
| [Oracle](../system-contracts/oracle.md) | `0x634200...0001` | [MiniRex](spec-system.md#mini_rex) | Off-chain data key-value storage with hint support   |
| [High-Precision Timestamp](../oracle-services/timestamp.md) | `0x634200...0002` | [MiniRex](spec-system.md#mini_rex) | Sub-second timestamps ([oracle service](../oracle-services/README.md)) |
| [KeylessDeploy](../system-contracts/keyless-deploy.md) | `0x634200...0003` | [Rex2](spec-system.md#rex2) | Deterministic cross-chain deployment (Nick's Method) |

{% hint style="info" %}
**Rex4 (unstable): New System Contracts**

| Contract          | Address              | Purpose                                |
| ----------------- | -------------------- | -------------------------------------- |
| MegaAccessControl | `0x634200...0004`    | Proactive volatile data access control |
| MegaLimitControl  | `0x634200...0005`    | Query remaining compute gas budget     |

**MegaAccessControl** — Disable volatile data access for your call frame and all descendant calls via `disableVolatileDataAccess()`.
While disabled, any volatile access reverts with `VolatileDataAccessDisabled(uint8 accessType)` — no gas detention is triggered.
Call `enableVolatileDataAccess()` to lift the restriction; reverts with `DisabledByParent()` if an ancestor call frame holds the disable.
Call `isVolatileDataAccessDisabled()` to query the current state.
The restriction automatically ends when the disabling call frame returns.

**MegaLimitControl** — Query effective remaining compute gas via `remainingComputeGas()`.
Returns `min(frame_remaining, tx_detained_remaining)` at call time.

See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.
{% endhint %}

See [System Contracts Overview](../system-contracts/README.md) for the full registry and details.

## Precompile Gas Overrides

| Precompile             | Address | Cost Override                          |
| ---------------------- | ------- | -------------------------------------- |
| KZG Point Evaluation   | `0x0A`  | 100,000 gas (2× standard Prague cost)  |
| ModExp                 | `0x05`  | EIP-7883 gas schedule                  |

## Concept Deep Dives

| Topic | Description |
| ----- | ----------- |
| [Spec System](spec-system.md) | How MegaETH versions EVM behavior through a linear spec progression |
| [Dual Gas Model](dual-gas-model.md) | Compute gas vs storage gas formulas and SALT mechanics |
| [Resource Limits](resource-limits.md) | Transaction and block limit values, two-phase checking, enforcement |
| [Resource Accounting](resource-accounting.md) | How each resource dimension is tracked during execution |
| [Gas Detention](gas-detention.md) | Volatile data detection, cap mechanics, evolution across specs |
