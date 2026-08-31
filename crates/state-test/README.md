# MegaEVM state test

The `state-test` CLI is a thin front-end over the `mega-state-test` runner library (`crates/mega-state-test`), which is a fork of `revme` in the official `revm` repository. 

The changes made to run `execution-specification-tests` on MegaEVM is: 
- `MegaTransaction`'s `enveloped_tx` is always set to `Some(vec![].into())` so that there is no L1 data fee induced. 
- State changes to the `BaseFeeVault` (`0x4200000000000000000000000000000000000019`) are pruned after transaction execution. 
- The EVM spec of all Ethereum's official test cases are forced to be `MegaSpecId::EQUIVALENCE`, which is equivalent to `SpecId::PRAGUE`. 

## Modes

Every mode operates on self-contained EEST fixtures (`TestUnit { env, pre, transaction, post, out }`); none need a network.

- **Validate** (default) — `state-test <paths>` executes each fixture and checks its recorded `post` (state root, logs root, gas, status). This is how the official Ethereum tests and the replay corpus (`bench/replay/fixtures/`, via `replay_corpus.rs`) are checked.
- **`--bench`** — `state-test --bench [--bench-runs N] [--bench-warmup W] [--bench-spec SPEC] <paths>` times each fixture's isolated EVM execution and prints `{ gas_used, success, bench: { min/median/mean, mgasPerSec } }` as JSON instead of validating. This is the only EVM-throughput benchmark entry point; the replay-throughput benchmark (`bench/replay/run.py`) drives it.
- **`--fill`** — `state-test --fill --bench-spec SPEC <paths>` computes each fixture's `post` and writes it back in place (atomically, via a temp file). This is the offline analog of `mega-evme replay --dump-fixture`'s post-fill step, for a fixture that has no on-chain origin (a hand-built case, or a `prestateTracer` snapshot such as `bench/replay/fixtures/attack_deploy.json`). After filling, the fixture is self-validating like any dumped one. A fixture that already has a non-empty `post` is refused unless `--force` is passed — filling replaces the whole `post` map with circularly-derived expectations, so an accidental run against real expectations (e.g. the official test suites) would destroy them. Filenames on the validation skip list and the Constantinople spec are refused outright, since validation would never check the result.
- **`--diff-spec`** — `state-test --bench-spec TARGET --diff-spec BASE <paths>` executes each fixture under both specs and classifies how they differ. Nothing is written and no recorded `post` is consulted: the two executions are compared against each other. This is the only check available for a spec nobody has computed expectations for yet — the frozen spec it inherits from is the oracle, and that spec's precision invariant is what says when the two are allowed to disagree. See `crates/mega-state-test/src/diff.rs` for the classification and `tools/eest-sweep/` for the corpus driver built on it.
- **`--keep-going`** — with `--fill`, records each unit's failure (or panic) and carries on with the rest of its file instead of aborting at the first one, then prints a `Fill tally:` line. Without it, one bad unit ends the whole run, which is why a corpus sweep used to have to split every multi-unit fixture into one file per unit first.

`--bench-spec` selects the spec to run under; without it, the fixture's single `post` spec is used (so `--fill` needs it when the `post` is still empty, and `--diff-spec` requires it outright).