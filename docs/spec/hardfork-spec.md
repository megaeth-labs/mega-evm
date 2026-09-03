---
description: MegaETH hardfork and spec versioning — how behavioral changes are versioned, activated, and tracked across network upgrades.
spec: Rex6
---

# Hardforks and Specs

MegaETH versions its verifiable behavior through **hardforks** and **specs**.
This page defines both concepts and summarizes what each spec introduces.

## Hardfork vs Spec

The protocol distinguishes between two related concepts:

- **[Hardfork](glossary.md#hardfork-megahardfork)** — A network upgrade event: _when_ changes are activated on the chain. A hardfork may include protocol-level changes beyond MegaEVM (e.g., networking, state sync, RPC behavior).
- **[Spec](glossary.md#spec-megaspecid)** — A set of MegaETH verifiable behaviors: _what_ a correct node does. A spec captures the execution-layer semantics that determine node correctness.

Hardforks map one-to-one onto specs: every hardfork schedules a spec rung of its own.
A rollback is expressed by scheduling an alias spec — a rung whose behavior is identical to an earlier spec (see below).
For example: `MiniRex` → `MINI_REX`, `MiniRex1` → `MINI_REX_1` (behavior: `EQUIVALENCE`), `MiniRex2` → `MINI_REX_2` (behavior: `MINI_REX`).

### Alias Specs: Behavior vs. Position

Hardforks map one-to-one onto specs, so the spec resolved for a block — the latest activated hardfork's spec — only ever climbs.
A rollback is expressed by an **alias spec**: a rung of its own whose behavior is defined to be identical to an earlier spec.
`MiniRex1` maps to the alias spec `MINI_REX_1` (behavior: `EQUIVALENCE`), and `MiniRex2` maps to `MINI_REX_2` (behavior: `MINI_REX`).

The one resolved spec is therefore read through two projections that answer different questions.

- **Behavior** — which semantics the EVM applies in this block.
  An alias spec executes exactly its target's semantics: during the `MiniRex1` window MegaEVM behaves as `EQUIVALENCE` again.
- **Position** — how far the ladder has climbed.
  This is monotone and never decreases; an alias rung stands above the specs whose behavior it rolls back.

The distinction matters for chain setup that is one-way.
System contracts predeployed under a hardfork remain deployed, and pre-block rules remain in effect, even while an alias rung rolls the executing semantics back.
A rollback changes how transactions execute; it does not un-deploy a contract or retract a system call.

A node MUST determine pre-block setup — system-contract predeploys, their bytecode versions, and the fail-closed rules on the pre-block EIP-2935/EIP-4788 system calls — from the resolved spec's position.
A node MUST determine all other behavior — opcode behavior, gas costs, resource limits, transaction classification — from the resolved spec's behavior.

Spec ranges elsewhere in this specification — "from MiniRex onward", "MiniRex through Rex5" — are stated on behavior.
An alias rung counts as its behavior target in such ranges: during the `MiniRex1` window, a rule stated "from MiniRex onward" does not apply, because the behavior in effect is `EQUIVALENCE`.

A published hardfork schedule MUST climb the spec ladder rung by rung: a hardfork MUST NOT be scheduled unless every hardfork of a lower rung is scheduled, with one exception — a network MAY omit an alias hardfork, since an alias rung carries no setup of its own.
Execution is additionally robust to a malformed schedule: because setup derives from position, a scheduled hardfork implies its predecessors' setup even if they were never scheduled.

This documentation covers specs — the verifiable behavioral definitions that determine correctness of a MegaETH node.
Protocol-level changes outside the verifiable execution layer (e.g., networking, peer discovery) that are part of a hardfork are not covered here.

## Spec Progression

```
EQUIVALENCE → MINI_REX → MINI_REX_1 → MINI_REX_2 → REX → REX1 → REX2 → REX3 → REX4 → REX5 → REX6 → REX7
```

Each newer behavior-introducing spec includes all previous behaviors.
The alias rungs `MINI_REX_1` (behavior: `EQUIVALENCE`) and `MINI_REX_2` (behavior: `MINI_REX`) are the exception: an alias rung introduces no behavior of its own and instead executes exactly its target's earlier behavior (see [Alias Specs](#alias-specs-behavior-vs-position)).
All specs build on Optimism Isthmus (Ethereum Prague) as the base layer.
All specs through REX6 are frozen; REX7 is **unstable** and under active development.

Frozen and activated are separate properties.
A frozen spec's semantics no longer change, but it takes effect on a network only once that network schedules the corresponding hardfork.
REX7 is unstable and has no activation timestamp on either mainnet or testnet.

### Backward Compatibility

A frozen spec's EVM semantics never change again.
Freezing is what confers that guarantee — not activation: a frozen spec no network has scheduled is as immutable as one running on mainnet, because a future schedule would replay history against it.
A new spec may add behavior, but it never alters what an existing frozen spec does.

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

### MINI_REX_1 and MINI_REX_2

Alias rungs with no behavior of their own.

- `MINI_REX_1` (behavior: `EQUIVALENCE`) — scheduled by the `MiniRex1` hardfork; rolls execution semantics back to `EQUIVALENCE`.
- `MINI_REX_2` (behavior: `MINI_REX`) — scheduled by the `MiniRex2` hardfork; restores `MINI_REX` semantics.

See [Alias Specs](#alias-specs-behavior-vs-position) for how behavior and position project from an alias rung.

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

REX6 is frozen and scheduled on both networks; see the [upgrade overview](upgrades/overview.md) for activation timestamps.

- **Unified per-opcode gas metering order** — Every storage-affecting opcode charges storage gas before its body and records compute gas exactly once after the body completes; the `CREATE2` memory-expansion gas is folded into that single recording.
- **Consolidated EIP-7702 authorization accounting** — Per-authorization data-size and KV-update charges are narrowed to applied authorizations, authority state growth resolves during validation, net-new authorities pay dynamic SALT account-creation gas, and an applied authority equal to the block beneficiary triggers beneficiary gas detention.
- **CREATE-frame resource accounting** — The creator nonce-bump write is booked to the parent frame's discardable lane, and `CREATE` records state growth only for net-new addresses.
- **[KeylessDeploy](system-contracts/keyless-deploy.md) sandbox hardening** — Unused gas is rescued on a transaction-level compute-gas halt, and a self-destructing constructor is classified as an empty-code deployment.
- **Post-execution fee-reward accounting** — Account writes performed by the post-execution fee-reward step count toward resource accounting.
- **System-originated transaction metering exemption** — Pre-block system calls and [Mega System Transactions](system-contracts/system-tx.md) charge storage gas at minimum bucket capacity and are not halted by resource limits or gas detention.
- **Beneficiary detention / volatile-access coverage** — A `SELFDESTRUCT` executed by the beneficiary comes under `disableVolatileDataAccess`, and CALL-family targets whose EIP-7702 delegate resolves to the beneficiary come under both detention and `disableVolatileDataAccess`; a `SELFDESTRUCT` balance credit to an existing beneficiary counts toward resource accounting.
- **Additional resource-accounting corrections** — A per-log data-size base is charged for the log address, and forwarded gas returns to the parent when a `CALL` / `CREATE` halts on the compute-gas limit.
- **Value self-transfer dedup** — A value transfer whose target equals the caller is counted as a single account-info write.

_See [Rex6 Network Upgrade](upgrades/rex6.md) for full details._

### REX7

REX7 is the current **unstable** spec under active development.
It introduces no behavioral change over REX6 yet; its semantics may change at any time before it is frozen.

_See [Rex7 Network Upgrade](upgrades/rex7.md) for the current state._
