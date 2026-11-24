# state-test

The command-line front end of the `mega-state-test` runner (`crates/mega-state-test`): it runs Ethereum's execution-spec state-test fixtures through the Satin engine.

```bash
state-test [--mode equivalence|satin] --fork Osaka|Amsterdam [options] <paths>...
```

- `--mode equivalence` (the default) is the gate: it exits non-zero when a failure is not explained by a registered deviation, when a fixture file cannot be read, or when a count differs from the pin it is given.
- `--mode satin` is the report: it runs the same entries under Satin's own configuration, prints the counts by outcome, and exits zero unless a fixture file cannot be read.
- `--fork` names the fork whose fixture entries run.
- `--expect-executed N`, `--expect-skipped N` and `--expect-deviations` pin the counts of a full run; CI passes all three.
- `--json-digest` prints one JSON line per executed transaction on standard error: the test's identity and the hash of the canonical record of the transaction's whole Mega outcome.
  `--json-digest-full` adds the record itself to each line.
  The two make a whole-outcome differential: run both at two revisions, sort and compare (`sort | cmp`), and read the full lines of the ids that differ.
- `--json-witness` prints, beside each digest line and for the same tests, one JSON line with the test's identity and the hash of the canonical record of everything the transaction read below the journal: the database's accounts, codes, storage slots and block hashes, and the SALT buckets and Oracle slots the external environment answered.
  `--json-witness-full` adds the record itself to each line.
  It is the read-set differential: a change that keeps every outcome and reads other state moves it, and a run that asks for both the digest and the witness prints both in one pass, the digest lines unchanged.
- `--limit-data-size BYTES`, `--limit-frame-data-size BYTES`, `--limit-kv-updates RECORDS`, `--limit-state-growth UNITS` and `--limit-compute-gas GAS` (Satin mode only) hold every transaction to tighter per-transaction limits than the spec's, so the limit-stop paths run across the corpus; the frame budget follows `--limit-data-size`.
  The verdicts of such a run are expected to fail; it is for comparing digests.
- `--summary-json FILE` writes the counts as JSON, `--json-outcome` prints one JSON line per test on standard error, `--trace` runs every test under an EIP-3155 tracer and `--threads` sets the worker count.

```bash
state-test btest [options] <paths>...
```

`btest` imports the execution-spec blockchain tests through Satin's block executor: the Osaka tests of the main release's `blockchain_tests`.

- It exits non-zero when a test fails — a block differs from its header without the exact outcome a registered deviation lists for it, or a listed test does not deviate as listed — when a fixture file cannot be read, or when a count differs from the pin it is given.
- `--expect-executed N` pins the tests executed, `--expect-skipped REASON=N` the tests skipped for one reason (repeated for each; once one is given, every reason not given is pinned at zero), and `--expect-deviations` requires every blockchain test a registered deviation lists to deviate exactly as listed; CI passes all three.
- `--summary-json FILE`, `--json-outcome` and `--threads` work as for the state tests; the summary gives each skip class's reason beside its count, and the JSON summary lists every class's reason under `skip_reasons`.
- `--json-witness` prints one JSON line per imported block on standard error: the test, the block's position, whether it was accepted or refused, and the hash of the canonical record of everything the block read below the journal; `--json-witness-full` adds the record.

What the two modes are, how a state test and a blockchain test are judged and the deviation registry are described in the `mega-state-test` crate's `README.md` and `DEVIATIONS.md`.
