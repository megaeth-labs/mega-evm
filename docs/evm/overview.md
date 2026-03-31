---
description: MegaEVM specification index — dual gas model, resource limits, gas detention, system contracts, and per-spec behavioral history.
spec: Rex3
---

# MegaEVM Overview

This page summarizes the current MegaEVM behavior.
The linked concept pages are the authoritative specification for each behavior.

## Stable Scope

This page describes the current MegaEVM behavior.
For full per-upgrade behavioral deltas, see [Network Upgrades](../upgrades/overview.md).

## Specifications

### Rex4 (unstable)

Rex4 features are unstable and are not yet part of the current stable behavior.
Where relevant, unstable Rex4 behavior is described in expandable sections on concept pages.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for the full unstable spec.

### Inheritance Boundary

MegaEVM builds on Optimism Isthmus (Ethereum Prague).
Unless explicitly overridden by the MegaETH specification, standard EVM behavior is inherited from that baseline.

### Gas and Resource Model

MegaETH replaces the single-dimensional intuition of standard EVM gas with a two-dimensional model.
Every transaction is charged for both compute gas and storage gas, and the transaction's total gas usage is the sum of those two components.

Storage-heavy operations such as state writes, code deposit, logs, and calldata therefore carry additional cost beyond inherited EVM compute gas.
For the complete formulas, constants, SALT multiplier rules, and charging lifecycle, see [Dual Gas Model](dual-gas-model.md).

### Runtime Resource Limits and Accounting

In addition to the transaction's gas limit, MegaETH enforces separate runtime ceilings on compute gas, data size, KV updates, and state growth.
These dimensions are tracked independently and limit execution even when the transaction still has remaining total gas.

The protocol distinguishes between:

- **resource limits**, which define ceilings and enforcement outcomes, and
- **resource accounting**, which defines how each dimension is counted during execution and across reverted call frames.

For limits, see [Resource Limits](resource-limits.md).
For counting rules, revert behavior, and deduplication rules, see [Resource Accounting](resource-accounting.md).

### Gas Detention

MegaETH restricts post-access computation after a transaction reads [volatile data](../glossary.md#volatile-data).
This includes block-environment data, beneficiary-related access, and oracle-backed data.

The purpose of gas detention is to bound the amount of compute gas that may follow access to shared, conflict-prone inputs.
For the detention categories, cap semantics, halt conditions, and stable-versus-unstable behavior, see [Gas Detention](gas-detention.md).

### Execution Semantics Overrides

MegaEVM inherits the baseline semantics of Optimism Isthmus / Ethereum Prague, but overrides selected execution behaviors.
The current stable differences include gas forwarding, contract size limits, precompile pricing overrides, and `SELFDESTRUCT` semantics.

### Gas Forwarding

CALL-like opcodes and `CREATE`/`CREATE2` use the 98/100 forwarding rule in current stable behavior.
This differs from the standard EVM's 63/64 forwarding rule.
The 98/100 rule was introduced in [Rex](../upgrades/rex.md) for stable behavior.
For the exact forwarding rule, stipend interaction, and opcode scope, see [Gas Forwarding](gas-forwarding.md).

### SELFDESTRUCT

`SELFDESTRUCT` follows [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics.
If the contract was created in the same transaction, `SELFDESTRUCT` removes code and storage and transfers the balance.
Otherwise it transfers the balance only and preserves code and storage.
This behavior became part of MegaETH in [Rex2](../upgrades/rex2.md).
For the full stable semantics and earlier MiniRex disablement, see [SELFDESTRUCT](selfdestruct.md).

### Contract Limits

| Limit | Value |
| ----- | ----- |
| Max contract size | 524,288 bytes (512 KB) |
| Max initcode size | 548,864 bytes (512 KB + 24 KB) |

These enlarged limits were introduced in [MiniRex](../upgrades/minirex.md).
For the exact limits and rejection rules, see [Contract Limits](contract-limits.md).

### Precompiles

| Precompile | Address | Stable MegaETH-Specific Behavior |
| ---------- | ------- | --------------- |
| KZG Point Evaluation | `0x0A` | 100,000 gas |
| ModExp | `0x05` | [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) gas schedule |

These stable overrides are part of the current behavior.
For the full precompile specification, including the inherited baseline and MegaETH-specific differences, see [Precompiles](precompiles.md).

### Built-In Protocol Interfaces

MegaETH predeploys the following stable system contracts:

| Contract | Address | Since | Purpose |
| -------- | ------- | ----- | ------- |
| [Oracle](../system-contracts/oracle.md) | `0x6342000000000000000000000000000000000001` | [MiniRex](../upgrades/minirex.md) | Off-chain data key-value storage |
| [High-Precision Timestamp](../system-contracts/high-precision-timestamp.md) | `0x6342000000000000000000000000000000000002` | [MiniRex](../upgrades/minirex.md) | Sub-second timestamp oracle service |
| [KeylessDeploy](../system-contracts/keyless-deploy.md) | `0x6342000000000000000000000000000000000003` | [Rex2](../upgrades/rex2.md) | Deterministic cross-chain deployment |

For the full registry and behavioral semantics, see [System Contracts Overview](../system-contracts/overview.md).
