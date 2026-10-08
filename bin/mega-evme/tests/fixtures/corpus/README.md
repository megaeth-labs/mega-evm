# Mainnet replay corpus

84 `MegaETH` mainnet blocks (chain `4326`), each pinned by number and hash and stored with an offline RPC capture of everything replaying it needs.
`tests/replay_corpus.rs` replays every block with `mega-evme replay --rpc.replay-file <capture> --block N --verify-receipt --verify-block` and requires every receipt verdict and the block verdict to match.
It runs in the normal test suite:

```bash
cargo test -p mega-evme --test replay_corpus
```

The archive is extracted once per test binary, and the blocks then replay concurrently inside one test, which reports every failing block at the end rather than stopping at the first.

## Layout

- `corpus.tar.xz` — one archive holding every capture: the member `<N>.cache.json` is the RPC capture of block `N`.
  Members are stored in block order with fixed metadata (mode `0644`, owner `0`, empty owner names, mtime `0`) in a ustar tar compressed with xz at preset `9` with the extreme flag, single-threaded.
  Packing the same members therefore always yields the same bytes; one solid archive is less than half the size of one archive per block, because the captures share contract code and most of their JSON.
- `manifest.json` — the single source of truth: `chain_id`, the `salt` source of the captures, the `archive` and its `sha256`, and one entry per block with its `number`, `hash`, `spec` (the spec the mainnet schedule assigns to the block's timestamp), `tx_count`, its `member` in the archive, that member's `sha256`, and an optional `note` saying what a notable block covers.

The tests walk the manifest, never the archive: a consistency test requires the archive to match its pinned digest and to hold exactly the manifest's members, in block order, each matching its own pinned digest, and the replay test checks each capture's served block hash and spec against the manifest.

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

The script seeds each block's capture from its member of the archive, and `--rpc.capture-file` is incremental, so only the requests the current `mega-evme` makes and the capture lacks are fetched.
When no selected block's capture changed, `corpus.tar.xz` and `manifest.json` are left untouched.
Otherwise the script repacks the whole archive and updates `manifest.json` in place: the changed members' digests and the archive's digest.

To add a block, pin it with `--pin <N>:<HASH>`; its entry is inserted in block order with a `spec` placeholder (`?`), and the corpus test reports the spec the mainnet schedule assigns to it.

Every recapture or addition rewrites the whole archive, about 4.3 MB, and that version is committed to the repository history for good, which cannot be rewritten.
Recapture only the blocks that need it, and batch recaptures and additions into one change, so the history gains one archive rather than one per block.

## Checking the packer

`--check` repacks the committed archive's members with the script's packer and fails unless the result is byte-identical to the committed archive:

```bash
python3 scripts/replay_corpus_capture.py --check
```

It needs neither an endpoint nor a binary.
The packer writes through the system's liblzma; the committed archive also matches what `xz -9e -T1` (XZ Utils 5.8) produces from the same tar, so a different liblzma that compresses differently shows up here rather than as an unexplained digest change in a recapture.
