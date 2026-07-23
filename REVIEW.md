# Code Review Guidelines

The automated `pr-review` workflow (the centralized Claude review action) already applies a baseline review rubric: the review mindset, the priority order (data structures → design → correctness → complexity → breakage → tests → naming → style), PR title/description checks, generic correctness/test/observability checks, the "what not to flag" list, reviewer anti-patterns, the `[Critical]`/`[Major]`/`[Minor]`/`[Nit]` severity scale, and previous-thread triage.

This file supplements that baseline with the rules that are **specific to mega-evm** or stricter than it.
Defer to the baseline for everything not listed here; the rules below take precedence wherever they conflict.

## Correctness and safety

### Spec backward compatibility

This is the single most important correctness concern in mega-evm.

- **Existing stable specs must never change behavior.**
  Check `CLAUDE.md` for which spec is currently unstable — all others are frozen.
  New EVM behavior, gas cost changes, or opcode modifications must introduce a new spec and be gated with `spec.is_enabled(MegaSpecId::NEW_SPEC)`.
- System contract changes (Solidity sources or Rust integration) require a new spec.
- Modified constants must be gated per-spec — verify that old spec paths still use the old values.
- If a PR claims to "fix" behavior for an existing spec, scrutinize whether this changes consensus.
  A true bug fix in a frozen spec is rare and must be justified.

### Consensus-critical execution

- All execution logic must be **deterministic and architecture-independent** — no `mem::transmute`, no native-endian byte conversions, no platform-dependent operations in consensus paths.
- Pre-block helpers (system contract deploys, pre-exec system calls, etc.) must return `Option<EvmState>` for the block executor to commit — never call `db.commit(...)` directly.
  Even idempotent "no change" paths must return `Some(EvmState)` with a read-only account entry; silently returning `None` drops the account from the stateless witness read set and produces an incomplete proof.
  See `crates/mega-evm/src/system/AGENTS.md` → `PRE-BLOCK STATE CHANGE CONTRACT`.
- Per-frame gas mechanisms (stipends, adjustments) must handle all frame termination paths: system contract interception, gas rescue on limit exceed, and frame return.
  Missing any path causes gas leakage.

## Design and architecture

- Respect revm's design patterns — mega-evm customizes revm through its trait hooks, not by replacing its abstractions.
- `no_std` compatibility must be maintained in the `mega-evm` crate — no direct `std::` usage.
  Follow the existing pattern: `#[cfg(not(feature = "std"))] use alloc as std;`.
- New workspace dependencies should use `default-features = false` — features are opted-in explicitly.
- **Release-blocking dependencies.**
  A crate that gets published must not depend on an unpublished crate.
  `cargo publish -p <crate> --locked` has to resolve at release time.
  Flag a new dependency that would break the next release's resolution.

## Tests

- Test names must use the `test_` prefix and state the object under test (function, struct, or behavior).
- If a change affects cross-component behavior that cannot be covered by unit tests, suggest e2e tests in the review comment (these may live in the `test-client` repo).
- For stateful systems (resource-limit trackers, gas-stipend lifecycle), assert state-machine invariants after each transition, not just at the end.
- Tests under `crates/mega-evm/tests/mutation/` are generated mutation-killing system tests, each keyed to a specific surviving mutant.
  Their comments — especially the `file:line:col` mutation-location references — must be kept up to date when the referenced source moves.
  Flag any PR that shifts lines in a mutated source file but leaves a now-stale location reference (or an orphaned/renamed test) in `tests/mutation/`; a reference that no longer points at its mutant defeats the purpose of the linkage.
  These tests should not be hand-edited otherwise — see `crates/mega-evm/tests/mutation/main.rs`.
- **Benchmark methodology.**
  A perf-comparison PR must pin comparable hardforks on both arms.
  A hard-coded `OpSpecId` that ignores the enum default silently benches the wrong fork.
  Use an even round count.
  Odd defeats A/B alternation.
  Adjudicate quantitatively.
  Don't drop a `saturating_sub`/guard "for speed" without a measured, attributable gain.
- **Storage-layout parity.**
  Where Rust mirrors a Solidity storage layout, add a parity test against that layout.
  A field reordering should fail a test instead of drifting silently into a consensus bug.

## mega-evm tooling and scope

- Never flag what CI already enforces: `cargo fmt`, `cargo clippy`, and `cargo sort` (dependency ordering).
- When assessing breakage, name the affected downstream consumers explicitly: `mega-reth`, `test-client`.
