# mega-state-test

The execution-spec state-test runner of the Satin engine.
It runs Ethereum's execution-spec state-test fixtures through `MegaEvm`; the `state-test` CLI (`crates/state-test`) is its front end.
The library keeps the `state_test` import name.

## Two modes

- **Equivalence** — a gate.
  Satin's machinery (its handler, frame lifecycle, Host and instruction table) runs priced as the fixture's own fork prices it, with every dimension of pricing only MegaETH has turned off.
  Every test either passes, is skipped for a reason the reference runner shares, or fails on a [registered deviation](DEVIATIONS.md); one failure no deviation explains fails the gate.
- **Satin** — a report.
  The same fixtures under Satin's own configuration, counted by outcome.
  Satin prices state, history and the access entries it pressed back on purpose, so almost every stateful fixture differs; the count is what CI shows, and it never fails the job.

A run executes the entries one fork defines: Osaka's in the main fixture release, Amsterdam's in the glamsterdam devnet release.
Satin's base spec is Osaka, so those are the two forks it can be configured to.

## The neutral configuration

Equivalence mode is built from test tooling in `mega-evm`, behind its `test-utils` feature, and unreachable from a default build:

- `MegaContext::with_neutral_cfg` takes every configuration field as given, the ones the Satin spec fixes included, and prices no history gas for any transaction.
- `test_utils::neutral_cfg(fork)` is that configuration for Osaka or Amsterdam: the fork's gas schedule, its EIP-8037, EIP-2780 and EIP-7708 switches, EIP-7825's execution cap and its code-size limits.
- `test_utils::neutralize_evm(evm, fork)` gives the EVM the two parts of the fork's pricing it carries: Ethereum's precompile set for the fork, not op-revm's Karst set with MegaETH's KZG price, and the fork's static opcode prices.
- SALT pricing needs nothing: without a SALT environment every bucket is minimal.

What stays Satin's is the machinery, and with it two rules no configuration can change: the instruction table's Amsterdam opcodes, and the rules revm gates on the spec id, which is Osaka's.
Both are in the registry.

## How a test is judged

As the revm fork's reference runner judges it, and more strictly where that runner is lenient:

- the post-state root and the logs hash for a transaction that executes;
- for one the fixture expects to be rejected, the exception it names, not any error, and the pre-state left as it was (`src/exceptions.rs` maps each validation error to the names that describe it);
- an expected output must be produced, not merely not contradicted;
- a fixture value that does not fit its width is a failure, not a value clamped to fit.

Base fees go to Optimism's base-fee vault on Satin, where Ethereum burns them.
The runner takes the vault back out of the post-state only when the fee routing made it: the pre-state has no vault, and the vault holds exactly the base fee of the gas the transaction used.

The runner skips what the reference runner skips (`src/skips.rs`), for the same reasons: the EIP-7610 collision fixtures with storage, which revm cannot see, and a transaction that cannot be built when the fixture expects it to be invalid.
So the two runners execute the same population, and the executed and skipped counts equal the reference runner's.

## Running it

```bash
# The fixture releases the revm fork's `scripts/run-tests.sh` names at the pinned tag.
cargo run --release -p state-test -- --fork Osaka <main>/state_tests
cargo run --release -p state-test -- --fork Amsterdam <devnet>/state_tests
cargo run --release -p state-test -- --mode satin --fork Osaka <main>/state_tests
```

`--expect-executed`, `--expect-skipped` and `--expect-deviations` turn a full run into the pinned gate CI runs (`.github/workflows/exec-spec-satin.yml`).
`--json-outcome` prints one JSON line per test, and `--trace` runs each test under an EIP-3155 tracer.

A deviation added or moved updates `src/deviations.rs` and regenerates [`DEVIATIONS.md`](DEVIATIONS.md) with `UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations`.
