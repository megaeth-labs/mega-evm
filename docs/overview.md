# MegaETH Specification

This documentation is the specification for the [MegaETH](https://megaeth.com) blockchain's execution layer.
It covers everything that differs from standard Ethereum and Optimism: the EVM modifications, system contracts, oracle services, resource metering, and the upgrade history that defines how these features evolved.

The reference implementation is [MegaEVM](https://github.com/megaeth-labs/mega-evm), built on [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/bluealloy/op-revm).
All standard EVM semantics are inherited from Optimism Isthmus (Ethereum Prague) unless explicitly overridden.
Contracts that don't touch MegaETH-specific features behave identically to Optimism.

## Why MegaETH Differs from Standard Ethereum

MegaETH is designed for real-time performance: sub-millisecond block times, extremely low base fees, and transaction gas limits far higher than Ethereum's.
These properties break assumptions that standard EVM gas pricing relies on.

Under standard EVM rules, MegaETH's low fees and high gas limits would make storage operations — state writes, logs, calldata — dramatically underpriced relative to their cost to node operators.
A single transaction could bloat on-chain state or history data for nearly free.
At the same time, MegaETH's parallel transaction execution requires mechanisms to detect and limit conflicts between concurrently executing transactions that read shared data.

MegaETH addresses these problems with a targeted set of protocol-level modifications:

- **[Dual Gas Model](evm/dual-gas-model.md)** — Adds a [storage gas](glossary.md#storage-gas) dimension on top of standard [compute gas](glossary.md#compute-gas), so state-heavy operations pay their true cost even when base fees are low.
- **[Multidimensional Resource Limits](evm/resource-limits.md)** — Enforces four independent per-transaction limits (compute gas, data size, KV updates, state growth) beyond the standard gas limit, preventing any single resource from being exhausted by a single transaction.
- **[Gas Detention](evm/gas-detention.md)** — Caps remaining compute gas after a transaction reads [volatile data](glossary.md#volatile-data) (block environment, oracle storage), forcing such transactions to terminate quickly and reducing parallel execution conflicts.
- **[Dynamic Gas Costs](evm/dual-gas-model.md#dynamic-salt-multiplier)** — Scales storage gas based on [SALT bucket](glossary.md#salt-bucket) capacity, making writes to crowded state regions progressively more expensive while keeping fresh storage free.
- **[System Contracts](system-contracts/overview.md)** — Pre-deployed protocol contracts providing oracle storage, high-precision timestamps, keyless deployment, and runtime access/limit control.
- **[Oracle Services](oracle-services/overview.md)** — Sequencer-operated data feeds (timestamps, price feeds) built on the Oracle system contract.

## How to Use This Documentation

This documentation serves two audiences:

**App builders** — developers building smart contracts and dApps on MegaETH.
Start with the [MegaEVM Overview](evm/overview.md) for a complete reference of current behavior, then check the [System Contracts](system-contracts/overview.md) and [Oracle Services](oracle-services/overview.md) for available infrastructure.
You primarily need to understand how gas costs differ and what resource limits apply to your transactions.

**Node builders** — teams implementing MegaETH-compatible execution clients.
Start with [Hardforks and Specs](hardfork-spec.md) to understand how behavior is versioned, then read each [Network Upgrade](upgrades/overview.md) page for the exact behavioral deltas introduced at each spec.

## Reference Implementation

| Component        | Version                                                          |
| ---------------- | ---------------------------------------------------------------- |
| **MegaEVM**      | [mega-evm](https://github.com/megaeth-labs/mega-evm)            |
| **Base EVM**     | [revm v27.1.0](https://github.com/bluealloy/revm)               |
| **Optimism EVM** | [op-revm v8.1.0](https://github.com/bluealloy/op-revm)          |
| **Alloy EVM**    | [alloy-evm v0.15.0](https://github.com/alloy-rs/evm)            |

## Spec Progression

MegaETH uses a spec system to version MegaEVM behavior at each stage of the protocol's evolution.
Each newer spec includes all previous behaviors:

```
EQUIVALENCE → MINI_REX → REX → REX1 → REX2 → REX3 → REX4 (unstable)
```

{% hint style="info" %}
**Backward Compatibility** — EVM semantics for stable (activated) specs are frozen.
A new spec may add behavior, but it never changes what an existing stable spec does.
Contracts deployed under a given spec will continue to behave identically, regardless of future upgrades.
{% endhint %}

- **EQUIVALENCE** — Baseline. Full Optimism Isthmus compatibility with block environment access tracking for parallel execution.
- **MINI_REX** — Dual gas model, multidimensional resource limits, gas detention, 98/100 gas forwarding, SELFDESTRUCT disabled, Oracle and Timestamp system contracts.
- **REX** — Revised storage gas economics (`base × (multiplier − 1)`), transaction intrinsic storage gas, state growth tracking, consistent CALL-like opcode behavior.
- **REX1** — Fix: compute gas limit reset between transactions.
- **REX2** — SELFDESTRUCT re-enabled (EIP-6780), KeylessDeploy system contract.
- **REX3** — Oracle gas cap raised to 20M, SLOAD-based oracle detention, keyless deploy compute gas tracking.
- **REX4** *(unstable)* — Per-call-frame resource budgets, relative gas detention, MegaAccessControl and MegaLimitControl system contracts.

See [Hardforks and Specs](hardfork-spec.md) for full details.
