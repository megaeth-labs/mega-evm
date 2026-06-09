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

`manifest.json` lists the cases. Each is one transaction chosen for a distinct
workload shape (system-contract interception, plain compute, log-heavy metering,
heavy multi-call). Captures live in `captures/` and are replayed offline, so the
benchmark is deterministic and needs no network in CI.

Every case records `expected_gas`; the driver fails if a binary replays it with
different gas (that would mean different work is being timed).

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

1. Pick a transaction with a workload not already covered.
2. Capture it (step 1 above) into `captures/<name>.cache.json`.
3. Add an entry to `manifest.json` with its `tx`, `expected_gas`, `category`, and
   a `note` describing why it is interesting.
4. Verify: `python3 bench/replay/run.py --bin pr=target/release/mega-evme`.

## CI

`.github/workflows/replay-bench.yml` runs weekly, on manual dispatch, and on a
member `/benchmark-replay` PR comment. It builds the PR and merge-base binaries,
runs the driver, and posts the comparison table (job summary + PR comment).

Timing on shared runners is noisy for sub-10µs cases; read the larger cases and
the overall trend across the corpus for real signal.
