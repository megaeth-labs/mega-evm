# Replay throughput benchmark

Measures how fast `mega-evm` executes **real, characteristic MegaETH mainnet
transactions**, and compares a PR against its merge-base so a change that speeds
up or slows down real-transaction execution is visible.

This is the real-transaction counterpart to the synthetic Criterion suite
(`crates/mega-evm/benches/`, run by `.github/workflows/benchmark.yml`). Criterion
answers _"how far is mega from vanilla revm on a crafted workload"_; this answers
_"did this change make real transactions faster or slower"_.

## How it works

1. **Record.** Each case is a real transaction captured once, online, into an
   offline RPC cache that also stores the on-chain receipt:

   ```bash
   mega-evme replay --rpc <url> \
     --rpc.capture-file bench/replay/captures/<name>.cache.json \
     --bench-runs 1 <tx-hash>
   ```

2. **Replay offline.** The driver replays each captured transaction in isolation
   with `mega-evme replay --rpc.replay-file <cap> --bench-runs N --json`, which
   times only the target transaction's EVM `transact` call (no RPC, no preceding
   transactions) and reports `min`/`median`/`mean` and Mgas/s. Replay reproduces
   the on-chain gas, so the same work is timed every run.

3. **Compare.** With two binaries (base and PR) the driver interleaves them
   **ABBA** across several rounds so slow machine drift cancels, takes each
   binary's median, and reports Δ% per transaction. A transaction whose PR median
   is more than the threshold slower is flagged a regression.

## Corpus

`manifest.json` lists the cases. Each is chosen for a distinct workload shape
(system-contract interception, plain compute, log-heavy metering, heavy
multi-call, limit-tracker stress). Every case records `expected_gas`; the driver
fails if a binary runs it with different gas (that would mean different work is
being timed).

There are two case types, both deterministic and network-free:

- **`capture`** (default) — a real mined transaction, replayed in block context
  from a committed RPC capture under `captures/` via `mega-evme replay
  --bench-runs`.
- **`fixture`** — a self-contained state-test fixture under `fixtures/`,
  benchmarked via `state-test --bench`. The fixture carries its own pre-state, so
  **no RPC and no archive node are needed**: any source that can produce a
  state-test (EEST) fixture works — a `mega-evme --dump-fixture` replay, a
  `debug_traceCall` + `prestateTracer` snapshot, or a hand-crafted case. A
  `fixture` case names the `spec` to run under (the fixture need only carry
  `env` / `pre` / `transaction`, plus an empty `post: {}`).

  Example: `fixtures/attack_deploy.json` is the mainnet attack contract
  deployment from the `attack_replay` Criterion bench (#299) — a prestate
  snapshot, not a mined transaction — converted to a fixture. It reproduces the
  source's exact 141,927,106 gas under Rex5, so a non-on-chain workload is
  tracked for regressions through the same driver.

## Running locally

```bash
cargo build --release -p mega-evme

# Measure the current binary:
python3 bench/replay/run.py --bin pr=target/release/mega-evme

# Compare two binaries (e.g. main vs the working tree):
python3 bench/replay/run.py \
  --bin base=/path/to/main/mega-evme \
  --bin pr=target/release/mega-evme \
  --rounds 7 --threshold-pct 7 --fail-on-regression
```

Useful flags: `--runs` / `--warmup` (per-invocation iterations), `--rounds`
(ABBA rounds), `--threshold-pct` (regression cutoff), `--json-out` /
`--markdown-out`, `--fail-on-regression` (non-zero exit on a flagged regression).

## Adding a case

A **mined transaction** (`capture` case):

1. Pick a transaction with a workload not already covered.
2. Capture it (step 1 above) into `captures/<name>.cache.json`.
3. Add an entry to `manifest.json` with its `tx`, `expected_gas`, `category`, and
   a `note`.

A **non-on-chain workload** (`fixture` case — e.g. an attack/edge case from a
`prestateTracer` snapshot or a hand-crafted scenario):

1. Produce a state-test fixture JSON (`env` + `pre` + `transaction` + `post: {}`)
   into `fixtures/<name>.json`. A `mega-evme replay --dump-fixture` output already
   has this shape; a `prestateTracer` snapshot maps directly (`prestate` → `pre`,
   tx fields → `transaction`, block → `env`).
2. Add an entry with `"type": "fixture"`, `"fixture": "fixtures/<name>.json"`,
   the `"spec"` to run under, and `expected_gas`.

Verify either with `python3 bench/replay/run.py --bin pr=target/release/mega-evme`
(the driver finds `state-test` next to `mega-evme`).

## CI

`.github/workflows/replay-bench.yml` runs weekly, on manual dispatch, and on a
member `/benchmark-replay` PR comment. It builds the PR and merge-base binaries,
runs the driver, and posts the comparison table (job summary + PR comment).

Timing on shared runners is noisy for sub-10µs cases; read the larger cases and
the overall trend across the corpus for real signal.
