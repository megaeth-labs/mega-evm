# Mainnet replay corpus

The `MegaETH` mainnet blocks (chain `4326`) `manifest.json` lists, each pinned by number and hash and stored with an offline RPC capture of everything replaying it needs.
`tests/replay_corpus.rs` replays every block with `mega-evme replay --rpc.replay-file <capture> --block N --verify-receipt --verify-block` and requires every receipt verdict and the block verdict to match.
It runs in the normal test suite:

```bash
cargo test -p mega-evme --test replay_corpus
```

The archive is extracted once per test binary, and the blocks then replay concurrently inside one test, which reports every failing block at the end rather than stopping at the first.
Block 6780842, with 21,153 transactions, dominates the run: it replays on one thread and takes over a minute in a debug build, while the other blocks finish within seconds.

## Layout

- `corpus.tar.xz` — one archive holding every capture: the member `<N>.cache.json` is the RPC capture of block `N`.
  Members are stored in block order with fixed metadata (mode `0644`, owner `0`, empty owner names, mtime `0`) in a ustar tar compressed with xz at preset `9` with the extreme flag, single-threaded.
  Packing the same members therefore always yields the same bytes; one solid archive is about a fifth smaller than one archive per block, because the captures share contract code and most of their JSON.
- `manifest.json` — the single source of truth: `chain_id`, the `salt` source of the captures, the `archive` and its `sha256`, and one entry per block with its `number`, `hash`, `spec` (the spec the mainnet schedule assigns to the block's timestamp), `tx_count`, its `member` in the archive, that member's `sha256`, and an optional `note` saying what a notable block covers.

Neither file ships in the published `mega-evme` crate: its `Cargo.toml` excludes the corpus and its test, because the archive alone is larger than crates.io accepts for a crate.

The tests walk the manifest, never the archive: a consistency test requires the archive to match its pinned digest and to hold exactly the manifest's members, in block order, each matching its own pinned digest, and the replay test checks each capture's served block hash and spec against the manifest.

## How the blocks were chosen

The block list is the mainnet fixture set of [stateless-validator](https://github.com/megaeth-labs/stateless-validator) (`test_data/mainnet/manifest.txt`), so both repositories replay the same blocks.
That set is a coverage cover: stateless-validator's `coverage-replayer` replays mainnet history under one build, records which coverage items of mega-evm, revm's execution crates and op-revm each block reaches, and selects a small set of blocks that together reach every item it observed.
The current list is the cover computed under mega-evm `f78e75c` with revm 40, from [stateless-validator#233](https://github.com/megaeth-labs/stateless-validator/pull/233).
The cover measures those crates, not `mega-evme` itself; the shapes listed under [What the corpus covers](#what-the-corpus-covers) are what the selected blocks happen to contain.
When stateless-validator regenerates its cover, this list can follow it again.

## Where the data came from

Every capture was recorded from a mainnet archive RPC endpoint by `mega-evme` itself (`--rpc.capture-file`), while replaying its block with `--verify-receipt`.
The block was then replayed again offline from the capture alone, and both runs had to verify every receipt before the capture was kept.
`scripts/replay_corpus_capture.py` performs exactly those steps and additionally requires `--verify-block` to match.
The current archive was rebuilt from an empty corpus: every block was pinned with `--pin` and captured fresh against a mainnet archive node, so no capture carries requests that only an earlier `mega-evme` made.

## What the corpus covers

Between them the blocks cover:

- every spec rung mainnet has executed: `MiniRex`, the `MiniRex1` and `MiniRex2` alias rungs, `Rex`, and `Rex1` through `Rex6`;
- the `Rex2`, `Rex5` and `Rex6` activation blocks, the first block each of those specs executed;
- the transaction types deposit, legacy, EIP-2930 (a single type-1 transaction, in block 6780842), EIP-1559 and EIP-7702 (under `MiniRex`, `Rex4`, `Rex5` and `Rex6`, including a failed one and one carrying three authorizations);
- contract creations that succeed (EIP-1559, in block 8652636 only) and that fail (EIP-1559 and deposit), failed deposits, and user deposits beside the L1 attributes deposit;
- large executions: transactions burning up to 3.29 billion gas, blocks above 320 million gas, and a block of 21,153 transactions using 2.06 billion gas.

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

The script seeds each block's capture from its member of the archive, and `--rpc.capture-file` is incremental, so only the requests the current `mega-evme` makes and the capture lacks are fetched.
When no selected block's capture changed, `corpus.tar.xz` and `manifest.json` are left untouched.
Otherwise the script repacks the whole archive and updates `manifest.json` in place: the changed members' digests and the archive's digest.
Both files are written in full beside their targets before either is replaced, and the archive is renamed into place before the manifest, so a run that fails or is interrupted can only stop between the two renames.
Every run, `--check` included, first finishes such an update from the manifest it left staged as `manifest.json.tmp`, and then requires the manifest to describe the archive exactly: its digest, its members, and each member's digest.
Any other drift fails the run, naming each difference; restore both files from version control and rerun the capture, since the script never re-pins the manifest from an archive it cannot vouch for.

To add a block, pin it with `--pin <N>:<HASH>`; its entry is inserted in block order with a `spec` placeholder (`?`), and the corpus test reports the spec the mainnet schedule assigns to it.

Every recapture or addition rewrites the whole archive, about 12 MB, and that version is committed to the repository history for good, which cannot be rewritten.
Recapture only the blocks that need it, and batch recaptures and additions into one change, so the history gains one archive rather than one per block.

## Checking the packer

`--check` verifies that the manifest describes the archive, then repacks the committed archive's members with the script's packer and fails unless the result is byte-identical to the committed archive:

```bash
python3 scripts/replay_corpus_capture.py --check
```

It needs neither an endpoint nor a binary.
The packer writes through the system's liblzma; the committed archive also matches what `xz -9e -T1` (XZ Utils 5.8) produces from the same tar, so a different liblzma that compresses differently shows up here rather than as an unexplained digest change in a recapture.
