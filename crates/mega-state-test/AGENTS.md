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
- Corpus drivers keep going per unit (`fill_test_suite_keep_going`, `diff_test_suite`) and record a unit's failure or panic rather than ending the file.
- BaseFeeVault state changes are pruned as MegaETH-specific normalization.
- The SALT bucket hasher comes from `mega_evm::AHashBucketHasher` (via the `test-utils` feature); never introduce a standalone salt/hasher dependency.

## ANTI-PATTERNS
- Do not explain a differential disagreement with a fixture allowlist; add a `Mechanism` that reads the evidence instead, and state which hypothesis it falsifies.
- Do not spread exception matching logic across multiple files.
- Keep it centralized to avoid drift.
- Do not bypass `compute_test_roots` when changing validation outputs.
- Do not add ad-hoc skip conditions outside `skip_test`.
- Keep skip reasons documented in-place by filename.

## WHERE TO LOOK
- Add/adjust skip policy: `runner.rs::skip_test`.
- Change validation semantics for roots/output/exception: `runner.rs::{validate_exception,validate_output,check_evm_execution}`.
- Change worker behavior or fail-fast policy: `runner.rs::{run_test_worker,run,TestRunnerConfig}`.
- Change what a differential run compares or what licenses a difference: `diff.rs::{DiffField,Mechanism,judge}`.
- Change the corpus sweep or its CI gates: `tools/eest-sweep/` and `.github/workflows/eest-nightly.yml`.
- Update JSON schema mapping for test fixtures: `src/types/*` and deserializer modules.
- Change CLI flags or path handling: `crates/state-test/src/main.rs`.
