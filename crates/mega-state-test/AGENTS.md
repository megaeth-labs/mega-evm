# AGENTS.md

## OVERVIEW
The execution-spec state-test runner of the Satin engine: Ethereum's fixtures through `MegaEvm` in equivalence mode (a gate) and Satin mode (a report), the witness replay of a fixture's transaction (a gate), and the blockchain tests through Satin's block executor (a gate).
Published as `mega-state-test`; the library keeps the `state_test` import name.
The `state-test` CLI (`crates/state-test`) is a thin front end over this crate.
`README.md` says what the two modes are and how a test is judged.

## STRUCTURE
- `src/runner.rs`: discovery, parallel execution, judging a test, the report, the summary and the gate.
- `src/mode.rs`: the two configurations a test runs under, built from `mega-evm`'s `test_utils::{neutral_cfg, neutralize_evm}`.
- `src/fork.rs`: the fixture forks a run executes (Osaka, Amsterdam).
- `src/exceptions.rs`: validation errors mapped to the execution-spec exception names.
- `src/skips.rs`: the reference runner's skips, with their reasons.
- `src/deviations.rs`: the registry of places Satin differs on purpose, each with the exact entries it explains and the hashes Satin produces for them; `DEVIATIONS.md` is rendered from it.
- `src/roots.rs`: the post-state root and the logs hash.
- `src/blockchain/`: the blockchain tests' gate. `mod.rs`: discovery, judging a test block by block, the report and the gate; `fixture.rs`: the fixture as the runner reads it, every block decoded from its RLP; `chain.rs`: a test's chain imported through `MegaBlockExecutor`, the Satin fork's parameters that bind nothing (`chain_spec`), and the accounts a Satin block adds (`SATIN_ACCOUNTS`); `skips.rs`: the classes a test is skipped for, decided from its content.
- `src/witness.rs`: `check_replay`, a fixture entry executed on a recorder of every read and replayed on a strict database and environments serving exactly a witness: the record of every read, and, for an entry the engine executed, the channel witness a node builds; `Replayed` says which.
- `tests/runner.rs`: the runner on fixtures written in the test, filled from revm's mainnet EVM on the fixture's fork.
- `tests/blockchain.rs`: the blockchain runner on chains written in the test, filled block by block from revm's mainnet EVM on Osaka, the pre-block system calls included.
- `tests/witness.rs`: `check_replay` on two fixtures written in the test, an executed one and a rejected one, and on the execution-spec fixtures `MEGA_STATE_TEST_FIXTURES` names (always ignored by default: it runs only with `-- --ignored` and the variable set; `.github/workflows/exec-spec-satin.yml` runs it on every file of both releases in both modes).

The fixture types are the revm fork's own (`revm::statetest_types`, the `test-types` feature), re-exported as `state_test::types`.

## KEY PATTERNS
- A fixture's expectation is Ethereum's; a test here that needs one fills it from revm's mainnet EVM, never from `MegaEvm`.
- Every failure is a `FailureKind`; equivalence mode names the deviation that explains it, and one it does not name is unattributed.
- A deviation lists the exact entries it explains — file, test, data, gas and value indices — with the hashes Satin produces; a failure is its only with those hashes, and `--expect-deviations` requires every listed entry to fail exactly as listed.
- Skips mirror the reference runner's, so the executed and skipped counts equal the ones `.github/workflows/exec-spec.yml` pins for it.
- A blockchain test is skipped only for a class decided from its content (`src/blockchain/skips.rs`); its failures are explained only by deviations the registry already has, each listing the test with what Satin produces at the block it fails at.
- An account is taken out of a blockchain test's state root only by `SATIN_ACCOUNTS`, only when the pre-state does not hold it and it holds exactly what Satin put there.

## ANTI-PATTERNS
- Do not accept any error for an expected exception; map the error in `src/exceptions.rs`.
- Do not register a deviation for a failure that is a bug; a deviation is a rule Satin keeps on purpose, with its reason.
- Do not skip a test to make the gate pass; a skip is one the reference runner makes too.
- Do not reach an engine switch from here; the neutral configuration lives in `mega-evm` behind `test-utils`.

## WHERE TO LOOK
- Add or move a deviation: `src/deviations.rs`, its entries taken from a `--json-outcome` run, then `UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations`.
- Change what a mode configures: `src/mode.rs`, and `mega-evm`'s `src/test_utils/neutral.rs`.
- Change how a test is judged: `src/runner.rs::check`, `src/exceptions.rs`.
- Change how a blockchain test is judged: `src/blockchain/mod.rs`; its skips: `src/blockchain/skips.rs`; what is taken out of its state root: `src/blockchain/chain.rs`.
- Add a blockchain test to a deviation: its `blockchain_entries`, taken from a `state-test btest --json-outcome` run.
- Change the pinned executed and skipped counts: `.github/workflows/exec-spec-satin.yml`.
