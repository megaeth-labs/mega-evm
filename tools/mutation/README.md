# MegaETH domain mutator (v0)

Minimal, **stdlib-only** mutation harness for MegaETH-specific properties that
generic mutation tools (e.g. `cargo-mutants`) miss or cannot express cleanly:

1. **Spec gates** — expressions of the form `<recv>.is_enabled(MegaSpecId::X)`
   (also `crate::MegaSpecId::X`).
2. **Gas / size constants** — `const NAME: T = <rhs>;` declarations in
   `crates/mega-evm/src/constants.rs`.
3. **Adjacent-spec swaps** — replace `MegaSpecId::X` with its predecessor or
   successor at the same gate sites (activation-boundary shifts).
4. **Protocol call deletion** — delete statement-position frame/limit recording
   calls that encode resource-limit and frame-alignment protocol.

The tool lives entirely under `tools/mutation/`. It is **not** part of the Cargo
workspace and has **zero CI impact**.

## Operators

| Operator | Sites | Bodies per site | Transformation |
|----------|-------|-----------------|----------------|
| `spec_gate` | `crates/mega-evm/src/**/*.rs` | 2 | whole expression → `true` / `false` |
| `gas_const` | `constants.rs` `const` decls | 2 | RHS → `(RHS) + 1` / `(RHS) - 1` |
| `adjacent_spec` | same inventory as `spec_gate` | up to 2 | `MegaSpecId::X` → pred / succ |
| `call_delete` | `src/{limit,evm,sandbox,system}/**` | 1 | delete whole statement call |

### Spec-gate details

- Matches `.is_enabled((?:crate::)?MegaSpecId::IDENT)`.
- Walks left from the `.` to capture the full receiver (`spec`, `ctx.spec`,
  `$context.host.spec_id()`, `MegaSpecId::REX5`, …), including **line-wrapped**
  method chains (shared segment walker with `call_delete`).
- Skips lines that are comments (`//`, `///`, `//!`).
- Skips matches inside double-quoted string literals (simple heuristic).
- Skips a site whose receiver walk yields **nothing** (reported as
  `empty-receiver`): replacing that span would splice the body straight after an
  unparsed prefix, and the compile error would be miscounted as a kill.
- Manual exclusions are listed in the report.

### Gas-const details

- Parses `pub const NAME: <type> = <rhs>;` including multi-line RHS.
- Preserves trailing `//` comments after `;`.
- Skips the `−1` body when RHS is the literal `0` (none today).
- Includes `usize` size limits (`MAX_CONTRACT_SIZE`, …) — they are consensus values.
- Does **not** mutate `pub use gas::{...}` re-exports.

### Adjacent-spec details

A `spec_gate` true/false flip moves behavior for **all** specs at once.
An adjacent swap moves the activation boundary by **exactly one** spec, so a
survivor pinpoints an untested boundary between two specific specs.

- Same site inventory and exclusions as `spec_gate` (product gates only).
- Spec order (must match `MegaSpecId` in `crates/mega-evm/src/evm/spec.rs`;
  enumeration hard-fails if the source enum diverges):

  `EQUIVALENCE → MINI_REX → REX → REX1 → REX2 → REX3 → REX4 → REX5 → REX6 → REX7`

- For each site with `MegaSpecId::X`:
  - **pred**: replace with predecessor (skipped when `X` is first)
  - **succ**: replace with successor (skipped when `X` is last)
- Mutant id: `adjacent_spec:<file>:<line>:<X>:<pred|succ>=<Y>`
- Preserves a `crate::` path prefix when present.

### Call-delete details

This operator executable-izes the resource-limit check protocol and frame
alignment rules: a survivor means a forgotten latch or stack misalignment could
ship silently.

Curated callees (match `self.<name>(...)` / `<recv>.<name>(...)` as a **full
statement** whose value is discarded):

`check_limit`, `push_frame`, `pop_frame`, `push_empty_frame`,
`before_frame_init`, `before_frame_return_result`, `on_sstore`, `on_log`,
`record_oracle_hint_bytes`, `record_compute_gas_all_dims`

- Scope: `crates/mega-evm/src/{limit,evm,sandbox,system}/**/*.rs`
- Same non-product exclusions as `spec_gate` (`#[cfg(test)]`, assert macros,
  comments, string literals).
- Only statement position: single-line `…callee(…);` or a clearly delimited
  multi-line call ending in `);`. Expression position (assigned, returned, in a
  condition, `?`-propagated) is **skipped**.
- Turbofish calls (`before_frame_return_result::<true>(…)`) are included when
  statement-shaped.
- Mutant id: `call_delete:<file>:<line>:<callee>`
- Some deletions still fail to compile (e.g. `#[must_use]`); the harness treats
  compile failure as killed, same as other operators. `--list` reports
  enumerated vs skipped counts per callee.

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
   - exceeds the wall-clock ceiling → **killed** at L1 with
     `kill_kind="timeout"` (see [Test timeout](#test-timeout)).
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
# Inventory only
python3 tools/mutation/mutate.py --list

# Filter by file / const / spec / operator substring
python3 tools/mutation/mutate.py --list --filter STORAGE_CALL_STIPEND
python3 tools/mutation/mutate.py --list --filter adjacent_spec
python3 tools/mutation/mutate.py --list --filter call_delete

# Run a subset (sentinels are prioritized by default)
python3 tools/mutation/mutate.py --limit 40 \
  --report tools/mutation/reports/subset-report.md \
  --title "M2 subset"

# Resume an interrupted campaign
python3 tools/mutation/mutate.py --limit 40 --resume \
  --state tools/mutation/state.json \
  --report tools/mutation/reports/subset-report.md

# Override the per-layer test timeout (seconds)
python3 tools/mutation/mutate.py --limit 40 --test-timeout 900
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
| `--test-timeout SECONDS` | Wall-clock ceiling for each oracle `cargo test` call (default: derived from the baseline — see below) |

## Test timeout

A mutant can make the tested path — or a test loop — run forever (a flipped
spec gate that turns a `while` condition always-true, for instance). Without a
ceiling that hangs the whole campaign: no result, no state progress, no journal
recovery, and a manual kill can land while the product file is still mutated.

Every oracle `cargo test` call therefore runs under a wall-clock ceiling.

**Derivation (explicit always wins):**

1. `--test-timeout N` → both layers use `N` seconds.
2. Otherwise, from the clean-tree baseline, **per layer**:
   `max(300, ceil(5 × that layer's baseline wall time))`.
   With an L1 baseline of 363.3s and an L2 baseline of 11.8s that is
   `L1 = ceil(1816.4) = 1817s` and `L2 = max(300, 59) = 300s` — the floor keeps
   a fast layer from getting a hair-trigger ceiling.
3. No baseline wall time available (`--skip-baseline`, and no explicit flag) →
   both layers use the absolute default **1800s**.

The baseline run itself cannot use rule 2 (it is the measurement rule 2 derives
from), so it uses `--test-timeout` when given and the 1800s absolute default
otherwise. A baseline that times out aborts the campaign like any other
baseline failure.

The resolved policy is printed at campaign start, stored in the state file
under `test_timeout`, and shown in the report.

**On expiry:**

- The child is spawned with `start_new_session=True`, so `cargo` is a process
  group leader. The harness signals the **whole group** — `SIGTERM`, 10s grace,
  then `SIGKILL` — because killing only the direct child leaves `rustc` and
  test binaries orphaned on the `target/` lock.
- The mutant is restored through the **normal journal compare-and-restore**,
  exactly like any other outcome, and the post-restore cleanliness assert runs.
- The result is recorded as `status="killed"` with `kill_kind="timeout"`,
  `timed_out=true`, `timeout_s=<ceiling>`, and
  `first_failing_test="<timeout after Ns>"`. The queue advances and `--resume`
  treats it as completed work.

**Classification:** a timeout *is* a kill (a mutant that hangs the suite is
detected), but the report counts it separately from assertion kills
(`killed=N [assertion=X, timeout=Y]`, plus a per-mutant `kind` column and a
list of timed-out mutants with their ceiling). A rising timeout count means the
threshold may be too tight, not that the tests got stronger.

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
- **Atomic product writes**: mutated source (and journal restore) is written via
  same-directory temp + `fsync` + `os.replace`, so a kill mid-write cannot leave
  a half-written file that matches neither journal hash. Orphaned `*.tmp.<pid>`
  temps are whitelisted by the cleanliness assert.
- **Per-layer test timeout**: every oracle `cargo test` call runs under a
  wall-clock ceiling; expiry kills the whole process group, restores through the
  journal, and records a `kill_kind="timeout"` kill. See
  [Test timeout](#test-timeout).
- **Full-tree clean**: after baseline and after every mutant restore the harness
  runs `git status --porcelain` on the whole repo. Only harness **runtime
  artifacts** are whitelisted (`tools/mutation/reports/`, `logs/`,
  `state*.json`, `.mutate-journal.json`, orphaned `*.tmp.<pid>`). Harness
  sources (`mutate.py`, `README.md`, `TODO.md`, …) dirty or
  renamed-into-artifact-dir abort and preserve the scene. Rename/copy porcelain
  lines check **both** path sides.

## Spec-gate exclusions (non-product)

Enumeration skips `is_enabled(...)` sites that are not product semantics
(also applied to `adjacent_spec` and, for test/assert filters, `call_delete`):

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

New operators (`adjacent_spec`, `call_delete`) should be smoke-checked with
explicit `--filter` runs on known-kill sites before a full campaign.

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
| Spec-gate points | product gates only | ~104 points → ~208 bodies |
| Gas constants | `constants.rs` | ~33 consts → ~66 bodies |
| Adjacent-spec | same sites; ends omit one body | ~180–210 bodies |
| Call-delete | statement-position only | tens of bodies (see `--list`) |
| **Total bodies** | | **see `--list` on HEAD** |

Deltas vs early surveys come from excluding `cfg(test)` /
`assert!`/`debug_assert!` sites plus comment/path filters.
Exact adjacent-spec and call-delete counts depend on current product code;
always re-run `--list` after a rebase.
