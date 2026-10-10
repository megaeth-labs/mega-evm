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
  The same applies inside execution: read through the journal (`inspect_account`), never `journal.database.basic(...)`, or the account driving the result is missing from the returned state — and an early halt placed before the sandbox state merge drops that read set entirely.
- Per-frame gas mechanisms (stipends, adjustments) must handle all frame termination paths: system contract interception, gas rescue on limit exceed, and frame return.
  Missing any path causes gas leakage.
- **Metering must be symmetric across every exit path of the instruction it wraps, and must not double-count.**
  A `record_*` / `compute_gas!` sitting on the success arm, or after the inner revm instruction returns, is skipped by stack underflow, OOG, decode failure and inner-call failure; conversely, charge per applied action rather than per candidate, and dedup across every frame shape (top-level, `caller == target` self-transfer, repeated recipients).
- **Ordering: guards before work, enforcement before mutation.**
  `inspect_account` / `resize_memory!` / `keccak256` / storage-gas computation belong after the wrapped instruction's own validity checks (salt underflow, EIP-3860 size, EIP-214 static, empty initcode), and a limit must be enforced before the state change it guards — a latched flag surfacing as a `Halt` after `pre_execution` journaled the writes leaves them committed under a failed receipt.
- **Keyless / sandbox child execution must reconcile every effect dimension into the parent and block:** gas, each limit dimension, volatile access, logs and the read set.
  A `SandboxOutcome` carrying state but dropping logs, or a tracker allocated fresh instead of merged, is a consensus bug; and a value validated before execution (a gas-limit override) must not be mutated afterwards.
- **The volatile-access / beneficiary-detention boundary must cover every sensitive surface** — a new volatile-derived read or side effect (Oracle SLOAD or hint, SELFDESTRUCT beneficiary, EIP-7702 delegate) has to consult the tracker; resolve the delegate before marking, and check the destructing contract itself, not only the stack target.
- **System calls and system-address transactions:** a mandatory pre-block system call with dynamic cost needs a `block.gas_limit`-derived budget, not revm's fixed default, or a spec flipping it fail-closed rejects blocks persistently; a system-address transaction must validate nonce, chain-id and EIP-3607 against replay, and a commit step re-checking block limits must re-check the nonce too.
- **Hot-path accounting must stay O(1)** in attacker-controlled call depth and authorization count — no frame-stack walk per opcode, no unindexed `Vec` scan per authorization.

## Design and architecture

- Structural and placement conventions live in [`CONVENTIONS.md`](CONVENTIONS.md) (module layout, file roles, composition idioms, when to add a new file vs. a spec-gated branch, and the smell list). Flag changes that violate them; for repo-level reviews, mine its "Known deviations & improvement backlog" (§8) for cleanup candidates.
- Respect revm's design patterns — mega-evm customizes revm through its trait hooks, not by replacing its abstractions.
- `no_std` compatibility must be maintained in the `mega-evm` crate — no direct `std::` usage.
  Follow the existing pattern: `#[cfg(not(feature = "std"))] use alloc as std;`.
- New workspace dependencies should use `default-features = false` — features are opted-in explicitly.
- **Release-blocking dependencies.**
  A crate that gets published must not depend on an unpublished crate.
  `cargo publish -p <crate> --locked --dry-run` has to resolve at release time.
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
  Missing the required explicit hardfork pin can silently bench the wrong fork when a shared bench subject defaults to a different one.
  For the paired Criterion-style harness, use an even round count.
  Odd defeats A/B alternation there.
  Do not apply that even-round rule to the replay ABBA harness.
  Adjudicate quantitatively.
  Don't drop a `saturating_sub`/guard "for speed" without a measured, attributable gain.
- **Storage-layout parity.**
  Where Rust mirrors a Solidity storage layout, add a parity test against that layout.
  A field reordering should fail a test instead of drifting silently into a consensus bug.
  Generated artifacts need the same treatment: embedded bytecode and slot constants must be cross-checked against the Solidity source at build time (not merely self-hashed), `build.rs` must list the generated sources in `rerun-if-changed`, and a test asserting a constant against itself is tautological.
- **Would this test still pass if the behavior it claims to check regressed?**
  The fixture runners have failed this repeatedly: an exception assertion accepting any `Err`, a setup failure skipping execution, a `zip` letting missing output pass, a duplicate test name silently overwriting, an `unwrap_or(default)` rewriting a contradictory fixture into a valid one, a filename-based skip, an unconditional prune before state-root validation.
  Benchmarks fail it the same way — an unfunded inner call, zero-byte initcode, a discarded inner-call result or an undeployed address means the measured path never runs, and a mock database that cannot fail a lookup cannot test its error path.

## Dev tools and test infrastructure

Applies to `bin/mega-evme`, `bin/mega-t8n`, `crates/mega-state-test`, `crates/state-test`, test utilities and benches — not consensus code, but `mega-state-test` is a published library and tool output feeds fixtures and indexers. Default severity cap `[Minor]`; raise only when the defect reaches that published API or corrupts a downstream fixture.

- An input-handling path (fixture, CLI flag, file path, RPC response) must fail with a structured error instead of aborting the process: a panic, unchecked index or overflow on **untrusted** input is the defect, as is `process::exit` from a library path — an idiomatic `unwrap` on a known-good value is not. A batch loop records a per-item rejection rather than aborting and dropping the rest.
- A computed output field — state root, receipt field, contract address, log index, provenance — must carry its real value or an explicit error; substituting a zero, `None`, `default()` or a silently clamped bound reports success for a result the tool never computed. Serialized output must be deterministically ordered, and an advertised flag must not be a stub returning `Ok(())`.
- Replay and capture must not diverge from real execution — source env values from the block header, honor tracer/reduction flags, key a fork cache by endpoint and not just chain id, never let a cached fixture shadow live RPC.
- Output hygiene: escape untrusted strings (revert reasons, paths, RPC payloads) before a terminal sees them, never format a secret key or credential-bearing URL into an error or log, and contain output paths — `PathBuf::join` on an external filename escapes the base directory.
- An override that mutates a `TxEnv` still needs `validate()`; `entry(addr).or_default()` shadows real forked account metadata; a shared thread-local cannot carry per-transaction override data.

## mega-evm tooling and scope

- Never flag what CI already enforces: `cargo fmt`, `cargo clippy`, and `cargo sort` (dependency ordering).
- When assessing breakage, name the affected downstream consumers explicitly: `mega-reth`, `test-client`.
