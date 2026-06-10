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
- **`--fill`** — `state-test --fill --bench-spec SPEC <paths>` computes each fixture's `post` and writes it back in place. This is the offline analog of `mega-evme replay --dump-fixture`'s post-fill step, for a fixture that has no on-chain origin (a hand-built case, or a `prestateTracer` snapshot such as `bench/replay/fixtures/attack_deploy.json`). After filling, the fixture is self-validating like any dumped one.

`--bench-spec` selects the spec to run under; without it, the fixture's single `post` spec is used (so `--fill` needs it when the `post` is still empty).