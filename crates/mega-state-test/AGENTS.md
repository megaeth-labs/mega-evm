# AGENTS.md

## OVERVIEW
The execution-spec state-test runner of the Satin engine: Ethereum's fixtures through `MegaEvm` in equivalence mode (a gate) and Satin mode (a report).
Published as `mega-state-test`; the library keeps the `state_test` import name.
The `state-test` CLI (`crates/state-test`) is a thin front end over this crate.
`README.md` says what the two modes are and how a test is judged.

## STRUCTURE
- `src/runner.rs`: discovery, parallel execution, judging a test, the report, the summary and the gate.
- `src/mode.rs`: the two configurations a test runs under, built from `mega-evm`'s `test_utils::{neutral_cfg, neutralize_evm}`.
- `src/fork.rs`: the fixture forks a run executes (Osaka, Amsterdam).
- `src/exceptions.rs`: validation errors mapped to the execution-spec exception names.
- `src/skips.rs`: the reference runner's skips, with their reasons.
- `src/deviations.rs`: the registry of places Satin differs on purpose, each with its pinned count; `DEVIATIONS.md` is rendered from it.
- `src/roots.rs`: the post-state root and the logs hash.
- `tests/runner.rs`: the runner on fixtures written in the test, filled from revm's mainnet EVM on the fixture's fork.

The fixture types are the revm fork's own (`revm::statetest_types`, the `test-types` feature), re-exported as `state_test::types`.

## KEY PATTERNS
- A fixture's expectation is Ethereum's; a test here that needs one fills it from revm's mainnet EVM, never from `MegaEvm`.
- Every failure is a `FailureKind`; equivalence mode names the deviation that explains it, and one it does not name is unattributed.
- A deviation matches on fork, failure kind and a path fragment, and pins its count; the gate checks the pins with `--expect-deviations`.
- Skips mirror the reference runner's, so the executed and skipped counts equal the ones `.github/workflows/exec-spec.yml` pins for it.

## ANTI-PATTERNS
- Do not accept any error for an expected exception; map the error in `src/exceptions.rs`.
- Do not register a deviation for a failure that is a bug; a deviation is a rule Satin keeps on purpose, with its reason.
- Do not skip a test to make the gate pass; a skip is one the reference runner makes too.
- Do not reach an engine switch from here; the neutral configuration lives in `mega-evm` behind `test-utils`.

## WHERE TO LOOK
- Add or move a deviation: `src/deviations.rs`, then `UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations`.
- Change what a mode configures: `src/mode.rs`, and `mega-evm`'s `src/test_utils/neutral.rs`.
- Change how a test is judged: `src/runner.rs::check`, `src/exceptions.rs`.
- Change the pinned counts: `.github/workflows/exec-spec-satin.yml`.
