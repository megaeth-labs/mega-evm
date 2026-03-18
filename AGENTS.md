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

Progression: `EQUIVALENCE` → `MINI_REX` → `MINI_REX1` → `MINI_REX2` → `REX` → `REX1` → `REX2` → `REX3` → `REX4`

- **Spec** defines EVM behavior (what the EVM does).
  Defined in `crates/mega-evm/src/evm/spec.rs`.
  The codebase **MUST** maintain **backward-compatibility**: semantics must remain the same for existing specs.
  The only exception is the **unstable** spec under active development (if exists, must be the latest one).
  - _At present, `REX4` is the unstable spec._
  - Specifications of each spec can be found in `./specs`.
- **Hardfork** (`MegaHardfork`) defines network upgrade events (when specs activate).
  Defined in `crates/mega-evm/src/block/hardfork.rs`.
- All specs use `OpSpecId::ISTHMUS` as the Optimism base layer.

### Core Source Layout (`crates/mega-evm/src/`)

- **`evm/`** — Core mega-evm logic: spec, context, factory, execution pipeline, modified opcodes, host hooks, precompiles.
- **`block/`** — Block execution: executor, factory, hardfork-to-spec mapping, limit enforcement.
- **`limit/`** — Resource limit tracking: compute gas, data size, KV updates, state growth.
- **`access/`** — Block env access tracking and volatile data detection for parallel execution.
- **`external/`** — External environmental context (SALT pricing, oracle).
- **`system/`** — System contract integration.
- **`sandbox/`** — Isolated EVM execution.
- **`constants.rs`** — All numeric constants organized by spec.
- **`types.rs`** — Shared type definitions.

For detailed architecture concepts (dual gas model, resource limits, gas detention, system contracts, external environment), see `.claude/rules/architecture.md`.

### Test Organization

Tests are organized by spec: `crates/mega-evm/tests/` with `equivalence/`, `mini_rex/`, `rex/`, `rex2/`, and `block_executor/`.

## Version Control

The main branch is `main`, but it's protected.
All changes should be made via PRs on GitHub.

Branch naming: `[DEVELOPER NAME]/[CHANGE CATEGORY]/[SHORT DESCRIPTION]` (e.g., `cz/feat/new-opcode`).

## Workflows

### Committing changes

Review all changes in the working tree (staged and unstaged).
There may be other changes beyond those made by the agent — ask if unsure whether to include them.
The commit message should be short and exclude agent information.

### Creating PR

1. Create and checkout a new branch if not already on one (not `main`).
2. Commit changes, then run lint check and fix warnings, then format — commit each step.
3. Push to remote and create PR via `gh` CLI.
   PR description must have a `Summary` section on top (will be used as squash-merge commit description).

### Implementing features or bug fixes

In addition to the feature/fix itself:
1. Update documentation if needed.
2. Add sufficient tests.

## NEVER

- Modify behavior for existing stable specs — new behavior **must** introduce a new spec gated with `spec.is_enabled(MegaSpecId::NEW_SPEC)`.
- Modify system contract Solidity or Rust integration without introducing a new spec.
- Intercept unknown selectors for system contracts — they must fall through to on-chain bytecode.
- Use `std::` directly in `mega-evm` — follow the `no_std` pattern (see `.claude/rules/rust.md`).
- Skip lint and format checks before finishing a change.

## ALWAYS

- Test logic changes — any EVM behavior modification must have accompanying tests.
- Define value-transfer policy for system contract interceptors (reject non-zero `transfer_value` for read-only/control methods).
- Cover system contract interceptor boundary behaviors in tests (normal path, non-zero value, unknown selector, CALL vs DELEGATECALL/CALLCODE).
- Keep documentation up to date (specs in `specs/`, docs in `docs/`, and this file).
- Run `cargo clippy --workspace --lib --examples --tests --benches --all-features --locked` and `cargo fmt --all --check` before completion.

## Verification

For any logic change:
- `cargo check -p mega-evm` passes.
- `cargo test -p mega-evm` passes.
- `cargo clippy --workspace --lib --examples --tests --benches --all-features --locked` clean.
- `cargo fmt --all --check` clean.

For system contract changes, additionally:
- `cd crates/system-contracts && forge build` passes.
- Interceptor tests cover normal, non-zero value, unknown selector, and call-type boundaries.

## Compact Instructions

When compressing context, preserve in priority order:

1. Architecture decisions and spec compatibility constraints (NEVER summarize away).
2. Modified files and their key changes.
3. Current verification status (which checks pass/fail).
4. Open TODOs and rollback notes.
5. Tool outputs can be discarded — keep only pass/fail status.
