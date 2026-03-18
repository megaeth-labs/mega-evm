# Architecture: Key Concepts

These are domain-knowledge references for mega-evm.
Read this file when working on related modules.

## Backward Compatibility of Specs

The spec system (`MegaSpecId`) forms a linear progression where each newer spec includes all previous behaviors.
The codebase **MUST** maintain backward-compatibility: EVM semantics must never change for existing (stable) specs.
The only exception is the latest spec if explicitly marked as **unstable**.
Consequently:

- Adding/modifying a system contract requires introducing a new spec.
- Changing gas costs, opcode behavior, or resource limits requires a new spec.
- Code should use `spec.is_enabled(MegaSpecId::X)` to gate spec-specific behavior.
- Modified opcodes (e.g., SELFDESTRUCT behavior varies by spec) are wired per-spec in the instruction table (`evm/instructions.rs`).

## Dual Gas Model (Compute Gas vs Storage Gas)

MegaETH separates EVM gas into two independent dimensions tracked during execution:

- **Compute gas**: Measures pure computational cost.
  Every opcode's gas consumption is recorded via wrapped instructions (`compute_gas_ext` in `evm/instructions.rs`).
  Subject to a per-spec compute gas limit and further restricted by gas detention (see below).
- **Storage gas**: Charges for persistent state modifications (SSTORE, account creation, contract deployment).
  These costs scale dynamically with SALT bucket capacity (see External Environment Dependencies below).
  LOG opcodes are charged in both dimensions: standard compute gas rates plus a storage gas multiplier on topic and data costs.

Both dimensions are enforced independently.
A transaction can be halted by exceeding either limit.

## Multidimensional Resource Limits

Beyond the dual gas model, mega-evm enforces **four independent per-transaction resource limits** via `AdditionalLimit` (`limit/mod.rs`):

- **Compute gas** — Computational opcode cost
- **Data size** — Calldata + logs + storage writes + code deploy + account updates
- **KV updates** — Storage writes + account modifications (net, with refunds)
- **State growth** — Net new accounts + net new storage slots (not all specs enable this)

Each dimension has its own per-spec limit defined in `constants.rs`.
All trackers are **frame-aware**: reverted inner calls discard their tracked usage, successful calls merge into the parent frame.
When any limit is exceeded, execution halts with `OutOfGas` and remaining gas is preserved for refund.

## Gas Detention (Volatile Data Access Restriction)

MegaETH's parallel EVM needs to minimize conflicts between concurrent transactions.
"Volatile" data — block environment fields (NUMBER, TIMESTAMP, COINBASE, etc.), the beneficiary's account state, and the oracle contract — is frequently read by many transactions and thus a major source of conflicts.

**Gas detention** restricts computation after volatile data is accessed by capping the remaining compute gas:

- Different volatile data categories (block env/beneficiary, oracle) have different cap levels defined in `constants.rs`.
- The **most restrictive cap wins** when multiple volatile sources are accessed.
- Caps are applied via host hooks (`evm/host.rs`) that mark access in a `VolatileDataAccessTracker` (`access/tracker.rs`), then enforced after each volatile opcode via `wrap_op_detain_gas!` in `evm/instructions.rs`.

This forces transactions that touch volatile data to terminate quickly, reducing parallel execution conflicts without banning the access outright.
Detained gas is effectively refunded — users only pay for actual computation performed.

## System Contracts

MegaETH pre-deploys system contracts at well-known addresses (`0x634200...0001`, `0002`, `0003`, etc.).
They are deployed idempotently during `pre_execution_changes()` in `block/executor.rs`, gated by hardfork activation:

| Contract                 | Address suffix | Purpose                                             |
| ------------------------ | -------------- | --------------------------------------------------- |
| Oracle                   | `...0001`      | External key-value storage with hint support        |
| High-Precision Timestamp | `...0002`      | Sub-second block timestamp                          |
| Keyless Deploy           | `...0003`      | Deterministic contract deployment via Nick's Method |
| MegaAccessControl        | `...0004`      | Access control (disableVolatileDataAccess)          |
| MegaLimitControl         | `...0005`      | Limit query/control (currently remainingComputeGas) |

Key design aspects:

- Solidity sources in `crates/system-contracts/contracts/`, compiled by Foundry, with Rust ABI bindings generated via `alloy-sol-types`.
- Bytecode is versioned and hash-verified at build time (`crates/system-contracts/build.rs`).
- The **MEGA_SYSTEM_ADDRESS** can call whitelisted system contracts as deposit-like transactions — no signature or fee required.
  This is how the sequencer updates oracle storage.
- **Any system contract modification requires a new spec** to preserve backward compatibility.

## External Environment Dependencies

mega-evm requires external context beyond revm's standard `BlockEnv`/`CfgEnv`, provided via the `ExternalEnvFactory` trait (`external/factory.rs`):

- **SALT environment** (`external/salt.rs`): Provides bucket capacity data for dynamic gas pricing.
  Each account and storage slot maps to a SALT bucket; gas cost = base cost × (bucket_capacity / MIN_BUCKET_SIZE).
  This makes storage operations more expensive in crowded state regions, preventing state bloat.
  Implementation: `DynamicGasCost` struct (`external/gas.rs`) lazily caches bucket multipliers.
- **Oracle environment** (`external/oracle.rs`): Supplies storage values for the oracle contract via `get_oracle_storage(slot)`.
  Oracle reads in `sload` are **always forced cold** for deterministic replay.
  The `on_hint(from, topic, data)` callback enables synchronous oracle hints during execution.
- An `EmptyExternalEnv` implementation disables both features (returns minimum bucket size, no oracle data) for testing or standalone use.
