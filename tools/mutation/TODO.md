# Domain mutator — open questions / future work

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
