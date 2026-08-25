---
description: Load prestate, override balances and storage, fork from RPC, and dump post-execution state.
---

# State Management

`mega-evme` lets you control the EVM's initial account state, override balances and storage, fork live chain state, and capture the post-execution state for inspection or reuse.
These options are available in the `run` and `tx` commands.

## Loading Initial State

### Prestate Files

A prestate file is a JSON document that seeds the EVM with account state before execution begins.
Use `--prestate` (alias `--pre-state`) to load one:

```bash
mega-evme run --prestate ./state.json ...
```

The file maps addresses to account objects.
See [State File Format](#state-file-format) for the exact schema.

### Sender Balance

`--sender.balance` (alias `--from.balance`) sets the balance of the transaction sender directly, without needing a full prestate file.
It accepts human-friendly suffixes:

```bash
mega-evme run --sender.balance 1ether ...
mega-evme run --sender.balance 100gwei ...
mega-evme run --sender.balance 1000wei ...
```

This is a shortcut for the common case of funding the sender before a test run.

### Faucet

`--faucet` adds ether on top of an address's existing balance.
The format is `ADDRESS+=VALUE`:

```bash
mega-evme run --faucet 0x4200000000000000000000000000000000000006+=1ether ...
```

The flag is repeatable, so you can fund multiple addresses at once:

```bash
mega-evme run \
  --faucet 0x4200000000000000000000000000000000000006+=10ether \
  --faucet 0x28B7E77f82B25B95953825F1E3eA0E36c1c29861+=5ether \
  ...
```

Ether suffixes (`ether`, `gwei`, `wei`) are supported.

### Balance Overrides

`--balance` sets (replaces) an address's balance to an exact value.
The format is `ADDRESS=VALUE`:

```bash
mega-evme run --balance 0x4200000000000000000000000000000000000006=50ether ...
```

Unlike `--faucet`, this overwrites whatever balance the address already has.
The flag is repeatable and supports ether suffixes.

**Faucet vs balance at a glance:**

| Flag                      | Effect                         |
| ------------------------- | ------------------------------ |
| `--faucet ADDRESS+=VALUE` | Adds VALUE to existing balance |
| `--balance ADDRESS=VALUE` | Sets balance to exactly VALUE  |

### Storage Overrides

`--storage` sets a specific storage slot on an address before execution.
The format is `ADDRESS:SLOT=VALUE`:

```bash
# Override WETH slot 0 (total supply)
mega-evme run --storage 0x4200000000000000000000000000000000000006:0x0=0x1 ...
```

The flag is repeatable, so you can override multiple slots across multiple contracts:

```bash
mega-evme run \
  --storage 0x4200000000000000000000000000000000000006:0x0=0x1 \
  --storage 0x28B7E77f82B25B95953825F1E3eA0E36c1c29861:0x0=0x1 \
  ...
```

### Block Hash Overrides

`--block-hash` (aliases `--blockhash`, `--block-hashes`, `--blockhashes`) controls what the `BLOCKHASH` opcode returns for a given block number.
The format is `BLOCK_NUMBER:BLOCK_HASH`:

```bash
# Replace 0xabc123... with a real block hash
mega-evme run --block-hash 100:0xabc123... ...
```

The flag is repeatable for multiple block numbers:

```bash
# Replace each hash with real block hashes from your target chain
mega-evme run \
  --block-hash 99:0xabc123... \
  --block-hash 100:0xdef456... \
  ...
```

This is useful when testing contracts that branch on historical block hashes.

## Fork Mode

Fork mode fetches account state from a live RPC endpoint instead of starting from an empty state.
Enable it with `--fork`, which requires `--rpc`:

```bash
mega-evme run --fork --rpc https://mainnet.megaeth.com/rpc ...
```

There is no default endpoint, and the `RPC_URL` environment variable is not consulted — `--rpc` must be passed explicitly.
Set the endpoint with `--rpc` (aliases `--rpc-url`, `--fork.rpc`) and pin a specific block with `--fork.block`:

```bash
mega-evme run \
  --fork \
  --rpc https://mainnet.megaeth.com/rpc \
  --fork.block 21000000 \
  --sender.balance 1ether \
  ...
```

All local override flags (`--prestate`, `--faucet`, `--balance`, `--storage`) apply on top of the forked state.
This lets you layer local modifications over a real chain snapshot without modifying the remote node.

## Dumping State

`--dump` prints the full post-execution account state to stdout after the run completes.
Use `--dump.output` to write it to a file instead:

```bash
# Print to console
mega-evme run --dump ...

# Write to file
mega-evme run --dump --dump.output ./post-state.json ...
```

The output uses the same JSON format as prestate files, so you can feed it directly back into `--prestate`.

An address the run only observed as nonexistent — read, or touched without ever gaining balance, nonce, or code — is omitted entirely: no account exists on either side of the commit, so there is nothing to describe, and a round-tripped prestate answers `EXTCODEHASH` for it with zero exactly as the chain does.
An address that `SELFDESTRUCT` erased during the run is reported as the marker `{"selfdestructed": true}` and nothing else.
Its code and storage still answer for the rest of that transaction, but none of it survives the commit, so printing the account as if it were live would describe a state no later run can observe.
See [Destroyed Accounts](#destroyed-accounts) for how such an entry is read back.

## State File Format

### Fields

Each top-level key is a checksummed or lowercase hex address.
The value is an account object with these fields:

| Field            | Type         | Description                                                                   |
| ---------------- | ------------ | ----------------------------------------------------------------------------- |
| `balance`        | hex quantity | Account balance in wei                                                        |
| `nonce`          | hex quantity | Transaction nonce                                                             |
| `code`           | hex bytes    | Deployed bytecode (`0x` for EOAs)                                             |
| `codeHash`       | hex bytes    | Keccak256 hash of `code`                                                      |
| `storage`        | object       | Map of storage slot to value                                                  |
| `selfdestructed` | boolean      | Written only for an address `SELFDESTRUCT` erased, and then as its only field |

Example:

```json
{
  "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045": {
    "balance": "0xde0b6b3a7640000",
    "nonce": "0x1",
    "code": "0x",
    "codeHash": "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
    "storage": {}
  },
  "0x1234567890abcdef1234567890abcdef12345678": {
    "balance": "0x0",
    "nonce": "0x0",
    "code": "0x6080604052...",
    "codeHash": "0xabc123...",
    "storage": {
      "0x0000000000000000000000000000000000000000000000000000000000000000": "0x1",
      "0x0000000000000000000000000000000000000000000000000000000000000001": "0xdeadbeef"
    }
  }
}
```

### Destroyed Accounts

An address that `SELFDESTRUCT` erased has no state left to describe — balance, nonce, code, and storage are all dropped when the state is committed — so it carries the marker instead of an account body:

```json
{
  "0xa16e02e87b7454126e5e10d957a927a7f5b5d2be": {
    "selfdestructed": true
  }
}
```

Loading a file that contains such an entry treats the address as absent rather than seeding an account for it, which is exactly the world the destroying transaction committed.
A dump therefore stays safe to feed straight back into `--prestate`.
Only the value `true` carries this meaning; an entry that spells the field out as `false` is an ordinary account and is loaded as written.

### Format Notes

All numeric values use Ethereum quantity encoding:

- `0x` prefix is required.
- No leading zeros, except `0x0` for zero.
- `balance` is in wei.
- Storage keys and values are 32-byte hex strings (zero-padded to 64 hex characters).

## RPC Cache and Retry

When using fork mode, `mega-evme` caches RPC responses to avoid redundant network calls and supports configurable retry behavior for resilience against transient failures.

These options take effect only when an RPC endpoint is actually contacted: on `run` and `tx` that requires `--fork` (without `--fork` they are accepted but silently ignored); on `replay` they apply to its `--rpc` fetches (an offline `--rpc.replay-file` run contacts no endpoint and does not use them).

The cache sits at the transport, below the provider, so it sees every single JSON-RPC request the run issues — block bodies included — rather than a fixed list of methods.
Batched JSON-RPC requests are the exception: they are forwarded as-is, neither read from nor written to the cache.
The chain-id probe that names the cache file is issued on a provider without the cache, so a cache file can never confirm its own chain identity.

Two kinds of response are never cached by either writer:

- A JSON-RPC `error` body — a transient rate limit surfaced as an `error` rather than an HTTP status would otherwise be replayed as a permanent failure.
- A `"result": null` — a briefly invisible transaction would otherwise freeze as "not found".

Three more hold only for the moment they were taken, and the per-chain online cache drops them so a later run does not inherit that moment:

- `eth_blockNumber`, whose result _is_ the chain tip.
- Any request whose params name a moving block (`latest`, `pending`, `earliest`, `safe`, `finalized`) at any depth, including inside an `eth_getLogs` filter.
- `eth_getTransactionByHash` metadata for a transaction with a null `blockNumber` or `blockHash`, which is what every consumer reads as "still pending".

The caller always gets the response; only the cache declines it, so the next identical request goes back to the endpoint.
Everything else — mined transactions, numbered blocks, receipts, state reads at a numbered height — is persisted as final: MegaETH's single-sequencer blocks do not reorg once served, so a recorded answer stays canonical.
On an endpoint whose recent history can still change, that assumption does not hold, and a cache recorded before a reorg keeps answering from the orphaned view until it is discarded; `--rpc.clear-cache` does that, and `--rpc.no-cache-file` keeps a run off the disk cache entirely.
Capture fixtures (`--rpc.capture-file`) keep these three deliberately: an offline rerun of a pending transaction has to be answered from the fixture, and dropping them would make it unusable for exactly that case.
See [RPC Cache File](../commands/replay.md#rpc-cache-file) for capture and offline replay.

### Per-Chain Cache Files

Each chain gets its own cache file named `rpc-cache-{chain_id}.json` inside the cache directory.
The per-chain filename makes cross-chain contamination impossible by construction: a cache populated from mainnet physically cannot be loaded during a testnet run.
The file body records the chain id too, and it is checked against the chain the endpoint reports before a single entry is adopted.
A body that disagrees with the endpoint is a hard error (exit `1`) naming both chains: the filename already separates chains, so disagreement inside means the file was swapped or the directory is shared across chains, and continuing would bury that behind a persist that swallows its own failures.

The body is a cache envelope — the `{version, chain_id, cache, external_env}` shape `--rpc.capture-file` also writes — plus a `kind: "cache"` marker identifying it as this cache's own file.
What is found at the path therefore falls into one of three cases:

- **This cache's file**, at the current version and for this chain: entries are loaded, and clean-exit persist merges into it.
- **Someone else's file** — a capture fixture, an envelope whose identity fields this build cannot read, a JSON shape neither writer produces, or a file that cannot be read at all: never loaded and never overwritten. The run warns, starts with an empty cache, and skips its persist, so a mispointed `--rpc.cache-dir` cannot destroy the file it was pointed at.
- **Unusable at this path** — not valid JSON, an envelope `version` this build does not write, undecodable entries, or the bare JSON array an older `mega-evme` wrote: removed and rebuilt from scratch, with one warning. A cache file is a regenerable artifact, so upgrading heals itself instead of failing the first run; an old array cannot be converted, because it stores only hashed request keys that this build hashes differently.

The default cache directory is the platform cache directory:

- **Linux**: `$XDG_CACHE_HOME/mega-evme/rpc`
- **macOS**: `~/Library/Caches/mega-evme/rpc`

Batch replay (`--tx-file` / `--block`) is the exception: it engages the on-disk cache only when the invocation asks for it explicitly, and otherwise behaves as if `--rpc.no-cache-file` were set.
A batch scan walks linear history whose request keys essentially never repeat across runs, so the file buys almost no hits, while its clean-exit persist re-reads, merges, and rewrites the whole file under the cross-process lock — a cost that grows with the file and serializes concurrent batch processes.

Two flags ask for it: `--rpc.cache-dir`, which names the file to use, and `--rpc.clear-cache`, which asks for that file to be deleted.
Clearing only means something while the disk cache is engaged, so a batch run that forced the cache off would parse the recovery flag, do nothing, and leave the polluted file in place for the next run.
With `--rpc.clear-cache`, a batch run deletes the cache file under the sidecar lock, starts from an empty cache, and persists on exit — the same sequence as single-transaction mode.
A clear that fails locally — the sidecar lock cannot be acquired, or the file cannot be unlinked — is an execution-class failure (exit `1`), not an RPC failure: retrying or switching the endpoint cannot fix the local filesystem.
An explicit `--rpc.no-cache-file` still wins over both flags.

### Concurrent cache-dir sharing

Multiple `mega-evme` processes may share the same `--rpc.cache-dir` safely.
On clean-exit persist, each process:

1. Takes an exclusive advisory lock on a sidecar file next to the cache (`rpc-cache-{chain_id}.json.lock`).
2. Re-reads the on-disk cache (a sibling process may have written since this process loaded).
3. Merges its in-memory entries over the on-disk ones (same key → this process's value wins).
4. Writes the result via a temp file and atomic rename, then releases the lock.

Both writers — the per-chain online cache and `--rpc.capture-file` — follow that sequence; the file they write is the same envelope, and what differs is how each one treats a failure.

The lock sidecar is left in place after the process exits; only the flock is released when the handle closes.
Lock contention blocks for a short critical section rather than failing the finished run.
If the lock cannot be acquired at all (for example the directory is not writable), persist fails closed and writes nothing.
An unlocked write is not offered as a fallback: it is exactly the lost-update race the lock exists to prevent, and it would delete a sibling process's entries silently.

For the online cache, persist is best-effort: a lock or write failure leaves the file untouched and warns, and the cost is a re-fetch on the next run.
The re-read under the lock classifies the target exactly as load does, so a file that turned out to be someone else's is left intact and the persist is skipped, while an unusable one is replaced by this process's entries.
The union of on-disk and in-memory entries is also held to `--rpc.cache-max-entries`, so two processes sharing one file cannot together keep more than either was allowed to.

A capture that cannot take the lock fails the run rather than writing unlocked: the envelope is the primary output of capture mode, so losing a concurrent writer's entries is worse than reporting the failure.
Capture adds hard-error checks before writing:

- The on-disk envelope `version` and `chain_id` must match this process's capture.
- `external_env` uses optimistic concurrency against the snapshot observed when this process opened the capture file:
  - If the locked on-disk snapshot is absent, or still equals the load-time snapshot, the caller's intentional update wins (so a sequential refresh with `--bucket-capacity` on an existing capture is accepted).
  - A run whose snapshot still equals the load-time one has not changed anything, whether it omitted `--bucket-capacity` or passed the values already in force: if another writer refreshed the on-disk snapshot meanwhile, that refresh is kept and this run's cache entries still merge.
    Re-asserting the current values is therefore not a way to defend them against a concurrent refresh.
  - Persist hard-errors only when this process changed the snapshot **and** the on-disk snapshot also changed since load, to a different value (true concurrent conflict).
    The error names all three values: loaded, ours, and on-disk.
  - Snapshots are canonicalized before comparison and write (deduplicate by bucket id with last-wins, then sort by id), so two workers with the same effective capacities in different CLI order do not conflict.
  - One-sided snapshots still merge: this process's snapshot is kept when set, otherwise the on-disk snapshot is propagated.
- Offline [`cache merge`](../commands/cache.md) still rejects non-identical non-null `external_env` snapshots across inputs (no load-time baseline to compare against).

A corrupt or unreadable on-disk envelope during re-read degrades to writing this process's entries only (warned), while identity/schema failures remain hard errors.

To consolidate cache envelopes offline, use [`cache merge`](../commands/cache.md), which checks chain identity from each envelope body rather than from file names.
Its output carries no `kind` marker, so it is a fixture and not a drop-in per-chain cache file: a run pointed at it warns, leaves it untouched, and starts with an empty cache.
`cache merge` follows the same lock protocol for its `--output`: it takes the output's sidecar lock, folds whatever the file holds at that moment into the union, writes, and releases — so merging into a file a live run is still persisting to loses neither side's entries.

### Cache Flags

| Flag                          | Type  | Default            | Description                                                                                                                                                                                                        |
| ----------------------------- | ----- | ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--rpc.cache-max-entries <N>` | `u32` | `0`                | Maximum number of items in the in-memory RPC LRU cache (and therefore what is persisted to the cache file). `0` = effectively unlimited, capped at 1,048,576 entries. Default.                                     |
| `--rpc.cache-dir <PATH>`      | path  | Platform cache dir | Directory for per-chain cache files. Each chain's cache is stored as `{cache_dir}/rpc-cache-{chain_id}.json`. Batch replay uses the on-disk cache only when this flag or `--rpc.clear-cache` is passed explicitly. |
| `--rpc.no-cache-file`         | flag  | `false`            | Disable on-disk cache persistence. The in-memory LRU cache still applies. Wins over `--rpc.clear-cache`. Already the default for batch replay unless `--rpc.cache-dir` or `--rpc.clear-cache` is passed.           |
| `--rpc.clear-cache`           | flag  | `false`            | Delete the current chain's cache file before loading it. Recovery path for a polluted or corrupt cache. Engages the on-disk cache, including in batch replay. No effect alongside `--rpc.no-cache-file`.           |

The in-memory cache is always installed on a forked or online run and cannot be turned off; `--rpc.no-cache-file` disables only on-disk persistence.
The cap is an eviction threshold, not an allocation: memory grows with the entries the run actually caches, so raising it costs nothing until that many responses arrive, and the default `0` (1,048,576 entries) is a guard rail against a long-running process growing without bound rather than a reserved budget.
Set `--rpc.cache-max-entries` to a smaller value when a run's own working set is the footprint you want to bound — entries beyond the cap are evicted least-recently-used first, and re-fetched if asked for again.

#### Removed Flags

| Removed                | Replacement                   | Note                                                                                                                                                                                                                                                                        |
| ---------------------- | ----------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--rpc.cache-size <N>` | `--rpc.cache-max-entries <N>` | `N > 0` carries over unchanged. `0` inverted meaning — it used to disable the cache, and now means "effectively unlimited" — so the old flag is rejected rather than aliased, and a script passing it fails loudly instead of silently doing the opposite of what it asked. |

### Retry Flags

| Flag                        | Type  | Default | Description                                                                                                                                                                        |
| --------------------------- | ----- | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--rpc.max-retries <N>`     | `u32` | `5`     | Maximum retry attempts for failing RPC requests. Retries on HTTP 429/503, rate-limit errors, and transport failures. `0` to disable.                                               |
| `--rpc.backoff-ms <MS>`     | `u64` | `1000`  | Fixed sleep duration in milliseconds between retry attempts (no exponential backoff).                                                                                              |
| `--rpc.cu-per-sec <CU/s>`   | `u64` | `660`   | Compute-unit budget (CU/s) for the retry layer's rate-limit accounting — not requests per second. Alias: `--rpc.rate-limit`. Values below 100 with retries enabled emit a warning. |
| `--rpc.request-timeout <S>` | `u64` | `30`    | Total per-HTTP-request timeout in seconds (connect + response). `0` disables. A hung endpoint then surfaces as a retryable transport error instead of hanging the process.         |

### Examples

Replay a transaction with a local per-chain cache directory (a warm cache avoids redundant RPC calls on later runs):

```bash
mega-evme replay \
  --rpc https://mainnet.megaeth.com/rpc \
  --rpc.cache-dir ./my-cache \
  0xabc123...
```

The per-chain cache supplements `--rpc`; it does not replace it.
For a fully offline replay, capture a single-file fixture and replay it with `--rpc.replay-file` (see [replay](../commands/replay.md#rpc-cache-file)).

Disable on-disk caching but keep the in-memory LRU:

```bash
mega-evme tx --fork --rpc https://mainnet.megaeth.com/rpc --rpc.no-cache-file ...
```

Clear a cache you no longer trust before replaying:

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --rpc.clear-cache 0xabc123...
```

A cache file left by an older `mega-evme` needs no migration step of its own: the first run of this build reports the file, removes it, and rebuilds the cache from scratch.
`--rpc.clear-cache` is for a file this build can still read but whose contents you want gone.

## Round-Trip Example

Dump state after a first run, tweak it, then replay with the modified state:

```bash
# Step 1: run and capture state
mega-evme run \
  --sender.balance 10ether \
  --dump \
  --dump.output ./state-after.json \
  ...

# Step 2: edit state-after.json as needed (e.g., adjust a storage slot)

# Step 3: reload the modified state
mega-evme run \
  --prestate ./state-after.json \
  ...
```

This pattern is useful for multi-step test scenarios where each run builds on the output of the previous one.
Combine with `--faucet` or `--balance` to patch specific accounts without editing the JSON by hand.
