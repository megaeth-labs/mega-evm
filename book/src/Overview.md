# Overview

MegaEVM is a specialized Ethereum Virtual Machine built on [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/bluealloy/op-revm) for the [MegaETH](https://megaeth.com) network.

## Why a Modified EVM?

MegaETH is designed for real-time performance: sub-millisecond block times, extremely low base fees, and transaction gas limits far higher than Ethereum's.
These properties break assumptions that standard EVM gas pricing relies on.

Under standard EVM rules, MegaETH's low fees and high gas limits would make storage operations — state writes, logs, calldata — dramatically underpriced relative to their cost to node operators.
A single transaction could bloat on-chain state or history data for nearly free.
At the same time, MegaETH's parallel transaction execution requires mechanisms to detect and limit conflicts between concurrently executing transactions that read shared data.

MegaEVM solves these problems with a targeted set of modifications on top of Optimism Isthmus (Ethereum Prague):

- **[Dual Gas Model](evm/dual-gas-model.md)** — Adds a [storage gas](glossary.md#storage-gas) dimension on top of standard [compute gas](glossary.md#compute-gas), so state-heavy operations pay their true cost even when base fees are low.
- **[Multidimensional Resource Limits](evm/resource-limits.md)** — Enforces four independent per-transaction limits (compute gas, data size, KV updates, state growth) beyond the standard gas limit, preventing any single resource from being exhausted by a single transaction.
- **[Gas Detention](evm/gas-detention.md)** — Caps remaining compute gas after a transaction reads [volatile data](glossary.md#volatile-data) (block environment, oracle storage), forcing such transactions to terminate quickly and reducing parallel execution conflicts.
- **[Dynamic Gas Costs](evm/dual-gas-model.md#dynamic-salt-multiplier)** — Scales storage gas based on [SALT bucket](glossary.md#salt-bucket) capacity, making writes to crowded state regions progressively more expensive while keeping fresh storage free.
- **[System Contracts](system-contracts/README.md)** — Pre-deployed contracts providing oracle services, high-precision timestamps, keyless deployment, and runtime access/limit control.

All standard EVM semantics are inherited unless explicitly overridden.
Contracts that don't touch MegaETH-specific features (storage-heavy patterns, volatile data, system contracts) behave identically to Optimism.

## How to Use This Book

This book serves two audiences:

**App builders** — developers building smart contracts and dApps on MegaETH.
Start with the [EVM Overview](evm/README.md) for a complete reference of current behavior, then check the [System Contracts](system-contracts/README.md) for available infrastructure.
You primarily need to understand how gas costs differ and what resource limits apply to your transactions.

**Node builders** — teams implementing MegaETH-compatible execution clients.
Start with the [Spec System](evm/spec-system.md) to understand how behavior is versioned, then read each [Network Upgrade](upgrades/README.md) page for the exact behavioral deltas introduced at each spec.

## EVM Version

| Component        | Version                                                          |
| ---------------- | ---------------------------------------------------------------- |
| **EVM**     | [revm v27.1.0](https://github.com/bluealloy/revm)               |
| **Optimism EVM** | [op-revm v8.1.0](https://github.com/bluealloy/op-revm)          |
| **Alloy EVM**    | [alloy-evm v0.15.0](https://github.com/alloy-rs/evm)            |

## Spec Progression

MegaEVM uses a spec system (`MegaSpecId`) that defines EVM behavior at each stage.
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

{% hint style="warning" %}
Rex4 is the current unstable specification and is subject to change before activation.
{% endhint %}


See [Spec System](evm/spec-system.md) for the full spec-vs-hardfork distinction and details.
