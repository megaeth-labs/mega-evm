# state-test

The command-line front end of the `mega-state-test` runner (`crates/mega-state-test`): it runs Ethereum's execution-spec state-test fixtures through the Satin engine.

```bash
state-test [--mode equivalence|satin] --fork Osaka|Amsterdam [options] <paths>...
```

- `--mode equivalence` (the default) is the gate: it exits non-zero when a failure is not explained by a registered deviation, when a fixture file cannot be read, or when a count differs from the pin it is given.
- `--mode satin` is the report: it runs the same entries under Satin's own configuration, prints the counts by outcome, and exits zero unless a fixture file cannot be read.
- `--fork` names the fork whose fixture entries run.
- `--expect-executed N`, `--expect-skipped N` and `--expect-deviations` pin the counts of a full run; CI passes all three.
- `--summary-json FILE` writes the counts as JSON, `--json-outcome` prints one JSON line per test on standard error, `--trace` runs every test under an EIP-3155 tracer and `--threads` sets the worker count.

What the two modes are, how a test is judged and the deviation registry are described in the `mega-state-test` crate's `README.md` and `DEVIATIONS.md`.
