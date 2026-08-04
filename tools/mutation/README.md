# MegaETH domain mutator (v0)

Minimal, **stdlib-only** mutation harness for two MegaETH-specific properties that
generic mutation tools miss:

1. **Spec gates** — expressions of the form `<recv>.is_enabled(MegaSpecId::X)`
   (also `crate::MegaSpecId::X`).
2. **Gas / size constants** — `const NAME: T = <rhs>;` declarations in
   `crates/mega-evm/src/constants.rs`.

The tool lives entirely under `tools/mutation/`. It is **not** part of the Cargo
workspace and has **zero CI impact**.

## Operators

| Operator | Sites | Bodies per site | Transformation |
|----------|-------|-----------------|----------------|
| `spec_gate` | `crates/mega-evm/src/**/*.rs` | 2 | whole expression → `true` / `false` |
| `gas_const` | `constants.rs` `const` decls | 2 | RHS → `(RHS) + 1` / `(RHS) - 1` |

### Spec-gate details

- Matches `.is_enabled((?:crate::)?MegaSpecId::IDENT)`.
- Walks left from the `.` to capture the full receiver (`spec`, `ctx.spec`,
  `$context.host.spec_id()`, `MegaSpecId::REX5`, …).
- Skips lines that are comments (`//`, `///`, `//!`).
- Skips matches inside double-quoted string literals (simple heuristic).
- Manual exclusions are listed in the report.

### Gas-const details

- Parses `pub const NAME: <type> = <rhs>;` including multi-line RHS.
- Preserves trailing `//` comments after `;`.
- Skips the `−1` body when RHS is the literal `0` (none today).
- Includes `usize` size limits (`MAX_CONTRACT_SIZE`, …) — they are consensus values.
- Does **not** mutate `pub use gas::{...}` re-exports.

## Oracle (layered)

For each mutant:

1. Assert full worktree is clean via `git status --porcelain`
   (whitelist: harness **runtime artifacts** only —
   `tools/mutation/reports/`, `tools/mutation/logs/`,
   `tools/mutation/state*.json`, `tools/mutation/.mutate-journal.json`;
   harness sources such as `mutate.py` / `README.md` are **not** allowed dirty;
   rename/copy lines require both paths to pass).
2. Journal original content + hashes, then apply text replacement.
3. **L1**: `cargo test -p mega-evm --quiet`
   - fail → **killed** at L1; record first failing test / compile error.
4. If L1 passes → **L2**: `cargo test -p mega-state-test --quiet`
   - fail → **killed** at L2; else **survived**.
5. **Compare-and-restore** from the journal (no `git checkout`):
   - file hash == mutated → write original text, clear journal;
   - == original → clear journal only;
   - otherwise (concurrent edit) → **do not touch the file**, keep journal,
     abort the campaign with the scene preserved.
6. Re-assert full-tree cleanliness after restore (catches collateral writes
   during the oracle). Unexpected dirt → abort, scene preserved.
7. Persist progress to state JSON (`--resume` safe).

Per-mutant cargo logs land in `tools/mutation/logs/`.

## CLI

```bash
# Inventory only (~320 bodies)
python3 tools/mutation/mutate.py --list

# Filter by file / const / spec substring
python3 tools/mutation/mutate.py --list --filter STORAGE_CALL_STIPEND
python3 tools/mutation/mutate.py --list --filter REX6

# Run a subset (sentinels are prioritized by default)
python3 tools/mutation/mutate.py --limit 40 \
  --report tools/mutation/reports/subset-report.md \
  --title "M2 subset"

# Resume an interrupted campaign
python3 tools/mutation/mutate.py --limit 40 --resume \
  --state tools/mutation/state.json \
  --report tools/mutation/reports/subset-report.md
```

| Flag | Meaning |
|------|---------|
| `--list` | Enumerate and print; do not mutate |
| `--filter SUB` | Keep mutants whose mid/file/site/body contains `SUB` |
| `--limit N` | Run at most N mutants |
| `--resume` | Resume a campaign; **requires** state binding to match HEAD, inventory hash, and oracle command |
| `--fresh` | Discard any existing state and start a new campaign (incompatible with `--resume`) |
| `--state PATH` | State file (default `tools/mutation/state.json`) |
| `--report PATH` | Markdown report path |
| `--title STR` | Report title |
| `--no-sentinel-priority` | Do not force must-kill sentinels first |
| `--skip-clean-check` | Debug only — skip initial clean-tree assert |
| `--skip-baseline` | Debug only — skip clean-tree L1+L2 baseline |

## Campaign safety (M4)

- **Binding**: state JSON stores `{head, inventory_hash, oracle_command}`.
  `--resume` refuses on mismatch and hints `--fresh`.
- **Atomic state write**: temp file + `os.replace`.
- **Queue**: saved at campaign start and **used** on resume; cleared when the
  campaign finishes (results + binding remain for audit).
- **Baseline**: unmutated L1+L2 runs once before any mutant; failure aborts.
- **Journal + compare-and-restore**: before each apply, original file content +
  hashes land in `tools/mutation/.mutate-journal.json`. Startup recovery and
  the regular per-mutant restore share the same hash logic (no `git checkout`).
  Concurrent-edit hash mismatch refuses to touch the file, keeps the journal,
  and aborts the campaign.
- **Full-tree clean**: after baseline and after every mutant restore the harness
  runs `git status --porcelain` on the whole repo. Only harness **runtime
  artifacts** are whitelisted (`tools/mutation/reports/`, `logs/`,
  `state*.json`, `.mutate-journal.json`). Harness sources (`mutate.py`,
  `README.md`, `TODO.md`, …) dirty or renamed-into-artifact-dir abort and
  preserve the scene. Rename/copy porcelain lines check **both** path sides.

## Spec-gate exclusions (non-product)

Enumeration skips `is_enabled(...)` sites that are not product semantics:

- Inside `#[cfg(test)]` modules / items
- Inside `assert!` / `debug_assert!` / `assert_eq!` / `assert_ne!` arguments
- Comment lines and simple string-literal matches (pre-existing)

## Sentinels

Default must-kill mutants used to validate the harness:

- `gas_const:mini_rex::BLOCK_DATA_LIMIT:+1` — pinned by
  `tests/mutation/constants.rs` (`assert_eq!(…, 13_107_200)`).
- `gas_const:rex::TX_DATA_LIMIT:-1` — same pinning suite.
- `spec_gate:…/evm/host.rs:252:REX6:false` — product gate on
  `load_account_delegated` (M3 unit test kills both polarities).

**Not** used as sentinels: `STORAGE_CALL_STIPEND ±1` (often equivalent under
REX4 burn + rescue-exclusion), nor unit-test-only gates in `spec.rs`.

If a chosen sentinel **survives**, the process exits with code 2 — treat as a
harness bug (or re-evaluate the sentinel prediction).

## Design constraints

- Python 3, **stdlib only** (no pip deps).
- Never leave product files dirty; always restore after each body via journal
  compare-and-restore (never `git checkout` on the product path).
- The harness lives under `tools/mutation/**`; campaigns may temporarily dirty
  product files under journal control.
- Open questions live in `TODO.md`.

## Expected inventory scale

| Class | Notes | Bodies (post M4 exclusions) |
|-------|-------|-----------------------------|
| Spec-gate points | product gates only | 103 points → 206 bodies |
| Gas constants | `constants.rs` | 33 consts → 66 bodies |
| **Total bodies** | | **272** |

Deltas vs the original ~322 survey come from excluding `cfg(test)` /
`assert!`/`debug_assert!` sites plus comment/path filters.
