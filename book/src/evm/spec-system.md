# Spec System

MegaETH EVM uses a **spec system** (`MegaSpecId`) to define EVM behavior at each stage of the protocol's evolution.

## Spec vs Hardfork

The codebase distinguishes between two related concepts:

- **Spec (`MegaSpecId`)** — Defines EVM behavior: *what* the EVM does.
- **Hardfork (`MegaHardfork`)** — Defines network upgrade events: *when* specs are activated.

Multiple hardforks can map to the same spec.
For example, both `MiniRex` and `MiniRex2` hardforks use the `MINI_REX` spec.

## Spec Progression

```
EQUIVALENCE → MINI_REX → MINI_REX1 → MINI_REX2 → REX → REX1 → REX2 → REX3 → REX4
```

Each newer spec includes all previous behaviors.
All specs use `OpSpecId::ISTHMUS` as the Optimism base layer.

## Spec Summary

### EQUIVALENCE

The default spec.
Maintains full compatibility with Optimism Isthmus EVM.
Adds block environment access tracking for parallel execution support.

### MINI_REX

The first spec to introduce MegaETH-specific modifications:

- **Dual gas model** — Compute gas + storage gas
- **Multidimensional resource limits** — Compute gas (1B), data size (3.125 MB), KV updates (125K)
- **Dynamic gas costs** — SALT bucket-based scaling
- **Gas detention** — Volatile data access triggers gas limiting
- **SELFDESTRUCT disabled**
- **Large contract support** — 512 KB contracts (21x increase from 24 KB)

### REX

Refines the storage gas economics introduced in MINI_REX:

- **Refined storage gas formulas** — Gradual scaling (20K–32K base costs vs MiniRex's 2M)
- **Transaction intrinsic storage gas** — 39,000 flat fee for all transactions
- **Zero cost fresh storage** — Minimum-sized SALT buckets charge 0 storage gas
- **Security fixes** — DELEGATECALL, STATICCALL, CALLCODE properly enforce gas forwarding and oracle access detection
- **State growth tracking** — New resource limit dimension

### REX1

- **Limit reset fix** — Resets compute gas limits at the start of each transaction
- Inherits Rex semantics fully

### REX2

- **SELFDESTRUCT restored** — Re-enabled with EIP-6780 semantics
- **KeylessDeploy system contract** — Enables deterministic cross-chain deployment (Nick's Method)

### REX3

- **Increased oracle access gas limit** — Raised from 1M to 20M
- **SLOAD-based oracle detention** — Triggers on SLOAD from oracle storage instead of CALL to oracle contract
- **Keyless deploy compute gas tracking** — Records the 100K overhead as compute gas

### REX4

- **Per-frame resource budgets** — All four resource dimensions (compute gas, data size, KV updates, state growth) are bounded per call frame with 98/100 forwarding
- **Relative gas detention cap** — Effective detained limit is `current_usage + cap` instead of an absolute cap
- **MegaAccessControl system contract** — Allows contracts to proactively disable volatile data access for a call subtree
- **MegaLimitControl system contract** — Allows querying effective remaining compute gas under detention and frame limits
- **Keyless deploy sandbox environment inheritance** — Sandbox inherits parent transaction's external environment for dynamic pricing and oracle behavior
