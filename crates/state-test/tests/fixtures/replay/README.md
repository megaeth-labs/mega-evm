# Replay fixture corpus

A committed set of self-validating EEST state tests, each minted from a real
MegaETH on-chain transaction by `mega-evme replay --dump-fixture`.

Each fixture is fully self-contained: it carries the pre-state read closure, the
block environment, the transaction, the MegaETH external environment (`megaEnv`),
and the expected `post` (state root, logs root, `megaGasUsed`, `megaStatus`).
Re-running a fixture through `state-test` re-executes the transaction in
isolation and checks those expectations — **no RPC access**, so it is
deterministic and runs in CI.

At dump time the tool enforces an on-chain fidelity gate: the local replay must
reproduce the transaction's `gasUsed` from its on-chain receipt exactly, or the
fixture is not written. So every fixture here is faithful to mainnet by
construction.

## What CI does with these

- **Correctness guard:** `cargo test --workspace` runs the `replay_corpus`
  integration test (and the dedicated `Replay Fixtures` workflow runs
  `state-test` over this directory). A failure means a code change altered
  execution — gas, status, or post-state — for the covered spec.
- **Performance (future):** the same fixtures feed an ABBA `--bench-runs`
  comparison of baseline vs. PR throughput (Mgas/s).

## Current coverage

| fixture                  | spec | gas     | notes                          |
| ------------------------ | ---- | ------- | ------------------------------ |
| `rex5_03687b32367d.json` | Rex5 | 75,514  | plain contract call            |
| `rex5_1ad7b95987d2.json` | Rex5 | 81,740  | Oracle system contract call    |
| `rex5_945e3fafdcb3.json` | Rex5 | 726,870 | large contract call            |

Only Rex5 is covered so far: the available RPC endpoint prunes historical state,
so Rex3/Rex4 transactions cannot be replayed there. Add them once an archive
endpoint is available.

## Adding a fixture

```bash
mega-evme replay --rpc <archive-url> \
  --dump-fixture crates/state-test/tests/fixtures/replay/<spec>_<txprefix>.json \
  <tx-hash>
# then validate locally:
cargo run -p state-test -- crates/state-test/tests/fixtures/replay
```
