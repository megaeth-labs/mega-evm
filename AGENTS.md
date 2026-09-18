# AGENTS.md

This file provides guidance to AI agents (e.g., claude code, codex, cursor, etc.) when working with code in this repository.

## Project Overview

MegaEVM (mega-evm) — a specialized EVM implementation for MegaETH, built on **revm** and **op-revm** by customizing the hooks revm exposes through its traits.

This branch (`satin`) builds **Satin**, the new engine: a single spec on the MegaETH fork of revm 40.0.3 and the MegaETH fork of op-revm 20.0.0.
Today's `main` is the legacy engine; Satin is the only active spec.
The legacy engine (specs `Equivalence` through `Rex7`, the 1.x crate line on crates.io revm 27) is frozen and is not edited here.

Satin is being built mechanism by mechanism from an empty skeleton.
The module table below says what exists and which mechanisms fill the rest.

## Build & Development Commands

```bash
# Build
cargo build

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

# Benchmarks: `transact` (MegaEvm next to op-revm), `corpus` (a slice of the differential corpus), `factory` (EVM construction)
cargo bench -p mega-evm --bench <target>                                  # wall-clock + HTML report
cargo codspeed build -p mega-evm --bench <target> && cargo codspeed run   # instruction counts (Linux only)

# Differential harness: MegaEvm against stock revm 43 over the scenario corpus
cargo test -p mega-differential --locked

# no_std check (run against riscv target)
cargo check -p mega-evm --target riscv64imac-unknown-none-elf --no-default-features

# System contracts (requires Foundry)
cd crates/system-contracts && forge build
```

Git submodules are required — clone with `--recursive` or run `git submodule update --init --recursive`.

## Workspace Structure

| Crate                   | Path                       | Member           | Purpose                                                      |
| ----------------------- | -------------------------- | ---------------- | ------------------------------------------------------------ |
| `mega-evm`              | `crates/mega-evm`          | yes              | The Satin engine                                             |
| `mega-system-contracts` | `crates/system-contracts`  | yes              | Solidity system contracts with Rust bindings (Foundry-based) |
| `mega-differential`     | `crates/mega-differential` | yes, not default | Differential harness: `MegaEvm` against stock revm 43        |
| `mega-state-test`       | `crates/mega-state-test`   | no               | State-test runner library; rejoins when ported to Satin      |
| `state-test`            | `crates/state-test`        | no               | State-test CLI; rejoins when ported to Satin                 |
| `mega-evme`             | `bin/mega-evme`            | no               | EVM execution CLI; rejoins when ported to Satin              |
| `mega-t8n`              | `bin/mega-t8n`             | no               | State transition (t8n) tool; rejoins when ported to Satin    |

`mega-differential` builds stock revm 43 from crates.io next to the fork, as its oracle; it is not a default member, so the commands run at the root without `-p` or `--workspace` stay on the engine's graph.
The four tool crates still target the legacy engine.
They are outside `[workspace] members`, so no workspace command builds them; do not edit their sources until they are ported to Satin.

### Dependencies on the forks

The root `Cargo.toml` pins `revm = "=40.0.3"` and redirects all twelve revm crates to the MegaETH fork with `[patch.crates-io]`.
`op-revm` is declared from the OP monorepo revision the node locks and redirected to the MegaETH fork of op-revm with `[patch."https://github.com/ethereum-optimism/optimism"]`; the OP alloy crates (`alloy-op-evm`, `op-alloy-*`) come from the same monorepo revision.

- Patch the twelve revm crates together, or the build resolves a second copy of revm.
- `cargo tree -i revm -p mega-evm` must show exactly one revm, from the fork; only `mega-differential`'s graph adds stock revm 43, as its oracle.
- The fork pins move by editing the patch blocks; commit the regenerated `Cargo.lock` with them.

## Architecture

### Spec System (`MegaSpecId`)

`MegaSpecId` has a single rung, `SATIN`, running on `OpSpecId::KARST` (Ethereum `OSAKA`).
`MegaHardfork` has the single fork `Satin`, which activates `SATIN`.

- `SATIN` is the unstable spec under active development: it has no activation timestamp yet (`block/chain.rs` lists the two chains with the Satin timestamp unset), and its behavior may change until it is sealed.
- Display and `FromStr` use `"Satin"`; serde uses the variant name `"SATIN"`.
- The legacy spec names (`Equivalence` … `Rex7`) parse to `ParseMegaSpecError::Legacy`; they are never mapped to Satin.
- There are no spec gates: a single-spec engine has nothing to gate.
  The spec that follows Satin introduces the first `is_enabled`-style gate.

### Core Source Layout (`crates/mega-evm/src/`)

| Module         | Holds now                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | Filled by                                                                                                                                                                                                                                                                          |
| -------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `evm/`         | `MegaSpecId`; `MegaContext`; `MegaEvm` over revm's `Evm` with its own frame lifecycle and `MegaHandler` over op-revm's handler (`execution.rs`); the staging Host and the non-warming journal reads (`host.rs`); the `SSTORE`/`LOG`/`SELFDESTRUCT` commit wrappers (`instructions.rs`); synthetic frame results (`frame.rs`); `TrustedObserver`, `DeclaredObserver` and the creation-revival refusal (`inspector.rs`); `MegaTransactionOutcome`, `MegaGasUsage` (`result.rs`); `MegaEvmFactory` (alloy-evm) | the Satin gas table, SALT pricing, history gas, compute gas; revert-class policy aborts, the system-call reservoir split; EIP-7708 and SLOTNUM; the system contract interceptors (`MegaEvm::intercept`), native keyless deployment (`MegaEvm::rewrite_keyless`), inspector support |
| `block/`       | `MegaHardfork`, the chain activation table, a `MegaBlockExecutor` skeleton, `BlockGasCounters` (`result.rs`)                                                                                                                                                                                                                                                                                                                                                                                                | the block executor (Karst block rules, filling `BlockGasCounters`, refusing `has_rewriting_inspector`), the pre-block system calls, the system contract deployments                                                                                                                |
| `external/`    | `ExternalEnvFactory`, `ExternalEnvs`, `SaltEnv`, `OracleEnv`, `EmptyExternalEnv`, the bucket hasher, `TestExternalEnvs`                                                                                                                                                                                                                                                                                                                                                                                     | SALT pricing, the oracle                                                                                                                                                                                                                                                           |
| `limit/`       | `LimitKind`, `LimitCheck`, `MegaLimitExceeded`, `LimitUsage`, `StagedRecord`; `AdditionalLimit`: the per-frame lanes of data-size bytes and write records (`frame_limit.rs`), staging, the latch; `EvmTxRuntimeLimits` (a data-size cap and a flat frame budget, both unlimited by default)                                                                                                                                                                                                                 | the data-size limit (its limit and per-frame budget rule, the transaction-body and code-deposit bytes), detention, the state-growth and KV limits                                                                                                                                  |
| `access/`      | nothing                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | volatile-data access tracking (detention)                                                                                                                                                                                                                                          |
| `system/`      | `keyless`: the Nick's Method transaction format, validation helpers and error ABI (data only)                                                                                                                                                                                                                                                                                                                                                                                                               | the system contract interceptors, deployment and oracle and control contracts; keyless deployment as a native CREATE sub-frame                                                                                                                                                     |
| `constants.rs` | the provisional numbers (CPSB, slot and account state gas, CPHB, execution cap, data-size limits)                                                                                                                                                                                                                                                                                                                                                                                                           | read by the gas and limit mechanisms; the numbers are provisional until the economics sign-off                                                                                                                                                                                     |
| `types.rs`     | transaction, halt reason, error and envelope aliases, kept because Satin adds nothing to them (its halt set is op-revm's)                                                                                                                                                                                                                                                                                                                                                                                   | —                                                                                                                                                                                                                                                                                  |
| `test_utils/`  | `MemoryDatabase`, `ErrorInjectingDatabase`, `BytecodeBuilder`, `GasInspector`, `transact`, the JSON `Scenario` format                                                                                                                                                                                                                                                                                                                                                                                       | `BytecodeBuilder` is extended with EIP-7708 and SLOTNUM                                                                                                                                                                                                                            |

### How Satin executes today

- `MegaContext` wraps op-revm's context shape (`MegaTransaction` = alloy-op-evm's `OpTx`, `L1BlockInfo`) and adds the spec and the external environments.
  It keeps the configuration twice — the `MegaSpecId` view callers see and the `OpSpecId` view op-revm executes on — and writes both together.
- The spec fixes part of the configuration, whatever the caller passes (`MegaContext::with_cfg`): the Osaka gas table, EIP-8037 on, the EIP-2780 intrinsic cost on, `tx_gas_limit_cap` = 200,000,000, EIP-7708 off, the system-call reservoir margin off.
  Gas above the 200M execution cap goes to the EIP-8037 reservoir.
  The Osaka gas table prices state gas at zero, so no transaction draws state gas until the Satin gas table is installed.
- `MegaEvm` runs transactions through `MegaHandler`, which wraps op-revm's handler, and implements revm's frame lifecycle itself; every `Host` and context method delegates to op-revm's context except the three that stage what a state-writing opcode did.
  With no limit configured Satin still equals op-revm: `tests/satin/equivalence.rs` pins it on the same `CfgEnv`, field by field, and the differential harness against revm 43; a later change that alters behavior on purpose updates that baseline.
- The common execution layer counts data-size bytes and write records per frame and enforces nothing by default; `EvmTxRuntimeLimits` sets a data-size cap and a frame budget to drive the abort protocol.
- The legacy engine's gas leakage pitfalls, limit-check protocol and storage-gas stipend describe mechanisms that do not exist here; the contracts below replace them.

### Contracts of the common execution layer

Every later mechanism plugs into these; a change to one comes back to this layer.

- **The Host only observes.**
  `sstore`, `log` and `selfdestruct` stage the facts revm hands them (`StagedRecord`) and record nothing.
  Recording in the Host would count a write the opcode's own failure takes back (an out-of-gas after the Host call), and a limit crossed there would stop the transaction for a write that never happened.
- **Commit after the opcode.**
  The wrappers of `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT` discard a stale record on entry, run revm's instruction, then commit the staged record if the opcode completed and discard it if it failed.
  Every other opcode runs revm's instruction unwrapped, and the static gas table is revm's.
- **Write records.**
  One 40-byte record per account or storage write: a slot's first change in the transaction (taken back on write-back to the original value), a value transfer's sender and recipient, a creation's creator nonce and created account, a `SELFDESTRUCT` moving value to another account, an applied EIP-7702 authority, the transaction's value recipient or created account.
  The sender's own account is part of the transaction body.
  Records are deduplicated per frame (a frame's account is recorded once) and live on the frame's lane: a success merges the lane into the caller's, a failure discards it; a creator's nonce record survives the creation's failure once the nonce was bumped.
  The KV count a node reports is the write-record count.
- **Lanes stay aligned with frames.**
  `frame_init` pushes one lane per frame result revm will return, an empty one for a frame answered without running (the latch, the depth guard, an interceptor, an inspector); `frame_return_result` pops it, and `last_frame_result` pops the outermost one when it never ran.
- **The latch.**
  A transaction-level limit stops the transaction with a revert, never a halt: the frame that crosses it reverts with `MegaLimitExceeded(uint8 kind, uint64 limit)`, the transaction is latched (`AdditionalLimit::latch`), no caller resumes (`before_frame_run`), no frame starts, every result returned above is rewritten to the stop, and the outermost frame settles like an EIP-8037 revert, its unspent regular gas and the reservoir back to the sender.
  A frame budget reverts its frame alone, without a latch.
  A real out-of-gas, a precompile out-of-gas and an invalid opcode still halt and burn.
  The outcome's `limit_exceeded`, not the output, tells a stop from a contract reverting with the same bytes.
- **Synthetic frame results carry the caller's pools.**
  A result built without running a frame is a `synthetic_frame_result`: gas untouched with the inherited reservoir (`untouched_call_gas`, `with_pools_of`), never `Gas::new(limit)`, and the calling opcode's upfront state-gas flags, so it settles exactly like revm's own (`settle_frame_result`).
- **The admission gate.**
  A rewriting inspector is a tool feature; `with_trusted_inspector` requires a `TrustedObserver` declaration, `has_rewriting_inspector` is what block execution refuses, and `DeclaredObserver` carries the declaration for a foreign tracer and proves it in debug builds.
  The one rewrite refused is a failed creation turned into a success (`FORBIDDEN_CREATE_REVIVAL`, an `EVMError::Custom`).
  The test utilities' inspectors are not declared.
- **Three ledgers.**
  `MegaGasUsage` splits a transaction's raw spend into regular (compute), state and history gas, all paid from the EIP-8037 pools, reservoir first; state or history gas that spilled onto regular gas stays on its own ledger.
  `gas_used` is the receipt's figure; `BlockGasCounters` sums the three per block.

## Test Organization (`crates/mega-evm/tests/`)

- `satin/` — tests of the Satin engine (integration tests; add new ones here or in a new directory with a `main.rs`).
- `_pending/` — the legacy tests the test inventory keeps, parked until the mechanism they test lands.
  It has no `main.rs`, so Cargo does not build it; its `README.md` names the mechanism that owns every file.
  The change that ports a pending test deletes it from `_pending/` in the same commit.
- Unit tests live next to the code in `#[cfg(test)] mod tests`.
- `crates/mega-differential/scenarios/` — the differential corpus: JSON scenarios (`test_utils::Scenario`) that run through `MegaEvm` and through stock revm 43 and are compared field by field.
  A behavior that should equal Ethereum's is pinned best as a scenario there.

## Version Control

`satin` is the integration branch of the new engine; pull requests for Satin target `satin`.
`main` carries the legacy engine and is not touched from Satin work.
Both are protected: all changes go through PRs on GitHub.

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

1. Check if the repo is current on a different branch other than `satin` or `main`.
   If not, create and checkout to a new branch.
   Make sure to inform the user about this branch creation.
2. Commit the changes in the worktree before fix linting issues.
3. Run lint check, and fix any lint warnings, and then commit if there are any changes.
4. Format the code and commit if there are any changes.
5. Push to the remote.
6. Use `gh` CLI tool to create a PR against `satin`.
   When generating the PR title and description, consider the overall changes in this branch across commits.
   In the PR description, make sure a `Summary` section is put on the top.
   The PR will be merged with `Squash and Merge` operation, whose commit description should include the summary.

### Implementing features or bug fixes

When the agent is requested to implement a new feature or bug fix, it should consider the following additional aspects in addition to the feature/fix itself and the other requirements by the user.

1. Should the documentation need to be updated (or added)?
2. Is there sufficient tests for this feature?

## Caveats for Agents

- **Satin is the only active spec; the legacy engine is frozen.**
  Do not add legacy specs, alias rungs or spec gates to this crate, and do not port a legacy mechanism the legacy test inventory retired.
  Behavior of the legacy specs is changed only on the legacy line, never here.
- **Build from the skeleton, one mechanism at a time.**
  Each module is filled by the mechanisms named in the module table; do not implement another mechanism to make yours work.
  If a change needs an interface that belongs to another mechanism, stub the smallest interface and name the mechanism that owns it.
- **Always test logic changes.**
  Any logic change or modification to mega-evm should be equipped with tests if there is no specific reason of not adding tests.
  The agent should always consider accompanying tests or suggest to add additional tests.
- **Keep the op-revm baseline honest.**
  A change that makes Satin differ from op-revm on purpose must update `tests/satin/equivalence.rs` (or add a case) so the difference is pinned, not silently absorbed.
- **Keep the differential harness clean.**
  `cargo test -p mega-differential --locked` must pass: every difference between `MegaEvm` and stock revm 43 is explained by an effect of `crates/mega-differential/deviations.json`, and every effect there explains one.
  A change that makes `MegaEvm` differ on purpose registers each effect with its mechanism and reason, as narrow as the mechanism allows (or teaches the oracle to model it); a change that removes a difference removes its effect.
- **Add benchmarks for performance-sensitive changes.**
  Changes on the EVM execution hot path must be accompanied by benchmarks.
  This includes new or modified opcode behavior, gas mechanics, system contract interception, resource limit tracking, and block executor pipeline changes.
  Per-PR instruction-count reports for these benchmarks are produced automatically by the CodSpeed CI workflow.
- **Always run benchmarks locally before committing.**
  New or modified benchmarks must be executed locally (`cargo bench -p mega-evm --bench <name>`) to verify they pass before committing.
  Benchmarks may compile but panic at runtime due to missing setup (e.g., required block fields), so compilation alone is not sufficient.
  For instruction-count deltas across a PR, use the CodSpeed report posted on the PR rather than local wall-clock numbers.
- **Use `test_` prefix for Rust test function names.**
  New `#[test]` functions should be named with a `test_` prefix for consistency with this repository and upstream revm style.
  If editing nearby tests in the same module, align names to the same `test_` style when reasonable.
- **System contracts carry over as they are.**
  Satin reuses the Solidity sources and bindings in `crates/system-contracts`; changing a contract needs a decision of its own, not a side effect of an engine change.
- **Rules for system contract interceptors (once the interceptors land).**
  For read-only or control methods, reject calls with non-zero `transfer_value` in the interceptor; if a method intentionally accepts value, document the reason in spec and code comments and add dedicated tests.
  Do not intercept unknown selectors: they fall through to on-chain bytecode and revert with a stable custom error such as `NotIntercepted()`.
  Only `CALL` and `STATICCALL` reach interceptor dispatch; `CALLCODE` and `DELEGATECALL` are rejected by the call-scheme guard before any interceptor is consulted.
  Interceptor tests cover the intercepted path, non-zero value, unknown selector fallback, and CALL vs DELEGATECALL/CALLCODE boundaries.
- **Validate per-fork parameters at load time (once the block executor lands).**
  A hardfork parameters type overrides `HardforkParams::validate()` with field-level invariant checks, so a bad chain config fails when it is loaded rather than at the fork's first block.
  The fork-requires-params rule is registered in the schedule validation too, so a schedule that activates the fork without its params is rejected at load time.
- **Pre-block helpers must return state, not commit directly (pre-block system calls, system contract deployment).**
  Any helper participating in pre-block execution (system contract deploys, pre-block system calls, etc.) returns `Option<EvmState>` and never calls `db.commit(...)` directly, so the witness generator sees the complete read and write set.
- **Respect `no_std` in `mega-evm` crate.**
  Do not use `std::` directly.
  Follow the existing pattern: `#[cfg(not(feature = "std"))] use alloc as std;` then `use std::{vec::Vec, ...};`.
  Use `core::` for items like `fmt`, `cell`, `convert`.
- **All execution logic must be deterministic and architecture-independent.**
  Code that affects EVM execution results, gas computation, state transitions, or consensus-critical hashing must produce identical output regardless of target architecture, endianness, or pointer width.
  Never use `mem::transmute`, native-endian byte conversions, or platform-dependent operations in consensus paths.
  Use explicit little-endian (`from_le_bytes`/`to_le_bytes`) or big-endian conversions instead.
  When vendoring external code, audit for hidden platform dependencies (e.g., `zerocopy::transmute!` is native-endian).
- **`cargo sort` is enforced in CI.**
  Dependencies in `Cargo.toml` must follow the grouped-by-family convention with comment headers (`# alloy`, `# revm`, `# megaeth`, `# misc`) and be sorted alphabetically within each group.
- **Use `default-features = false` for new workspace dependencies.**
  This is the standard convention — features are opted-in explicitly.
- **Use `cargo check` (not `cargo clippy`) for compiler error checking.**
  Use `cargo clippy` only when specifically checking lint warnings.
- **Before finishing a change, always run full lint and format checks.**
  Run `cargo clippy --workspace --lib --examples --tests --benches --all-features --locked` before completion.
  Run `cargo fmt --all --check` before completion.
- **Keep documentation up to date.**
  When making changes, always check whether related documentation needs updating.
  The primary documentation is in `docs/`; it still describes the legacy engine until the Satin specification pages are written.
  The spec documentation is under `docs/spec/`, and the mega-evme documentation is under `docs/mega-evme/`.
  Also update this `AGENTS.md` when relevant (e.g., the module table, the unstable-spec marker).
- **One sentence, one line.**
  When writing markdown or similar format files, put each sentence in a separate line.
- **Run Prettier on docs before committing.**
  `docs/` markdown files are checked by Prettier in CI (`prettier --check 'docs/**/*.md'`).
  After editing any `docs/` file, run `npx prettier --write 'docs/**/*.md'` to fix formatting.

## Documentation Conventions (`docs/`)

The `docs/` directory is organized into two GitBook sections:

- **`docs/spec/`** — The public-facing specification for the MegaETH blockchain's execution layer — covering MegaEVM, system contracts, oracle services, resource metering, and the upgrade history.
  It is framed as a protocol specification, not as documentation for a specific crate.
- **`docs/mega-evme/`** — Documentation for the `mega-evme` CLI tool.

All conventions for writing and editing the spec documentation (audience, content rules, upgrade page format, writing style) are defined in [`docs/spec/AGENTS.md`](docs/spec/AGENTS.md).
