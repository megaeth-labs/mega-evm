# Hardforks and Specs

MegaETH versions MegaEVM behavior through **hardforks** and **specs**.
This page defines both concepts and summarizes what each spec introduces.

## Hardfork vs Spec

The protocol distinguishes between two related concepts:

- **[Hardfork](glossary.md#hardfork-megahardfork)** — A network upgrade event: *when* changes are activated on the chain. A hardfork may include protocol-level changes beyond MegaEVM (e.g., networking, state sync, RPC behavior). Represented as `MegaHardfork` in the reference implementation.
- **[Spec](glossary.md#spec-megaspecid)** — A set of MegaEVM behaviors: *what* the EVM does. A spec captures only the execution-layer semantics. Represented as `MegaSpecId` in the reference implementation.

Multiple hardforks can map to the same spec.
A hardfork can also map to an older spec.
For example: `MiniRex` → `MINI_REX`, `MiniRex1` → `EQUIVALENCE` (rollback), `MiniRex2` → `MINI_REX` (restoration).

This documentation covers specs (MegaEVM behavior).
Protocol-level changes outside MegaEVM that are part of a hardfork are not covered here.

## Spec Progression

```
EQUIVALENCE → MINI_REX → REX → REX1 → REX2 → REX3 → REX4 (unstable)
```

Each newer spec includes all previous behaviors.
All specs build on Optimism Isthmus (Ethereum Prague) as the base layer.
The latest spec (currently REX4) may be marked **unstable**, meaning its semantics can still change before network activation.

### Backward Compatibility

EVM semantics for stable (activated) specs are frozen.
A new spec may add behavior or change the unstable spec, but it never alters what an existing stable spec does.
Every spec carries the invariant: "Stable pre-{Spec} semantics MUST remain unchanged."

This means:
- Contracts deployed under a given spec will continue to behave identically after future upgrades.
- Adding or modifying a system contract requires introducing a new spec.
- Changing gas costs, opcode behavior, or resource limits requires a new spec.
- Implementations should gate spec-specific behavior on the active spec (e.g., `spec.is_enabled(MINI_REX)`).

## Spec Summary

### EQUIVALENCE

The baseline spec.
Maintains full compatibility with Optimism Isthmus EVM.
Adds block environment access tracking for parallel execution support.

### MINI_REX

The first spec to introduce MegaETH-specific modifications:

- **[Dual gas model](evm/dual-gas-model.md)** — [Compute gas](glossary.md#compute-gas) + [storage gas](glossary.md#storage-gas)
- **[Multidimensional resource limits](evm/resource-limits.md)** — Compute gas (1B), data size (3.125 MB), KV updates (125K)
- **[Dynamic gas costs](evm/dual-gas-model.md#dynamic-salt-multiplier)** — [SALT bucket](glossary.md#salt-bucket)-based scaling
- **[Gas detention](evm/gas-detention.md)** — [Volatile data](glossary.md#volatile-data) access triggers gas limiting
- **SELFDESTRUCT disabled**
- **Large contract support** — 512 KB contracts (21x increase from 24 KB)

*See [MiniRex Network Upgrade](upgrades/minirex.md) for full details.*

### REX

Refines the [storage gas](glossary.md#storage-gas) economics introduced in MINI_REX:

- **Refined storage gas formulas** — Gradual scaling (20K–32K base costs vs MiniRex's 2M)
- **Transaction intrinsic storage gas** — 39,000 flat fee for all transactions
- **Zero cost fresh storage** — Minimum-sized [SALT buckets](glossary.md#salt-bucket) charge 0 storage gas
- **Security fixes** — DELEGATECALL, STATICCALL, CALLCODE properly enforce gas forwarding and oracle access detection
- **[State growth](evm/resource-accounting.md#state-growth) tracking** — New [resource limit](evm/resource-limits.md) dimension

*See [Rex Network Upgrade](upgrades/rex.md) for full details.*

### REX1

- **Limit reset fix** — Resets compute gas limits at the start of each transaction
- Inherits Rex semantics fully

*See [Rex1 Network Upgrade](upgrades/rex1.md) for full details.*

### REX2

- **SELFDESTRUCT restored** — Re-enabled with [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics
- **[KeylessDeploy](system-contracts/keyless-deploy.md) system contract** — Enables deterministic cross-chain deployment (Nick's Method)

*See [Rex2 Network Upgrade](upgrades/rex2.md) for full details.*

### REX3

- **Increased oracle access gas limit** — Raised from 1M to 20M
- **SLOAD-based [oracle](system-contracts/oracle.md) detention** — Triggers on SLOAD from oracle storage instead of CALL to oracle contract
- **[Keyless deploy](system-contracts/keyless-deploy.md) [compute gas](glossary.md#compute-gas) tracking** — Records the 100K overhead as compute gas

*See [Rex3 Network Upgrade](upgrades/rex3.md) for full details.*

### REX4 (unstable)

{% hint style="warning" %}
Rex4 is the current unstable specification and is subject to change before activation.
{% endhint %}

- **Per-[call-frame](glossary.md#call-frame) resource budgets** — All four [resource dimensions](glossary.md#resource-dimension) (compute gas, data size, KV updates, state growth) are bounded per call frame with 98/100 forwarding
- **Relative [gas detention](evm/gas-detention.md) cap** — Effective [detained limit](glossary.md#detained-limit) is `current_usage + cap` instead of an absolute cap
- **Storage gas stipend** — Value-transferring CALL/CALLCODE receives an additional 23,000 gas for [storage gas](glossary.md#storage-gas) operations, fixing LOG events in `receive()` under the [dual gas model](evm/dual-gas-model.md)
- **MegaAccessControl system contract** — Allows contracts to proactively disable [volatile data](glossary.md#volatile-data) access for a call subtree
- **MegaLimitControl system contract** — Allows querying effective remaining compute gas under detention and call frame limits
- **[Keyless deploy](system-contracts/keyless-deploy.md) sandbox environment inheritance** — Sandbox inherits parent transaction's external environment for dynamic pricing and oracle behavior

*See [Rex4 Network Upgrade](upgrades/rex4.md) for full details.*
