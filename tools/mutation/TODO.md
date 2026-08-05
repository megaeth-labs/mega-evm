# Domain mutator — open questions / future work

## Known equivalent mutants

Survivors that were analysed and found to have no observable effect. Each entry
carries the proof; do not spend test budget on them again.

- `spec_gate:crates/mega-evm/src/limit/state_growth.rs:169:REX4:true` —
  `current_call_remaining`. Only affects pre-REX4 specs, and the sole consumer of that
  value (`sandbox/execution.rs`'s `with_tx_state_growth_limit` for the REX5+ `KeylessDeploy`
  sandbox) is unreachable before REX5, so no pre-REX4 execution ever observes the added
  `min(frame_remaining, tx_remaining)`. Pre-REX4 frames are also pushed with `u64::MAX`
  (state_growth.rs:121, the only push path), so the `min` is a no-op for every state this
  tracker can reach through its own API except a refund-skewed corner with an unlimited TX
  budget — which still has no consumer.
- `spec_gate:crates/mega-evm/src/limit/state_growth.rs:206:REX4:true` — `check_limit`. The
  REX4 branch is a strict prefix of the shared TX-level check: it only adds
  `exceeds_current_frame_limit` before falling through to the same `tx_usage()` vs
  `tx_limit()` comparison the pre-REX4 path performs. Pre-REX4 frames carry a `u64::MAX`
  limit, so `used - refund > limit` can never hold and the added branch can never fire.
  Extensionally identical for every reachable state.
- `spec_gate:crates/mega-evm/src/limit/data_size.rs:74:REX5:true` — `DataSizeTracker`'s
  `rex5_enabled`. Its only read is as a disjunct inside the `debug_assert!` in `check_limit`
  (data_size.rs:193); forcing it `true` only relaxes that assertion. The field is private and
  has no accessor, so nothing else can observe it.
- `storage_call_stipend.rs` `if burn > 0` → `>=` (cargo-mutants operator). `burn == 0` makes
  the guarded call `gas.record_regular_cost(0)`, which subtracts nothing and cannot fail.
- `storage_call_stipend.rs` `if stipend > 0` → `>=` (cargo-mutants operator). With
  `stipend == 0` the taken branch computes `gas.remaining().min(gas.limit() - 0)`, and
  `remaining <= limit` always holds, so it returns `gas.remaining()` — exactly the `else`
  branch.

## Out of scope for v0

- `spec.is_enabled(SomeType::ACTIVATION_SPEC)` style gates (associated constants,
  not a literal `MegaSpecId::X` path). Appears in `system/intercept.rs` for
  interceptor activation.
- Flipping `is_enabled` *arguments* (e.g. `REX5` → `REX4`) — a stronger operator
  already referenced by a comment in `limit/limit.rs`; not implemented here.
- Equivalent gates written as `matches!` / ordinal comparisons.
- Constants outside `constants.rs` (inline magic numbers, re-exported revm gas
  names under `equivalence`).
- Parallel mutant execution / incremental test selection.

## Known heuristics / footguns

- Receiver walk for `.is_enabled` stops at whitespace and operators; unusual
  formatting (receiver split across lines with a line break *before* the `.`)
  would be missed. Current tree has no such sites.
- String-literal detection is single-line and double-quote only.
- Gas-const occurrence keys on the exact RHS text (`500_000`, `1000`, …).
  If a comment ever contains the same digit form, occurrence indexing could
  target the wrong span — review exclusions if that appears.
- Wrapping multi-line RHS as `(expr) ± 1` collapses formatting to one line
  (still valid Rust).

## Oracle

- L2 (`mega-state-test`) may be sparse relative to MegaETH-specific gates;
  most kills are expected at L1.
- No compile-only fast path yet: a type-error mutant still pays full `cargo test`
  startup (rustc fails early inside the same invocation).
- Consider an optional L0 `cargo check -p mega-evm` for faster kill on syntax
  breakage only — not added to keep the oracle definition simple.

## Campaign ops

- Full inventory (~274 bodies post M4 exclusions × ~1 min) is multi-hour; use `--resume`.
- State JSON is written atomically (temp + `os.replace`); binding validates HEAD /
  inventory hash / oracle on resume (M4).
- Crash journal + regular restore share compare-and-restore; concurrent-edit
  mismatch refuses restore, keeps journal, aborts campaign (M5).
- Full-tree `git status --porcelain` after baseline and each mutant restore;
  unexpected dirt aborts with scene preserved (M5).
  Whitelist narrowed to runtime artifacts only (reports/logs/state/journal);
  harness sources dirty → abort; rename both-sides checked (M6).
- **TODO (optional hardening)**: isolate each mutant in a temporary git worktree
  so product files in the main tree never go dirty even mid-apply. Journal restore
  is the v0 simple path; worktree isolation would remove the remaining race with
  concurrent human edits.
