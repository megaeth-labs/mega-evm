# AGENTS.md

This file provides guidance to AI agents (e.g., claude code, codex, cursor, etc.) when working with code in this repository.

## Project Overview

MegaETH EVM (mega-evm) — a specialized EVM implementation for MegaETH, built on **revm** and **op-revm** by customizing several hooks exposed by trait of revm.

## Build & Development Commands

```bash
# Build
cargo build
cargo build --release -p mega-evme       # CLI tool

# Test
cargo test                                # all tests
cargo test -p mega-evm                    # core crate only
cargo test -p mega-evm -- test_name       # single test

# Check compiler errors (preferred over clippy for quick checks)
cargo check
cargo check -p mega-evm

# Lint (CI runs all of these)
cargo fmt --all --check
cargo clippy --workspace --lib --examples --tests --benches --all-features --locked
cargo sort --check --workspace --grouped --order package,workspace,lints,profile,bin,benches,dependencies,dev-dependencies,features

# Benchmarks
cargo bench -p mega-evm --bench transact

# no_std check (run against riscv target)
cargo check -p mega-evm --target riscv64imac-unknown-none-elf --no-default-features

# System contracts (requires Foundry)
cd crates/system-contracts && forge build
```

Git submodules are required — clone with `--recursive` or run `git submodule update --init --recursive`.

## Workspace Structure

| Crate                   | Path                      | Purpose                                                      |
| ----------------------- | ------------------------- | ------------------------------------------------------------ |
| `mega-evm`              | `crates/mega-evm`         | Core EVM implementation                                      |
| `mega-system-contracts` | `crates/system-contracts` | Solidity system contracts with Rust bindings (Foundry-based) |
| `state-test`            | `crates/state-test`       | Ethereum state test runner                                   |
| `mega-evme`             | `bin/mega-evme`           | CLI tool for EVM execution (`run`, `tx`, `replay`)           |

## Architecture

### Spec System (`MegaSpecId`)

Progression: `EQUIVALENCE` → `MINI_REX` → `MINI_REX1` → `MINI_REX2` → `REX` → `REX1` → `REX2` → `REX3`

- **Spec** defines EVM behavior (what the EVM does).
  Defined in `crates/mega-evm/src/evm/spec.rs`.
  The code base **MUST** maintain **backward-compatibility**, which means the semantics (i.e., EVM behaviors) must remain the same for existing specs.
  The only exception for this is the **unstable** spec that is under active development (if exists, must be the latest one).
  - _At present, `REX3` is the unstable spec._
    When a new spec is introduced, this line should be updated to indicate the unstable spec.
  - Specifications of each spec can be found in `./specs`, which should always be maintained to be consistent with the implementation.
- **Hardfork** (`MegaHardfork`) defines network upgrade events (when specs activate).
  Multiple hardforks can map to one spec.
  Defined in `crates/mega-evm/src/block/hardfork.rs`.
- All specs use `OpSpecId::ISTHMUS` as the Optimism base layer.
  But this is subject to change in the future.

### Core Source Layout (`crates/mega-evm/src/`)

- **`evm/`** — Core mega-evm logic: spec definitions, context, factory, execution pipeline, modified opcodes (LOG, SELFDESTRUCT), host hooks, precompiles.
  This module collects all our modifications and customizations of EVM's behavior for mega-evm based on the revm.
- **`block/`** — Block execution: executor, factory, hardfork-to-spec mapping, limit enforcement.
  This module defines how a block in MegaETH block should be executed.
- **`limit/`** — Resource limit tracking: compute gas, data size, KV updates, state growth (each in its own module).
  MegaETH introduces additional resource metering mechanism (documentation in `docs/RESOURCE_ACCOUNTING.md`) and this module implements their logic as utility structs to be used by mega-evm.
- **`access/`** — Block env access tracking and volatile data detection for parallel execution.
  MegaETH incorporates parallel EVM, so it is essential to reduce the conflicts between transactions by restricting the access to some "hot" resources.
  This module collects the logic of tracking the access to such hot resources during transaction execution.
- **`external/`** — External environmental context depended by mega-evm.
  The execution of mega-evm relies on some external environment information, in addition to revm's normal BlockEnv and CfgEnv.
- **`system/`** — System contract integration.
  MegaETH provides several system contracts that are predeployed on the chain.
  Any modification of such system contract must induce a new Spec to ensure backward compatibility.
- **`sandbox/`** — Isolated EVM execution
- **`constants.rs`** — All numeric constants organized by spec
- **`types.rs`** — Shared type definitions

### Key Concepts

#### Backward Compatibility of Specs

The spec system (`MegaSpecId`) forms a linear progression where each newer spec includes all previous behaviors.
The codebase **MUST** maintain backward-compatibility: EVM semantics must never change for existing (stable) specs.
The only exception is the latest spec if explicitly marked as **unstable**.
Consequently:

- Adding/modifying a system contract requires introducing a new spec.
- Changing gas costs, opcode behavior, or resource limits requires a new spec.
- Code should use `spec.is_enabled(MegaSpecId::X)` to gate spec-specific behavior.
- Modified opcodes (e.g., SELFDESTRUCT behavior varies by spec) are wired per-spec in the instruction table (`evm/instructions.rs`).

#### Dual Gas Model (Compute Gas vs Storage Gas)

MegaETH separates EVM gas into two independent dimensions tracked during execution:

- **Compute gas**: Measures pure computational cost.
  Every opcode's gas consumption is recorded via wrapped instructions (`compute_gas_ext` in `evm/instructions.rs`).
  Subject to a per-spec compute gas limit and further restricted by gas detention (see below).
- **Storage gas**: Charges for persistent state modifications (SSTORE, account creation, contract deployment).
  These costs scale dynamically with SALT bucket capacity (see External Environment Dependencies below).
  LOG opcodes are charged in both dimensions: standard compute gas rates plus a storage gas multiplier on topic and data costs.

Both dimensions are enforced independently.
A transaction can be halted by exceeding either limit.

#### Multidimensional Resource Limits

Beyond the dual gas model, mega-evm enforces **four independent per-transaction resource limits** via `AdditionalLimit` (`limit/mod.rs`):

- **Compute gas** — Computational opcode cost
- **Data size** — Calldata + logs + storage writes + code deploy + account updates
- **KV updates** — Storage writes + account modifications (net, with refunds)
- **State growth** — Net new accounts + net new storage slots (not all specs enable this)

Each dimension has its own per-spec limit defined in `constants.rs`.
All trackers are **frame-aware**: reverted inner calls discard their tracked usage, successful calls merge into the parent frame.
When any limit is exceeded, execution halts with `OutOfGas` and remaining gas is preserved for refund.

#### Gas Detention (Volatile Data Access Restriction)

MegaETH's parallel EVM needs to minimize conflicts between concurrent transactions.
"Volatile" data — block environment fields (NUMBER, TIMESTAMP, COINBASE, etc.), the beneficiary's account state, and the oracle contract — is frequently read by many transactions and thus a major source of conflicts.

**Gas detention** restricts computation after volatile data is accessed by capping the remaining compute gas:

- Different volatile data categories (block env/beneficiary, oracle) have different cap levels defined in `constants.rs`.
- The **most restrictive cap wins** when multiple volatile sources are accessed.
- Caps are applied via host hooks (`evm/host.rs`) that mark access in a `VolatileDataAccessTracker` (`access/tracker.rs`), then enforced after each volatile opcode via `wrap_op_detain_gas!` in `evm/instructions.rs`.

This forces transactions that touch volatile data to terminate quickly, reducing parallel execution conflicts without banning the access outright.
Detained gas is effectively refunded — users only pay for actual computation performed.

#### System Contracts

MegaETH pre-deploys system contracts at well-known addresses (`0x634200...0001`, `0002`, `0003`, etc.).
They are deployed idempotently during `pre_execution_changes()` in `block/executor.rs`, gated by hardfork activation:

| Contract                 | Address suffix | Purpose                                             |
| ------------------------ | -------------- | --------------------------------------------------- |
| Oracle                   | `...0001`      | External key-value storage with hint support        |
| High-Precision Timestamp | `...0002`      | Sub-second block timestamp                          |
| Keyless Deploy           | `...0003`      | Deterministic contract deployment via Nick's Method |

Key design aspects:

- Solidity sources in `crates/system-contracts/contracts/`, compiled by Foundry, with Rust ABI bindings generated via `alloy-sol-types`.
- Bytecode is versioned and hash-verified at build time (`crates/system-contracts/build.rs`).
- The **MEGA_SYSTEM_ADDRESS** can call whitelisted system contracts as deposit-like transactions — no signature or fee required.
  This is how the sequencer updates oracle storage.
- **Any system contract modification requires a new spec** to preserve backward compatibility.

#### External Environment Dependencies

mega-evm requires external context beyond revm's standard `BlockEnv`/`CfgEnv`, provided via the `ExternalEnvFactory` trait (`external/factory.rs`):

- **SALT environment** (`external/salt.rs`): Provides bucket capacity data for dynamic gas pricing.
  Each account and storage slot maps to a SALT bucket; gas cost = base cost × (bucket_capacity / MIN_BUCKET_SIZE).
  This makes storage operations more expensive in crowded state regions, preventing state bloat.
  Implementation: `DynamicGasCost` struct (`external/gas.rs`) lazily caches bucket multipliers.
- **Oracle environment** (`external/oracle.rs`): Supplies storage values for the oracle contract via `get_oracle_storage(slot)`.
  Oracle reads in `sload` are **always forced cold** for deterministic replay.
  The `on_hint(from, topic, data)` callback enables synchronous oracle hints during execution.
- An `EmptyExternalEnv` implementation disables both features (returns minimum bucket size, no oracle data) for testing or standalone use.

## Test Organization (`crates/mega-evm/tests/`)

Tests are organized by spec: `equivalence/`, `mini_rex/` (11 modules), `rex/`, `rex2/`, and `block_executor/`.
Each module tests specific features of that spec.

## Version Control

The main branch is `main`, but it's protected.
All change should be made via PRs on GitHub.

### Branch naming convention

The naming convension for git branches is `[DEVELOPER NAME]/[CHANGE CATEGORY]/[SHORT DESCRIPTION]`, where:

- `[DEVELOPER NAME]` is the (nick)name of the developer.
- `[CHANGE CATEGORY]` should indicate what type of modifications this PR is making, e.g., feat, fix, doc, ci, refactor, etc.
- `[SHORT DESCRIPTION]` is a short (a few words) description of the detailed changes in this branch.

## Workflows

### Committing changes

When requested to commit changes, the agent should first review the current all changes in the working tree, regardless of whether they are staged or not.
There may be other changes in the worktree in addition to those made by the agent, which may also need to be included.
If the agent is not sure whether some changes should be included in the commit, ask the user.
The commit message should reflect the overall changes of the commit, which may beyond the existing context of the agent.

The commit message should be short and exclude any information of the agent itself.

### Creating PR

When a PR creation is requested, the agent should:

1. Check if the repo is current on a different branch other than `main`.
   If not, create and checkout to a new branch.
   Make sure to inform the user about this branch creation.
2. Commit the changes in the worktree before fix linting issues.
3. Run lint check, and fix any lint warnings, and then commit if there are any changes.
4. Format the code and commit if there are any changes.
5. Push to the remote.
6. Use `gh` CLI tool to create a PR.
   When generating the PR title and description, consider the overall changes in this branch across commits.
   In the PR description, make sure a `Summary` section is put on the top.
   The PR will be merged with `Squash and Merge` operation, whose commit description should include the summary.

### Implementing features or bug fixes

When the agent is requested to implement a new feature or bug fix, it should consider the following additional aspects in addition to the feature/fix itself and the other requirements by the user.

1. Should the documentation need to be updated (or added)?
2. Is there sufficient tests for this feature?

## Caveats for Agents

- **Always test logic changes.**
  Any logic change or modification to mega-evm should be equipped with tests if there is no specific reason of not adding tests.
  The agent should always consider accompanying tests or suggest to add additional tests.
- **Do NOT modify behavior for existing stable specs.**
  All specs in `MegaSpecId` are currently stable (frozen).
  New EVM behavior, gas cost changes, or opcode modifications **must** introduce a new spec and be gated with `spec.is_enabled(MegaSpecId::NEW_SPEC)`.
  Never change what an existing spec does.
- **System contract changes require a new spec.**
  Do not modify system contract Solidity sources or their Rust integration without also introducing a new spec for backward compatibility.
- **Respect `no_std` in `mega-evm` crate.**
  Do not use `std::` directly.
  Follow the existing pattern: `#[cfg(not(feature = "std"))] use alloc as std;` then `use std::{vec::Vec, ...};`.
  Use `core::` for items like `fmt`, `cell`, `convert`.
- **`cargo sort` is enforced in CI.**
  Dependencies in `Cargo.toml` must follow the grouped-by-family convention with comment headers (`# alloy`, `# revm`, `# megaeth`, `# misc`) and be sorted alphabetically within each group.
- **Use `default-features = false` for new workspace dependencies.**
  This is the standard convention — features are opted-in explicitly.
- **Use `cargo check` (not `cargo clippy`) for compiler error checking.**
  Use `cargo clippy` only when specifically checking lint warnings.
- **Keep documentation up to date.**
  When making changes, always check whether related documentation needs updating.
  This includes spec files in `specs/`, docs in `docs/`, and the `CLAUDE.md` itself (e.g., unstable spec marker, spec progression list, system contract table).
- **One sentence, one line.**
  When writing markdown or similar format files, put each sentence in a separate line.
