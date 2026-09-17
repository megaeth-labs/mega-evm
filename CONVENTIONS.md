# CONVENTIONS.md — Structure & Taste Guide for `mega-evm`

This document tells a future PR author _where new code belongs_ and _how to keep it consistent_ with this crate's organizational grammar.
It is about structure and taste — module placement, file roles, composition idioms, readability mechanisms — not build/spec/process.
For those, see the companions and do not duplicate them here:

- `AGENTS.md` (crate root + nested per-module) — build, `no_std`, spec progression, and the WHERE-TO-LOOK routing tables.
- `ARCH.md` — spec/consensus behavior per feature.
- `REVIEW.md` — mega-evm-specific review rules (the PR gate).

The crate's guiding values: _good taste_ (eliminate special cases), _make invalid states unrepresentable_, _additive spec evolution_ (never rewrite history), and _named, explicit patterns_ over ad-hoc branching.

---

## How to use this doc

This doc serves two review modes; keep them distinct.

- **PR gate** (per-change review): §1–§5 define the bar, §6 lists the smells a change must not introduce, §7 is the author checklist. Flag any _new_ code that violates them.
- **Repo-level audit** (proactive readability/maintainability sweep): treat every convention in §1–§6 as a _detection rule_ and scan the whole tree for pre-existing violations, then propose cleanups. §6 doubles as a scan plan; §8 is the running registry of already-known deviations — start there, then widen the scan.

§1–§6 describe the **target** state. §8 records where today's code does **not** meet it yet.
A §8 entry is an improvement candidate, **not** an endorsed pattern — never cite it as a convention to preserve, and when you find a fresh instance of one, add it there rather than normalizing it into §1–§6.

---

## 1. The mental model — pick the module by execution-stack layer

The crate is **7 behavior modules** (`access`, `block`, `evm`, `external`, `limit`, `sandbox`, `system`) plus two single-file support modules (`constants`, `types`) and the `lib.rs` façade.
Most modules are private and glob-re-exported (`pub use evm::*`, …) so the public surface is flat; the two deliberately kept namespaced are `pub mod constants` and `pub mod sandbox`.
The dividing principle is **the layer of the execution stack a concern belongs to**, not the user-facing feature.
Map your feature onto a layer _first_; do not invent a new top-level module for a feature.

| Adding X…                                                  | …belongs in    | Notes                                                             |
| ---------------------------------------------------------- | -------------- | ----------------------------------------------------------------- |
| Opcode / instruction / host / context behavior             | `evm/`         | Thin wrapper over op-revm; one file per revm trait surface        |
| Block assembly, pre-block changes, hardfork→spec mapping   | `block/`       | Above the EVM; owns the `on_state`→commit pipeline                |
| Per-tx / per-frame resource metering                       | `limit/`       | One file per resource _dimension_; registry-of-trackers           |
| Volatile-data access detection + gas detention             | `access/`      | Stateful tracker vs. typed bitflag, kept apart                    |
| System-contract address / deploy / interception            | `system/`      | One file per contract; interceptors centralized in `intercept.rs` |
| SALT / oracle / external data behind a trait               | `external/`    | Pluggable factory boundary; block context captured at creation    |
| Isolated nested execution (keyless / Nick's-Method deploy) | `sandbox/`     | Type-erased DB to dodge the monomorphization recursion ICE        |
| A tunable number                                           | `constants.rs` | One `pub mod <spec>` block per hardfork; never inline             |
| Crate-wide upstream type alias                             | `types.rs`     | `Mega*`-prefixed vocabulary shared across modules                 |
| Short public alias (`Evm`, `Context`, `SpecId`)            | `lib.rs`       | One block, kept 1:1 with the canonical `Mega*` types              |

The same noun recurs per layer with a layer-specific meaning — `limit.rs` exists in `evm/` (per-tx config struct), `block/` (block-level accumulation), and `limit/` (the enforcement engine); `result.rs` and `factory.rs` likewise.
Keep the shared name; let the module disambiguate. Do not flatten or invent unique names.

---

## 2. File-splitting conventions

**One-X-per-file.** Each cohesive concept gets its own file; the filename _is_ the concept:

- `limit/`: one file per resource dimension — `compute_gas.rs`, `data_size.rs`, `kv_update.rs`, `state_growth.rs`, `storage_call_stipend.rs`.
- `system/`: one file per system contract — `oracle.rs`, `sequencer_registry.rs`, `keyless_deploy.rs`, `control.rs`, `limit_control.rs`.
- `evm/`: one file per revm trait surface — `host.rs` (`Host`/`HostExt`/`JournalInspectTr`), `instructions.rs` (opcode table), `execution.rs` (`Handler`), `precompiles.rs`, `factory.rs`, `interfaces.rs`.

**Recurring role-named files** (their name signals their role across the crate):

| File                      | Role                                                                                                                                              |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `mod.rs`                  | Wiring façade only: module doc, `mod x;` declarations, `pub use x::*;`, plus the top-level trait + its no-op default impl. **No business logic.** |
| `factory.rs`              | The `*Factory` trait/struct that captures context once and mints per-block / per-execution instances (methods take no block param).               |
| `result.rs`               | `*Outcome` wrappers (often `Deref` to inner) + their paired `*ExceededError` enums; accessor-only, no execution logic.                            |
| `error.rs`                | One fat error enum + mirrored `encode_error_result`/`decode_error_result` + round-trip tests when errors cross an ABI/consensus boundary.         |
| `helpers.rs` / `eips.rs`  | Free `pub(crate) transact_*` functions returning _uncommitted_ `Option<EvmState>` (see §4 contract).                                              |
| `spec.rs` / `hardfork.rs` | The spec enum (`MegaSpecId`) / the hardfork→spec activation mapping.                                                                              |
| `test_utils.rs`           | Configurable mock impls, gated `#[cfg(any(test, feature = "test-utils"))]`.                                                                       |

**When a concern earns its own file vs. a new module:** a _new cohesive concept within a layer_ (a new tracker, a new system contract, a new external source, a new revm trait impl) → new **file** in the owning module.
A _whole new execution-stack layer_ → new **module** (rare; the 7 are expected to be stable).
Extend an existing file only when the change belongs to a concept already living there.

**Split by direction / lifecycle when one file would mix concerns:**

- `sandbox/state.rs` (read-only DB wrapper) is split from `sandbox/state_merge.rs` (replay-sensitive write-back) so the consensus-critical merge is isolated.
- `access/tracker.rs` (mutable stateful tracker, raw caps only) is split from `access/volatile.rs` (the `bitflags!` newtype it manipulates).

---

## 3. Composition patterns — how to plug in

These are the named idioms. New behavior = a new unit + a registration line, not scattered branching.

**Registry-of-trackers (`limit/`).**
`AdditionalLimit` (`limit/limit.rs`) is a thin coordinator owning one field per sub-tracker.
Every lifecycle hook (`before_tx_start`, `before_frame_init`, `after_frame_run_instructions`, `before_frame_return_result`, `on_sstore`, `on_log`, `push_empty_frame`, …) is a **fan-out** that calls the identically-named method on each sub-tracker _in the same order_, then runs `check_limit()`.
Put no dimension math in the coordinator.
Every tracker implements the shared `TxRuntimeLimit` trait (`limit/frame_limit.rs`) — override only the hooks your dimension reacts to; leave the rest as empty defaults.
Compose frame-awareness from the generic `FrameLimitTracker<I>` (it owns the per-frame stack, the 98/100 child-budget rule, persistent/discardable/refund lanes, and an O(1) `net_usage` cache) — never hand-roll a frame stack.
Mutate usage only through `add_tx_persistent` / `add_frame_discardable` / `add_frame_refund` (direct field writes desync the cache).
Cross-tracker identity/result types live in `limit/mod.rs`: `LimitKind`, `LimitCheck`, the `MegaLimitExceeded` `sol!` ABI — a new dimension adds a _variant here_, it does not invent its own result type.

**Lifecycle hooks as the universal surface (`evm/execution.rs`).**
The EVM frame lifecycle is the one interception point.
`evm/execution.rs` is the single site that invokes hooks on `additional_limit`; trackers mirror the same method names one level down.
Attach cross-cutting behavior to an existing hook rather than inventing a new one.

**Macro-wrapped, layered instruction table (`evm/instructions.rs`).**
Opcode behavior is a stack of single-purpose `*_ext` modules — `compute_gas_ext` → `storage_gas_ext` → `forward_gas_ext`/`additional_limit_ext` → `volatile_data_ext` — each generated by a `wrap_op_*!` / `wrap_call_*!` macro, called through `run_inner_instruction_or_abort!`.
~150 opcodes are _one registration line each_; never hand-write a per-opcode handler.
The inner→outer layering **is** the contract; add a new orthogonal concern as a new `*_ext` module, not a new branch inside an existing handler.

**Trait-based factories / extension points (`external/`, `evm/factory.rs`, `system/intercept.rs`).**
Swap backends through traits whose factory captures block context at creation (`ExternalEnvFactory::external_envs(block)`, `EvmFactory::create_evm`); methods on the product take no block param.
Ship every trait with a deterministic no-op default impl in `mod.rs` (`EmptyExternalEnv`) and a configurable mock in `test_utils.rs`.
Wrap upstream behavior via `Deref` / the `delegate!` macro; hand-write only the methods MegaETH customizes.

**Spec-gating as the branch-new-behavior idiom.**
This is the universal switch. Three granularities:

- _Whole opcode-table slot changes_ → new `mod rexN` in `instructions.rs` whose `instruction_table()` clones the prior spec's table and reassigns only changed slots; doc the delta as "Changes from rexN-1".
- _Sub-opcode / call-site deltas_ → inline `if spec.is_enabled(MegaSpecId::REXn) { new } else { legacy }`, legacy branch left byte-for-byte verbatim, comment naming the spec + replay reason.
- _Tracker / helper behavior_ → cache `spec.is_enabled(REXn)` as a `rexN_enabled: bool` in `new()` and branch inside; never fork the tracker (`DataSizeTrackerV2`) or the aggregator by spec.

`is_enabled` is monotonic (`other as u8 <= self as u8`); always check against the spec that _introduced_ the behavior.

---

## 4. When to add a new file vs. extend an existing one

| You are adding…                        | Do this                                                                                                                                                                                                                                                                                         |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A new resource dimension               | New file `limit/<dimension>.rs` with one `*Tracker` impl'ing `TxRuntimeLimit`; add a field + fan-out lines to `AdditionalLimit`; add a `LimitKind` variant in `limit/mod.rs`; update `limit/AGENTS.md`.                                                                                         |
| An opcode tweak                        | Reuse/extend a `*_ext` macro layer in `evm/instructions.rs`; gate with a spec branch or a new `mod rexN` table delta. Never edit the base table to "fix" history.                                                                                                                               |
| A new hardfork                         | New `mod rexN` table (delta over prior) + `MegaSpecId::REXn` arm; new `pub mod rexN` in `constants.rs` referencing earlier specs via `super::`.                                                                                                                                                 |
| A new system contract                  | New file `system/<name>.rs` (address + `Vx_y_z_CODE`/`_CODE_HASH` re-exports + ABI + `transact_deploy_*` helper); register a gated call in `block/executor.rs::pre_execution_changes`; add an interceptor in `system/intercept.rs` _if_ it intercepts runtime calls; update `system/AGENTS.md`. |
| A runtime call interception            | New unit struct in `system/intercept.rs` impl'ing the interceptor trait with `const ACTIVATION_SPEC`; one gated branch in the dispatch function.                                                                                                                                                |
| A swappable external data source       | New trait file in `external/` + its `EmptyExternalEnv` no-op impl + a `test_utils.rs` mock.                                                                                                                                                                                                     |
| A tunable number                       | New `pub const` in the spec sub-module of `constants.rs` (dimension-local byte weights may live atop the dimension file). Never inline.                                                                                                                                                         |
| A new CLI subcommand (`bin/mega-evme`) | New `src/<name>/` folder: `cmd.rs` (clap `Cmd` + `async run`) + `mod.rs` (wiring); register in `cmd.rs`'s `Commands` enum. Shared plumbing only in `common/`.                                                                                                                                   |
| A test                                 | New file under the spec dir named after the introducing hardfork (`equivalence`/`mini_rex`/`rex`…`rex5`, or `block_executor` for cross-spec); register with one `mod x;` line in that dir's `main.rs`.                                                                                          |

**Extend an existing file** only when the change belongs to a concept already living there (e.g. a new SSTORE weight goes into the dimension's own file, not a new one).

---

## 5. Readability & maintainability mechanisms

- **Macros hide boilerplate.** `wrap_op_*!`/`wrap_call_*!` for opcodes; the `delegate!` macro + `derive_more` `Deref`/`AsRef` for transparent wrappers (`MegaContext`, `EnrichedMegaTx`, outcome types). The only hand-written code is the behavior the wrapper genuinely adds.
- **Invariants made explicit, in prose + assertion + test.** Subtle contracts are stated in a doc-comment _on_ the field/method, backed by `debug_assert!`, and pinned by a named regression test:
  - **PRE-BLOCK STATE CHANGE CONTRACT** (`system/mod.rs`, `block/AGENTS.md`): pre-block helpers return `Option<EvmState>` and **never call `db.commit`**; the executor fires `on_state` (witness) _before_ committing. Return `Some(read-only EvmState)` even on no-change paths; `None` means _gated out entirely_.
  - **Frame-stack alignment**: any path that short-circuits `frame_init` (interceptors, access early-returns) must `AdditionalLimit::push_empty_frame()` so every tracker stack stays 1:1 with the call stack.
  - **Gas-leakage / cache-sync paths**: `net_usage` cache invariant, stipend-exclusion, discardable-rollback-on-revert — each documented and debug-asserted.
  - **Frozen behavior**: pre-fork branches and load-bearing hydration paths carry a comment forbidding "cleanup", plus a test (e.g. `test_inspect_account_delegated_does_not_hydrate_pre_rex5`).
- **Constants centralization.** `constants.rs` is a chronological, append-only ledger: one `pub mod <spec>`, newer specs deriving from older via `super::<spec>::X`. A reader sees exactly what each hardfork tuned without opening execution code.
- **Newtypes over primitive aliases.** Resource/access values are typed (`VolatileDataAccess` bitflags, `BucketId`, `LimitKind`), not bare integers, so invalid states are unrepresentable. Errors wrap their upstream base as a `Base(OpHaltReason)` variant with explicit `From`/`TryFrom`.
- **Nested `AGENTS.md` per module** is the authoritative file-role map and contract registry (STRUCTURE / KEY PATTERNS / ANTI-PATTERNS / WHERE-TO-LOOK pointing at exact `file::function`). Update it whenever you add a file or change an invariant. (`sandbox/` is the lone module without one — its contract lives in `sandbox/mod.rs` / `sandbox/state.rs`; treat those headers as authoritative.)
- **Doc-comment style:** every test/bench file opens with a `//!` stating the exact invariant it pins; cross-module contracts are documented bidirectionally (producer side in one module, consumer side in the other, cross-linked by path).

---

## 6. Smells to avoid

- **Mutating stable spec behavior in place.** Never "fix" a buggy opcode slot or refactor a pre-fork branch — historical blocks must replay byte-for-byte. The fix is a gated branch / a delta-override in the _deriving_ table, with the old path left verbatim.
- **`db.commit` inside a pre-block helper.** Bypasses the `on_state` witness hook → silently incomplete stateless witness. Return `Option<EvmState>`; let the executor commit.
- **Returning `None` as an "already up to date" shortcut.** Drops the account from the witness read set. Use `Some(read-only EvmState)`; reserve `None` for gated-out.
- **Ad-hoc constants at call sites.** Add to the spec sub-module of `constants.rs` (or the dimension's own weight block) and reference it.
- **Bypassing the tracker registry.** Inlining metering into opcode handlers or `execution.rs`, mutating `persistent_usage`/`discardable_usage` directly, or skipping a `push_empty_frame` on a synthetic frame — all desync the per-frame stacks.
- **Forking a type per spec** (`FooTrackerV2`, a spec-forked aggregator). Cache the `is_enabled` bool and branch inside.
- **Hardwiring system bytecode as raw literals.** Re-export `Vx_y_z_CODE`/`_CODE_HASH` from `mega-system-contracts` and pin with a `keccak256` test (one documented legacy embedded constant excepted).
- **Reaching across module internals.** Compose through the boundary trait/hook; don't import another module's private types.
- **Primitive aliases where a newtype belongs**, or logic placed in a `mod.rs` (which must stay a wiring façade).
- **>3 indentation levels / deep nesting** instead of layered `*_ext` composition or early returns.
- **Dead commented-out code left inline** (e.g. the `increment_balances` block in `block/executor.rs`) — if it pins an equivalence, make it a named test; otherwise delete it. And **bare `#![allow(...)]`** without an inline structural justification.
- **Placeholder `StateChangeSource::Transaction(0)` for new pre-block sources** — it's a documented stopgap in `block/executor.rs`, not a pattern to copy; prefer a proper source variant.

---

## 7. Checklist before opening a PR

- [ ] New code lives in the module that owns its **execution-stack layer** (no new top-level module for a feature).
- [ ] New cohesive concept → its own file with the right **role name**; `mod.rs` left as a pure façade.
- [ ] New behavior is an **additive spec gate** (table delta, inline `is_enabled`, or cached bool) — no pre-fork path edited.
- [ ] Plugged into the right composition idiom: tracker→`AdditionalLimit` + `LimitKind`; opcode→`*_ext` macro; backend→factory trait; system contract→`system/` file + executor hook (+ interceptor + `ACTIVATION_SPEC` if intercepted).
- [ ] Every tunable number is in `constants.rs` (or a dimension weight block); nothing inlined.
- [ ] Pre-block helpers return `Option<EvmState>`, never `db.commit`; no-change paths return `Some(read-only)`.
- [ ] Any synthetic/short-circuited frame calls `push_empty_frame`; usage mutated only via the cache-aware helpers.
- [ ] Newtypes/typed enums used over bare primitives; errors wrap their `Base(...)` upstream variant.
- [ ] Load-bearing invariants stated in doc-comment + `debug_assert!` + a named regression test.
- [ ] The module's nested `AGENTS.md` STRUCTURE / WHERE-TO-LOOK updated.
- [ ] No dead commented code, no bare `#![allow(...)]` without justification, no >3-level nesting.

---

## 8. Known deviations & improvement backlog

Places where today's code does **not** yet meet §1–§6.
These are improvement candidates for a repo-level review, not endorsed patterns.
Verify each still exists before acting (line numbers drift); when you spot a new instance of one of these, add it here.

| Deviation                                                                                                                         | Location                                                                           | Convention missed                                                           | Suggested improvement                                                                                                                |
| --------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- | --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| Placeholder `StateChangeSource::Transaction(0)` used for pre-block / system-call state-change sources                             | `crates/mega-evm/src/block/executor.rs` (the `pre_execution_changes` outcomes)     | §5 typed identity / "make invalid states unrepresentable"                   | Add a dedicated `StateChangeSource` variant for pre-block & system sources and use it instead of the `Transaction(0)` stand-in.      |
| Dead commented-out `increment_balances` / `balance_increment_state` block                                                         | `crates/mega-evm/src/block/executor.rs` (post-block balance section)               | §6 "no dead commented-out code"                                             | Delete it; if it pins an equivalence worth keeping, convert it into a named regression test.                                         |
| Near-duplicate `before_frame_init` / `after_sstore` / `current_call_remaining` bodies and a copy-pasted 98/100 child-budget clamp | `crates/mega-evm/src/limit/data_size.rs`, `crates/mega-evm/src/limit/kv_update.rs` | §3 "compose frame-awareness from `FrameLimitTracker`, don't hand-roll"; DRY | Hoist the shared clamp/body into `FrameLimitTracker<I>` (or a shared helper) so each dimension keeps only its own accounting.        |
| No nested `AGENTS.md` (the lone module without one)                                                                               | `crates/mega-evm/src/sandbox/`                                                     | §5 "nested per-module `AGENTS.md` is the authoritative file-role map"       | Add `sandbox/AGENTS.md`, or make the exception explicit by blessing the `mod.rs` / `state.rs` headers as the authoritative contract. |

### Repo-wide scan plan

Turn each convention into a query when auditing the tree:

| Convention                       | Scan for violations                                                                                                                                          |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `mod.rs` is a wiring façade (§2) | `mod.rs` files carrying business logic beyond `mod`/`pub use` and the top-level trait + its default impl                                                     |
| One-X-per-file (§2)              | oversized files mixing concepts; a tracker / system contract / revm-trait impl not in its own file                                                           |
| Registry fan-out (§3)            | metering inlined in opcode handlers or `execution.rs`; direct writes to `*_usage` fields instead of the cache-aware helpers; a hook that skips a sub-tracker |
| Macro-wrapped opcodes (§3)       | per-opcode handlers not routed through `wrap_op_*!` / `run_inner_instruction_or_abort!`                                                                      |
| Spec-gating, no type forks (§3)  | `*V2` / spec-suffixed type or fn forks instead of a cached `is_enabled` bool; an edited pre-fork branch                                                      |
| Newtypes over primitives (§5)    | bare `u64` / `U256` / `Address` where a domain newtype already exists                                                                                        |
| Constants centralization (§6)    | numeric literals at call sites not sourced from `constants.rs` or a dimension weight block                                                                   |
| Depth (§6)                       | >3 indentation levels instead of early returns / layered `*_ext` composition                                                                                 |
