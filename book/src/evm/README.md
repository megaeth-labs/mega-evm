# Current EVM Standard

This page describes the current MegaETH EVM behavior as a single reference.
It reflects the latest spec (Rex4) and serves as the starting point for developers building on MegaETH.
For incremental changes introduced by each upgrade, see the [Network Upgrades](../upgrades/README.md) section.
For deep dives on individual topics, see the linked concept pages below.

## Base Layer

MegaETH EVM builds on Optimism Isthmus (Ethereum Prague).
All standard EVM semantics are inherited unless explicitly overridden.

## Dual Gas Model

Every transaction's total gas cost is the sum of two independent components:

```
total gas used = compute gas + storage gas
```

- **Compute gas** — Standard Optimism EVM (Isthmus) gas costs for all opcodes and operations.
- **Storage gas** — Additional costs for operations that impose persistent storage burden on nodes.

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
| **SSTORE (0 → non-0)**    | 20,000 × (multiplier − 1)  | Only for zero-to-non-zero transitions              |
| **Account creation**       | 25,000 × (multiplier − 1)  | Value transfer to empty account                    |
| **Contract creation**      | 32,000 × (multiplier − 1)  | CREATE/CREATE2 opcodes or creation transactions    |
| **Code deposit**           | 10,000/byte                 | Per byte when contract creation succeeds           |
| **LOG topic**              | 3,750/topic                 | Per topic, regardless of revert                    |
| **LOG data**               | 80/byte                     | Per byte, regardless of revert                     |
| **Calldata (zero)**        | 40/byte                     | Per zero byte in transaction input                 |
| **Calldata (non-zero)**    | 160/byte                    | Per non-zero byte in transaction input             |
| **Floor (zero)**           | 100/byte                    | EIP-7623 floor cost for zero bytes                 |
| **Floor (non-zero)**       | 400/byte                    | EIP-7623 floor cost for non-zero bytes             |

### Dynamic SALT Multiplier

Storage gas costs for SSTORE, account creation, and contract creation scale dynamically based on SALT bucket capacity.

**Formula**: `multiplier = bucket_capacity / MIN_BUCKET_SIZE`

- At `multiplier = 1` (minimum bucket size): **zero storage gas** — fresh storage is free.
- At `multiplier > 1`: linear scaling makes crowded state regions more expensive.

See [Dual Gas Model](dual-gas-model.md) for details.

## Multidimensional Resource Limits

MegaETH enforces four independent post-execution resource limits beyond standard gas:

| Resource         | Transaction Limit        | Block Limit     |
| ---------------- | ------------------------ | --------------- |
| Compute gas      | 200,000,000 (200M)      | Unlimited       |
| Data size        | 13,107,200 (12.5 MB)    | 13,107,200      |
| KV updates       | 500,000                  | 500,000         |
| State growth     | 1,000                    | 1,000           |

When any transaction-level limit is exceeded during execution, the transaction halts with `OutOfGas`.
Remaining gas is preserved and refunded — not consumed.

### Per-Frame Resource Budgets

Each call frame receives a bounded share of the remaining resources:

- The top-level frame starts with the full transaction budget for each dimension.
- Each inner frame receives `remaining × 98/100` of its parent's remaining budget.
- A frame-local exceed **reverts** that frame with `MegaLimitExceeded(uint8 kind, uint64 limit)` — it does not halt the transaction.
- The parent frame can continue executing after a child frame reverts.
- Compute gas consumed by reverted child frames still counts toward the transaction total.

| kind | Resource     |
| ---- | ------------ |
| 0    | Data size    |
| 1    | KV updates   |
| 2    | Compute gas  |
| 3    | State growth |

See [Resource Limits](resource-limits.md) and [Resource Accounting](resource-accounting.md) for details.

## Gas Forwarding (98/100 Rule)

All CALL-like opcodes (CALL, STATICCALL, DELEGATECALL, CALLCODE) and CREATE/CREATE2 forward at most **98/100** of remaining gas to subcalls, replacing the standard EVM's 63/64 rule.
This prevents call-depth attacks under MegaETH's high gas limits.

## Gas Detention

Accessing **volatile data** — block environment fields, the beneficiary's account, or oracle storage — triggers a compute gas cap that forces the transaction to terminate quickly.
This reduces parallel execution conflicts without banning the access outright.
Detained gas is refunded at transaction end.

### Volatile Data Categories

| Category                    | Trigger                                               | Cap   |
| --------------------------- | ----------------------------------------------------- | ----- |
| Block env / Beneficiary     | NUMBER, TIMESTAMP, COINBASE, etc. or beneficiary access | 20M  |
| Oracle                      | SLOAD from oracle contract storage                     | 20M  |

### Relative Cap

The effective detained limit is calculated relative to current usage:

```
effective_detained_limit = current_usage + cap
```

A transaction that has consumed 25M compute gas before reading TIMESTAMP gets an effective limit of 25M + 20M = 45M — it can still perform 20M more gas of computation after the access.

Across multiple volatile accesses, the most restrictive effective limit applies.

### Oracle Forced-Cold SLOAD

All SLOAD operations on the oracle contract use cold access gas cost (2100 gas) regardless of EIP-2929 warm/cold tracking state.
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
| Max initcode   | 536,576 bytes (512 KB + 24 KB) |

These are 21× larger than standard Ethereum limits (24 KB / 48 KB).

## System Contracts

MegaETH pre-deploys system contracts at well-known addresses:

| Contract                 | Address                                         | Purpose                                             |
| ------------------------ | ----------------------------------------------- | --------------------------------------------------- |
| Oracle                   | `0x634200...0001`                                | Off-chain data key-value storage with hint support   |
| High-Precision Timestamp | `0x634200...0002`                                | Sub-second block timestamp                           |
| KeylessDeploy            | `0x634200...0003`                                | Deterministic cross-chain deployment (Nick's Method) |
| MegaAccessControl        | `0x634200...0004`                                | Proactive volatile data access control               |
| MegaLimitControl         | `0x634200...0005`                                | Query remaining compute gas budget                   |

### MegaAccessControl

You can disable volatile data access for your frame and all descendant calls by calling `disableVolatileDataAccess()`.
While disabled, any volatile access reverts immediately with `VolatileDataAccessDisabled(uint8 accessType)` — no gas detention is triggered.
A descendant frame cannot re-enable access disabled by an ancestor.
The restriction ends when the disabling frame returns.

### MegaLimitControl

You can query your effective remaining compute gas by calling `remainingComputeGas()`.
The returned value equals `min(frame_remaining, tx_detained_remaining)` at call time — a single reliable number accounting for both frame budgets and detention.

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
