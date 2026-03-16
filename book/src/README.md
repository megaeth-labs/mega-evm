# MegaETH EVM

A specialized Ethereum Virtual Machine (EVM) implementation tailored for [MegaETH](https://megaeth.com), built on top of [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/bluealloy/op-revm).

## What is MegaETH EVM?

MegaETH EVM extends the Optimism EVM with a set of modifications designed for high-performance blockchain operations.
Key differences from a standard EVM include:

- **Dual Gas Model** — Separates gas into compute gas and storage gas, enabling independent pricing of computation versus state operations.
- **Multidimensional Resource Limits** — Enforces four independent per-transaction limits (compute gas, data size, KV updates, state growth) beyond the standard gas limit.
- **Gas Detention** — Restricts computation after accessing volatile data (block env, oracle) to reduce parallel execution conflicts.
- **Dynamic Gas Costs** — Scales storage operation costs based on SALT bucket capacity, preventing state bloat.
- **System Contracts** — Pre-deployed contracts providing oracle services, keyless deployment, access control, and more.

## EVM Version

| Component        | Version                                                          |
| ---------------- | ---------------------------------------------------------------- |
| **Base EVM**     | [revm v27.1.0 (v83)](https://github.com/bluealloy/revm)         |
| **Optimism EVM** | [op-revm v8.1.0 (v83)](https://github.com/bluealloy/op-revm)    |
| **Alloy EVM**    | [alloy-evm v0.15.0](https://github.com/alloy-rs/core)           |

## Spec Progression

MegaETH EVM uses a spec system (`MegaSpecId`) that defines EVM behavior at each stage.
Each newer spec includes all previous behaviors:

```
EQUIVALENCE → MINI_REX → MINI_REX1 → MINI_REX2 → REX → REX1 → REX2 → REX3 → REX4
```

See [Spec System](evm/spec-system.md) for details.
