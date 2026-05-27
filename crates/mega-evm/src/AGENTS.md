# AGENTS.md

## OVERVIEW
Core MegaEVM crate implementation layer that composes execution, block processing, limits, system contracts, and external environments.

## STRUCTURE
- `lib.rs`: crate module wiring, public re-exports, type aliases.
- `evm/`: transaction execution engine, instruction table, host hooks, context and factory.
- `block/`: block executor, hardfork mapping, block-level limit pipeline.
- `limit/`: per-tx and per-frame resource trackers plus exceed handling.
- `system/`: system contract addresses, deployment helpers, frame interceptors.
- `access/`: volatile-data access bitmaps and disable/enable depth tracking.
- `external/`: SALT and oracle external environment contracts and factories.
- `sandbox/`: isolated execution paths used by special flows.
- `constants.rs`: per-spec limits and gas constants used across modules.

## KEY PATTERNS
- `no_std` discipline is active for this crate.
- Use `#[cfg(not(feature = "std"))] use alloc as std;` when std collections are required.
- Spec progression is additive.
- Keep behavior gates explicit via `spec.is_enabled(...)` at call sites.
- Per-frame trackers must stay stack-aligned with EVM frame lifecycle hooks.
- Intercepted synthetic frame results must keep tracker alignment via empty-frame pushes.
- Keep aliases in `lib.rs` aligned with canonical `Mega*` types.

## WHERE TO LOOK
- Add or modify opcode semantics: `evm/instructions.rs` and `evm/host.rs`.
- Add transaction-level runtime constraints: `limit/limit.rs` and tracker submodules.
- Add block inclusion or accounting rules: `block/limit.rs` and `block/executor.rs`.
- Add/adjust hardfork activation logic: `block/hardfork.rs`.
- Add system contract deployment or interception behavior: `system/*.rs` plus block pre-execution deployment calls.
- Add external dependency plumbing: `external/factory.rs`, `external/gas.rs`, `external/oracle.rs`, `external/salt.rs`.
- Add reusable constants: `constants.rs`.
