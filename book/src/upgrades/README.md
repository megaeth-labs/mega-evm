# Overview

This section documents each MegaETH network upgrade, describing what changed, why, and what developers need to know.

## History

| Upgrade | Key Changes |
| ------- | ----------- |
| [MiniRex](minirex.md) | [Dual gas model](../evm/dual-gas-model.md), [multidimensional resource limits](../evm/resource-limits.md), [gas detention](../evm/gas-detention.md), 98/100 gas forwarding, SELFDESTRUCT disabled, system contracts ([Oracle](../system-contracts/oracle.md), [Timestamp](../oracle-services/timestamp.md)) |
| MiniRex1 | Rollback: reverted to Equivalence spec (maps to `EQUIVALENCE`) |
| MiniRex2 | Restoration: re-activated MiniRex spec (maps to `MINI_REX`) |
| [Rex](rex.md) | Transaction intrinsic [storage gas](../glossary.md#storage-gas), revised storage gas economics (`base × (multiplier − 1)`), consistent CALL-like opcode behavior, [state growth](../evm/resource-accounting.md#state-growth) tracking |
| [Rex1](rex1.md) | Fix: [compute gas](../glossary.md#compute-gas) limit reset between transactions |
| [Rex2](rex2.md) | SELFDESTRUCT re-enabled ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780)), [KeylessDeploy](../system-contracts/keyless-deploy.md) system contract |
| [Rex3](rex3.md) | [Oracle](../system-contracts/oracle.md) gas cap raised to 20M, SLOAD-based oracle detention, keyless deploy compute gas tracking |
| [Rex4](rex4.md) | Per-[call-frame](../glossary.md#call-frame) resource budgets, relative gas detention, MegaAccessControl, MegaLimitControl, sandbox environment inheritance |

## How to Read These Pages

Each upgrade page follows a consistent structure:

- **Summary** — What changed and why
- **What Changed** — Previous vs new behavior for each change
- **Developer Impact** — What you need to care about
- **Safety and Compatibility** — Backward-compatibility boundaries and failure-mode differences
- **References** — Links to the normative spec and related documentation
