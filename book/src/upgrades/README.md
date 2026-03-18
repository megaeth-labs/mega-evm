# Network Upgrades

This section documents each MegaETH network upgrade, describing what changed, why, and what developers need to know.

## Upgrade History

| Upgrade | Key Changes |
| ------- | ----------- |
| [MiniRex](minirex.md) | Dual gas model, multidimensional resource limits, gas detention, 98/100 gas forwarding, SELFDESTRUCT disabled, system contracts (Oracle, Timestamp) |
| [Rex](rex.md) | Transaction intrinsic storage gas, revised storage gas economics (`base × (multiplier − 1)`), consistent CALL-like opcode behavior, state growth tracking |
| [Rex1](rex1.md) | Fix: compute gas limit reset between transactions |
| [Rex2](rex2.md) | SELFDESTRUCT re-enabled (EIP-6780), KeylessDeploy system contract |
| [Rex3](rex3.md) | Oracle gas cap raised to 20M, SLOAD-based oracle detention, keyless deploy compute gas tracking |
| [Rex4](rex4.md) | Per-frame resource budgets, relative gas detention, MegaAccessControl, MegaLimitControl, sandbox environment inheritance |

## How to Read These Pages

Each upgrade page follows a consistent structure:

- **Summary** — What changed and why
- **What Changed** — Previous vs new behavior for each change
- **Developer Impact** — What you need to care about
- **Safety and Compatibility** — Backward-compatibility boundaries and failure-mode differences
- **References** — Links to the normative spec and related documentation
