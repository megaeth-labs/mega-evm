# Replay corpus & throughput benchmark

`fixtures/` is a single corpus of characteristic MegaETH workloads, used two ways:

- **Correctness** — `crates/state-test/tests/replay_corpus.rs` re-executes each
  fixture and checks its recorded post-state (state/logs root, gas, status) on
  **every PR** (via `cargo test`). A change that alters execution turns this red.
- **Performance** — `run.py` times each fixture and compares a PR against its
  merge-base (`.github/workflows/replay-bench.yml`, on demand), so a speed-up or
  slow-down on real workloads is visible. This is the real-workload counterpart
  to the synthetic Criterion suite (`crates/mega-evm/benches/`).

Keeping one corpus means a fixture added for either purpose serves both.

## The unit: a self-contained fixture

Every case is one **self-contained state-test fixture** (`fixtures/<name>.json`):
it carries its own pre-state, transaction, block environment, and MegaETH
external environment, so it replays in isolation with **no RPC and no archive
node**. The same EEST `TestUnit` shape `mega-evme replay --dump-fixture`
produces:

```jsonc
{
  "<name>": {
    "env":         { currentNumber, currentTimestamp, currentBaseFee, … },
    "pre":         { "0x…": { balance, code, nonce, storage }, … },  // the prestate
    "transaction": { type, sender, to, data, gasLimit, value, … },
    "megaEnv":     { bucketCapacities, oracleStorage },
    "post":        { "Rex5": [ { hash, logs, megaGasUsed, megaStatus } ] }
  }
}
```

Because the fixture is self-contained, the source of the workload does not
matter — a real mined transaction (`mega-evme replay --dump-fixture`), a
`debug_traceCall` + `prestateTracer` snapshot, or a hand-crafted case all become
the same kind of JSON. The corpus stores each as a committed fixture and times
it with `state-test --bench`.

## How it works

1. **Build the fixture.** For a real transaction, dump it once (offline once
   captured, or straight from RPC):

   ```bash
   mega-evme replay --rpc <url> --dump-fixture bench/replay/fixtures/<name>.json <tx-hash>
   ```

   The dump only writes if the local replay reproduces the on-chain receipt's gas
   and status, so the fixture is faithful by construction. For a non-on-chain
   workload, produce the same `TestUnit` shape by other means (see below).

2. **Replay offline.** The driver times each fixture's isolated EVM `transact`
   call with `state-test --bench`, reporting `min`/`median`/`mean` and Mgas/s.

3. **Compare.** With two binaries (base and PR) the driver interleaves them
   **ABBA** across several rounds so machine drift cancels, takes each binary's
   median, and reports Δ% per case. A case whose PR median is more than the
   threshold slower is flagged a regression.

## Corpus

`manifest.json` lists the bench cases — each chosen for a distinct workload shape
(system-contract interception, plain compute, log-heavy metering, heavy
multi-call, limit-tracker stress). Every case records `expected_gas`; the driver
fails if a binary runs it with different gas (that would mean different work is
being timed). A case names its `spec` explicitly only when its fixture carries an
empty `post`; otherwise the spec is taken from the fixture's `post` key.

The correctness test (`replay_corpus.rs`) globs `fixtures/` directly, so every
fixture with a populated `post` is validated. `fixtures/attack_deploy.json` is a
mainnet attack contract deployment (#299) converted from a `prestateTracer`
snapshot — a non-on-chain, 142M-gas limit-tracker stress case; its `post` was
filled offline with `state-test --fill`, so it too is validated and benched.

## Running locally

```bash
cargo build --release -p mega-evme -p state-test

# Measure the current build:
python3 bench/replay/run.py --bin pr=target/release

# Compare two builds (each --bin is a directory containing mega-evme + state-test,
# or the path to mega-evme with state-test alongside it):
python3 bench/replay/run.py \
  --bin base=/path/to/main/target/release \
  --bin pr=target/release \
  --rounds 7 --threshold-pct 7 --fail-on-regression
```

Useful flags: `--runs` / `--warmup`, `--rounds` (ABBA rounds), `--threshold-pct`,
`--json-out` / `--markdown-out`, `--fail-on-regression`.

## Adding a case

For a **mined transaction**, dump it (step 1 above) into `fixtures/<name>.json`,
then add a `manifest.json` entry with `"type": "fixture"`, the `fixture` path,
`expected_gas`, `category`, and a `note`.

For a **non-on-chain workload** (an attack/edge case from a `prestateTracer`
snapshot or a hand-crafted scenario), produce the `TestUnit` JSON directly
(`env` + `pre` + `transaction` + an empty `post: {}`), then fill its `post` so it
is validated too:

```bash
state-test --fill --bench-spec <SPEC> bench/replay/fixtures/<name>.json
```

Add the `manifest.json` entry as above (no `spec` needed once `post` is filled).

Both kinds end up with a populated `post`, so they are picked up by the per-PR
correctness test automatically — no extra wiring.

Verify with `python3 bench/replay/run.py --bin pr=target/release` (bench) and
`cargo test -p state-test --test replay_corpus` (correctness).

> To benchmark a single transaction ad-hoc, dump it
> (`mega-evme replay --dump-fixture /tmp/x.json <tx>`) and time it
> (`state-test --bench /tmp/x.json`) — no manifest entry needed.

## CI

- **Correctness** runs on every PR: `cargo test` (`build-and-test.yml`) executes
  `replay_corpus.rs`, which validates the recorded post-state of every fixture.
- **Performance** runs on demand: `replay-bench.yml` (weekly / manual dispatch /
  member `/benchmark-replay` PR comment) builds the PR and merge-base binaries,
  runs the driver, and posts the comparison table (job summary + PR comment).

Timing on shared runners is noisy for sub-10µs cases; read the larger cases and
the overall trend across the corpus for real signal.
