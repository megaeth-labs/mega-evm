# AGENTS.md

## OVERVIEW
Ethereum execution-specification state-test fixtures and runner adapted for MegaEVM equivalence validation and regression detection.
Published as `mega-state-test`; the library keeps the `state_test` import name.
The `state-test` CLI (`crates/state-test`) is a thin front-end over this crate.

## STRUCTURE
- `src/runner.rs`: test discovery, execution pipeline, validation, worker concurrency, `--fill`.
- `src/diff.rs`: differential execution — run one fixture under two specs and classify any disagreement against the target spec's precision invariant.
- `src/panic_capture.rs`: turning a panic inside one fixture unit into a recorded result instead of a lost run.
- `src/types/`: forked revm statetest data model and deserializers.
- `src/utils.rs`: root/hash validation helpers and utility glue.
- `tests/`: replay-corpus validation, fixture benches, and dump round-trip tests (rely on `bench/replay/fixtures/`, so they are excluded from the published package).

## KEY PATTERNS
- Runner forces MegaEVM compatibility assumptions from this crate's adaptation layer.
- Expected-exception handling and output/root validation are centralized in `runner.rs`.
- Known slow/problematic vectors are explicitly skipped by filename list.
- Failure debugging path can re-run with tracer context for inspection.
- Parallel execution uses shared queue and atomic counters with optional single-thread mode.
- Differential classification is evidence-based, never a list of fixtures allowed to differ: every `Mechanism` is a fact read off an execution, and the hypothesis it falsifies is what licenses a difference.
- Only an execution-provenance observation licenses anything. The fixture is the input under test, so a `Mechanism` read out of revert-payload bytes is reported and never falsifies a hypothesis; a derived quantity (the Rex7 destroyed remainder) needs an independent witness rather than certifying itself.
- A differential run is defined for exactly one spec pair, the one whose precision invariant the classifier encodes (`DiffSpecs::new`). There is no general two-spec comparator.
- A unit is a family of transactions, one per vector its `post` names (`TestUnit::vectors`). Diff, fill and bench each enumerate them; nothing takes index `{0,0,0}` and calls it the unit.
- The transaction vector is the unit of counting everywhere (`FillReport::vectors`, `diff_test_suite`, validation's judged count), so one corpus produces one total whichever mode swept it.
- Every mode fails when it judged nothing: an empty tally is truthful and meaningless, and neither a corpus that never arrived nor a unit that pins no expectation may read as a pass. What counts is work actually judged — an expectation checked, a vector filled — never a file walked or a unit parsed.
- A value whose constructor is the check keeps its fields private (`DiffSpecs`), so the classifier cannot be handed a pair that was assembled around `new`.
- Corpus drivers keep going per unit (`fill_test_suite_keep_going`, `diff_test_suite`) and record a unit's failure or panic rather than ending the file.
- BaseFeeVault state changes are pruned as MegaETH-specific normalization.
- The SALT bucket hasher comes from `mega_evm::AHashBucketHasher` (via the `test-utils` feature); never introduce a standalone salt/hasher dependency.

## ANTI-PATTERNS
- Do not explain a differential disagreement with a fixture allowlist; add a `Mechanism` that reads the evidence instead, and state which hypothesis it falsifies.
- Do not let a `Mechanism` inferred from bytes the fixture could have written falsify a hypothesis; `Mechanism::provenance` records where an observation came from and the licensing rule follows it.
- Do not classify a halt by matching its `Debug` rendering; match the `MegaHaltReason` variants with no catch-all arm, so a new variant has to be decided rather than defaulted.
- Do not drop an entry the fixture-discovery walk could not read; an unreadable directory is a hole in coverage, not an empty one.
- Do not count units, files, or anything else a run merely reached in a tally that gates a sweep; count the judgements it made.
- Do not write a unit's `post` for some of its vectors, and do not record a unit-wide field (`out`) for a multi-vector unit whose vectors disagree on it; refuse the unit instead.
- Do not spread exception matching logic across multiple files.
- Keep it centralized to avoid drift.
- Do not bypass `compute_test_roots` when changing validation outputs.
- Do not add ad-hoc skip conditions outside `skip_test`.
- Keep skip reasons documented in-place by filename.

## WHERE TO LOOK
- Add/adjust skip policy: `runner.rs::skip_test`.
- Change validation semantics for roots/output/exception: `runner.rs::{validate_exception,validate_output,check_evm_execution}`.
- Change worker behavior or fail-fast policy: `runner.rs::{run_test_worker,run,TestRunnerConfig}`.
- Change what a differential run compares or what licenses a difference: `diff.rs::{DiffField,Mechanism,Provenance,halt_kind,judge}`.
- Change which spec pair a differential run accepts: `diff.rs::DiffSpecs::new`.
- Change how a unit's transaction vectors are enumerated: `types/test_unit.rs::TestUnit::vectors`.
- Change what a fill records per unit or reports per vector: `runner.rs::{fill_unit,fill_suite,FillReport}`.
- Change the corpus sweep, how it decides a cached corpus is whole, or its CI gates: `tools/eest-sweep/` (`run.sh`, `tests/cache_integrity.sh`) and `.github/workflows/eest-nightly.yml`.
- Update JSON schema mapping for test fixtures: `src/types/*` and deserializer modules.
- Change CLI flags or path handling: `crates/state-test/src/main.rs`.
