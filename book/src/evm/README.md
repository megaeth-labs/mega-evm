# EVM Overview

MegaETH EVM extends the Optimism Isthmus EVM with several key modifications for high-performance parallel execution.

## Key Concepts

| Concept | Description |
| ------- | ----------- |
| [Spec System](spec-system.md) | How MegaETH versions EVM behavior through a linear spec progression |
| [Dual Gas Model](dual-gas-model.md) | Separating compute gas and storage gas for independent pricing |
| [Resource Limits](resource-limits.md) | Four independent per-transaction limits beyond standard gas |
| [Resource Accounting](resource-accounting.md) | How each resource dimension is tracked during execution |
| [Gas Detention](gas-detention.md) | Restricting computation after volatile data access |

## Design Principles

MegaETH's EVM modifications serve two primary goals:

1. **Parallel execution** — Minimize conflicts between concurrent transactions by detecting and restricting access to shared ("volatile") state.
2. **State sustainability** — Prevent state bloat through multidimensional resource limits and dynamic storage pricing.

All modifications are backward-compatible: each new spec includes all previous behaviors, and existing specs are never changed.
