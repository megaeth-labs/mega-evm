---
description: MegaETH hardfork and spec versioning — how behavioral changes are versioned, activated, and tracked across network upgrades.
spec: Rex5
---

# Hardforks and Specs

MegaETH versions its verifiable behavior through **hardforks** and **specs**.
This page defines both concepts and summarizes what each spec introduces.

## Hardfork vs Spec

The protocol distinguishes between two related concepts:

- **[Hardfork](glossary.md#hardfork-megahardfork)** — A network upgrade event: _when_ changes are activated on the chain. A hardfork may include protocol-level changes beyond MegaEVM (e.g., networking, state sync, RPC behavior).
- **[Spec](glossary.md#spec-megaspecid)** — A set of MegaETH verifiable behaviors: _what_ a correct node does. A spec captures the execution-layer semantics that determine node correctness.

Multiple hardforks can map to the same spec.
A hardfork can also map to an older spec.
For example: `MiniRex` → `MINI_REX`, `MiniRex1` → `EQUIVALENCE` (rollback), `MiniRex2` → `MINI_REX` (restoration).

This documentation covers specs — the verifiable behavioral definitions that determine correctness of a MegaETH node.
Protocol-level changes outside the verifiable execution layer (e.g., networking, peer discovery) that are part of a hardfork are not covered here.

## Spec Progression

```
EQUIVALENCE → MINI_REX → REX → REX1 → REX2 → REX3 → REX4 → REX5 → REX6
```

Each newer spec includes all previous behaviors.
All specs build on Optimism Isthmus (Ethereum Prague) as the base layer.
All specs through REX5 are stable (frozen); REX6 is **unstable** and under active development.

### Backward Compatibility

EVM semantics for stable (activated) specs are frozen.
A new spec may add behavior, but it never alters what an existing stable spec does.
Every spec carries the invariant that stable pre-{Spec} semantics remain unchanged.

This means:

- Contracts deployed under a given spec continue to behave identically after future upgrades.
- Adding or modifying a system contract requires introducing a new spec.
- Changing gas costs, opcode behavior, or resource limits requires a new spec.
- Implementations gate spec-specific behavior on the active spec.

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

_See [MiniRex Network Upgrade](upgrades/minirex.md) for full details._

### REX

Refines the [storage gas](glossary.md#storage-gas) economics introduced in MINI_REX:

- **Refined storage gas formulas** — Gradual scaling (20K–32K base costs vs MiniRex's 2M)
- **Transaction intrinsic storage gas** — 39,000 flat fee for all transactions
- **Zero cost fresh storage** — Minimum-sized [SALT buckets](glossary.md#salt-bucket) charge 0 storage gas
- **Security fixes** — DELEGATECALL, STATICCALL, CALLCODE properly enforce gas forwarding and oracle access detection
- **[State growth](evm/resource-accounting.md#state-growth) tracking** — New [resource limit](evm/resource-limits.md) dimension

_See [Rex Network Upgrade](upgrades/rex.md) for full details._

### REX1

- **Limit reset fix** — Resets compute gas limits at the start of each transaction
- Inherits Rex semantics fully

_See [Rex1 Network Upgrade](upgrades/rex1.md) for full details._

### REX2

- **SELFDESTRUCT restored** — Re-enabled with [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics
- **[KeylessDeploy](system-contracts/keyless-deploy.md) system contract** — Enables deterministic cross-chain deployment (Nick's Method)

_See [Rex2 Network Upgrade](upgrades/rex2.md) for full details._

### REX3

- **Increased oracle access gas limit** — Raised from 1M to 20M
- **SLOAD-based [oracle](system-contracts/oracle.md) detention** — Triggers on SLOAD from oracle storage instead of CALL to oracle contract
- **[Keyless deploy](system-contracts/keyless-deploy.md) [compute gas](glossary.md#compute-gas) tracking** — Records the 100K overhead as compute gas

_See [Rex3 Network Upgrade](upgrades/rex3.md) for full details._

### REX4

- **Per-[call-frame](glossary.md#call-frame) resource budgets** — All four [resource dimensions](glossary.md#resource-dimension) (compute gas, data size, KV updates, state growth) are bounded per call frame with 98/100 forwarding
- **Relative [gas detention](evm/gas-detention.md) cap** — Effective [detained limit](glossary.md#detained-limit) is `current_usage + cap` instead of an absolute cap
- **[Storage gas stipend](glossary.md#storage-gas-stipend)** — Value-transferring CALL/CALLCODE receives an additional 23,000 gas for [storage gas](glossary.md#storage-gas) operations, fixing LOG events in `receive()` under the [dual gas model](evm/dual-gas-model.md)
- **MegaAccessControl system contract** — Allows contracts to proactively disable [volatile data](glossary.md#volatile-data) access for a call subtree
- **MegaLimitControl system contract** — Allows querying effective remaining compute gas under detention and call frame limits
- **[Keyless deploy](system-contracts/keyless-deploy.md) sandbox environment inheritance** — Sandbox inherits parent transaction's external environment for dynamic pricing and oracle behavior

_See [Rex4 Network Upgrade](upgrades/rex4.md) for full details._

### REX5

- **[SequencerRegistry](system-contracts/sequencer-registry.md) system contract** — Tracks the system address and sequencer roles independently with on-chain change scheduling and history.
- **Dynamic system address** — `MEGA_SYSTEM_ADDRESS` is resolved per block from `SequencerRegistry.currentSystemAddress()` instead of a hardcoded constant.
- **Oracle v2.0.0** — `onlySystemAddress` reads the authority from `SequencerRegistry`. In-place Oracle bytecode upgrades preserve existing storage instead of clearing it.
- **Caller-account update deduplication** — Fixes overcounting of caller-account data-size and KV updates across multiple value-transferring sub-calls or creates from the same parent frame.
- **[KeylessDeploy](system-contracts/keyless-deploy.md) trailing-bytes rejection** — RLP encodings with trailing bytes after the signed payload are rejected with `MalformedEncoding()`.
- **CALLCODE new-account storage gas fix** — New-account storage gas is now charged against the caller's storage context rather than the code-source address.
- **Storage-gas-stipend separated allowance** — The `STORAGE_CALL_STIPEND` no longer inflates the callee's gas limit; it is a per-frame allowance drawn only at storage-gas surcharge sites and is neither burned nor rescued.
- **Value-transfer CALL/CALLCODE compute attribution** — Compute gas recorded into the parent excludes the `CALL_STIPEND` granted to the callee.
- **CREATE code-deposit compute-gas atomicity** — Contract-creation code-deposit compute gas is charged atomically with the deployment commit and not double-counted in post-execution accounting.
- **EIP-2935 / EIP-4788 pre-block gas floor** — The history-storage and beacon-roots pre-block system calls use `max(block_gas_limit, 30,000,000)` and are fail-closed: a non-successful call rejects the block.
- **CREATE2 empty-initcode short-circuit** — A `CREATE2` with zero-length init code short-circuits after the salt check using the empty-code hash, without offset conversion, memory expansion, or hashing.
- **KeylessDeploy empty-code log forwarding** — An empty-runtime-code deployment success forwards the constructor's logs before returning.

_See [Rex5 Network Upgrade](upgrades/rex5.md) for full details._

### REX6

REX6 is the current **unstable** spec under active development; its semantics may still change before network activation.

- **Unified per-opcode gas metering order** — Every storage-affecting opcode charges storage gas before its body and records compute gas exactly once after the body completes; the `CREATE2` memory-expansion gas is folded into that single recording.
- **Consolidated EIP-7702 authorization accounting** — Per-authorization data-size and KV-update charges are narrowed to applied authorizations, authority state growth resolves during validation, net-new authorities pay dynamic SALT account-creation gas, and an applied authority equal to the block beneficiary triggers beneficiary gas detention.
- **CREATE-frame resource accounting** — The creator nonce-bump write is booked to the parent frame's discardable lane, and `CREATE` records state growth only for net-new addresses.
- **[KeylessDeploy](system-contracts/keyless-deploy.md) sandbox hardening** — Unused gas is rescued on a transaction-level compute-gas halt, and a self-destructing constructor is classified as an empty-code deployment.
- **Post-execution fee-reward accounting** — Account writes performed by the post-execution fee-reward step count toward resource accounting.
- **System-originated transaction metering exemption** — Pre-block system calls and [Mega System Transactions](system-contracts/system-tx.md) charge storage gas at minimum bucket capacity and are not halted by resource limits or gas detention.
- **Beneficiary detention / volatile-access coverage** — A `SELFDESTRUCT` executed by the beneficiary and CALL-family targets whose EIP-7702 delegate resolves to the beneficiary come under detention and `disableVolatileDataAccess`; a `SELFDESTRUCT` balance credit to an existing beneficiary counts toward resource accounting.
- **Additional resource-accounting corrections** — A per-log data-size base is charged for the log address, and forwarded gas returns to the parent when a `CALL` / `CREATE` halts on the compute-gas limit.
- **Value self-transfer dedup** — A value transfer whose target equals the caller is counted as a single account-info write.

_See [Rex6 Network Upgrade](upgrades/rex6.md) for full details._
