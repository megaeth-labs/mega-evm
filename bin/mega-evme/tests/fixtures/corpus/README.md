# Mainnet replay corpus

84 `MegaETH` mainnet blocks (chain `4326`), each pinned by number and hash and stored with an offline RPC capture of everything replaying it needs.
`tests/replay_corpus.rs` replays every block with `mega-evme replay --rpc.replay-file <capture> --block N --verify-receipt --verify-block` and requires every receipt verdict and the block verdict to match.
It runs in the normal test suite:

```bash
cargo test -p mega-evme --test replay_corpus
```

The blocks replay concurrently inside one test, which reports every failing block at the end rather than stopping at the first.

## Layout

- `manifest.json` — the single source of truth: `chain_id`, the `salt` source of the captures, and one entry per block with its `number`, `hash`, `spec` (the spec the mainnet schedule assigns to the block's timestamp), `tx_count`, `archive`, the archive's `sha256`, and an optional `note` saying what a notable block covers.
- `<N>.cache.json.tar.xz` — a tar holding exactly `<N>.cache.json`, the RPC capture of block `N`, compressed with `xz -9e`.

The tests walk the manifest, never the directory: a consistency test requires the directory to hold exactly the archives the manifest lists, each matching its pinned digest, and the replay test checks each capture's served block hash and spec against the manifest.

## Where the data came from

Every capture was recorded from a mainnet archive RPC endpoint by `mega-evme` itself (`--rpc.capture-file`), while replaying its block with `--verify-receipt`.
The block was then replayed again offline from the capture alone, and both runs had to verify every receipt before the capture was kept.
`scripts/replay_corpus_capture.py` performs exactly those steps and additionally requires `--verify-block` to match.

## What the corpus covers

Between them the 84 blocks cover:

- every spec rung mainnet has executed: `MiniRex`, the `MiniRex1` and `MiniRex2` alias rungs, `Rex`, and `Rex1` through `Rex6`;
- the `Rex2`, `Rex5` and `Rex6` activation blocks, the first block each of those specs executed;
- the transaction types deposit, legacy, EIP-2930 (block 10206729 is the only one with type-1 transactions), EIP-1559 and EIP-7702;
- contract creations that succeed (EIP-1559) and that fail (legacy, EIP-1559 and deposit), failed deposits, and a user deposit beside the L1 attributes deposit;
- large executions: transactions burning up to 3.29 billion gas, blocks above 320 million gas, and a block of 162 transactions.

The manifest's `note` field marks the blocks that cover a shape few others do.

## SALT bucket capacities

The captures carry no SALT bucket capacities (`"salt": "default-minimum"`), so every bucket replays at the minimum capacity.
Each block still reproduces its receipts and its header under it, which is what the test checks.

## Recapturing a block

A maintainer with access to a mainnet archive RPC endpoint recaptures with the capture script, from the repository root:

```bash
cargo build --release -p mega-evme
RPC_URL=<endpoint> python3 scripts/replay_corpus_capture.py \
  --bin target/release/mega-evme --blocks <N> [<N> ...]
```

The script starts from the block's existing capture, and `--rpc.capture-file` is incremental, so only the requests the current `mega-evme` makes and the capture lacks are fetched.
When nothing new was needed the archive is left untouched; otherwise the script rewrites it and prints the block's manifest entry with the new digest, to be pasted into `manifest.json`.

Recapture only the blocks that need it.
Every archive written here is committed to the repository history for good, and that history cannot be rewritten, so a needless recapture costs its full size forever.

To add a block, pin it with `--pin <N>:<HASH>`; its printed entry carries a `spec` placeholder, and the corpus test reports the spec the mainnet schedule assigns to it.
