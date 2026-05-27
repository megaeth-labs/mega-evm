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
Enable it with `--fork`:

```bash
mega-evme run --fork ...
```

By default it connects to `http://localhost:8545`.
Override the endpoint with `--rpc` (or the `RPC_URL` environment variable) and pin a specific block with `--fork.block`:

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

## State File Format

### Fields

Each top-level key is a checksummed or lowercase hex address.
The value is an account object with these fields:

| Field      | Type         | Description                       |
| ---------- | ------------ | --------------------------------- |
| `balance`  | hex quantity | Account balance in wei            |
| `nonce`    | hex quantity | Transaction nonce                 |
| `code`     | hex bytes    | Deployed bytecode (`0x` for EOAs) |
| `codeHash` | hex bytes    | Keccak256 hash of `code`          |
| `storage`  | object       | Map of storage slot to value      |

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

### Format Notes

All numeric values use Ethereum quantity encoding:

- `0x` prefix is required.
- No leading zeros, except `0x0` for zero.
- `balance` is in wei.
- Storage keys and values are 32-byte hex strings (zero-padded to 64 hex characters).

## RPC Cache and Retry

When using fork mode, `mega-evme` caches RPC responses to avoid redundant network calls and supports configurable retry behavior for resilience against transient failures.

### Per-Chain Cache Files

Each chain gets its own cache file named `rpc-cache-{chain_id}.json` inside the cache directory.
The per-chain filename makes cross-chain contamination impossible by construction: a cache populated from mainnet physically cannot be loaded during a testnet run.

The default cache directory is the platform cache directory:

- **Linux**: `$XDG_CACHE_HOME/mega-evme/rpc`
- **macOS**: `~/Library/Caches/mega-evme/rpc`

### Cache Flags

| Flag                     | Type  | Default            | Description                                                                                                              |
| ------------------------ | ----- | ------------------ | ------------------------------------------------------------------------------------------------------------------------ |
| `--rpc.cache-size <N>`   | `u32` | `10000`            | Maximum number of items in the in-memory RPC LRU cache. Set to `0` to disable the cache layer entirely.                  |
| `--rpc.cache-dir <PATH>` | path  | Platform cache dir | Directory for per-chain cache files. Each chain's cache is stored as `{cache_dir}/rpc-cache-{chain_id}.json`.            |
| `--rpc.no-cache-file`    | flag  | `false`            | Disable on-disk cache persistence. The in-memory LRU cache still applies — use `--rpc.cache-size 0` to disable that too. |
| `--rpc.chain-id <ID>`    | `u64` | auto-detected      | Chain ID override. Skips the `eth_chainId` RPC call at startup and uses this value to locate the per-chain cache file.   |
| `--rpc.clear-cache`      | flag  | `false`            | Delete the current chain's cache file before loading it. Recovery path for a polluted or corrupt cache.                  |

### Retry Flags

| Flag                      | Type  | Default | Description                                                                                                                          |
| ------------------------- | ----- | ------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `--rpc.max-retries <N>`   | `u32` | `5`     | Maximum retry attempts for failing RPC requests. Retries on HTTP 429/503, rate-limit errors, and transport failures. `0` to disable. |
| `--rpc.backoff-ms <MS>`   | `u64` | `1000`  | Fixed sleep duration in milliseconds between retry attempts (no exponential backoff).                                                |
| `--rpc.rate-limit <CU/s>` | `u64` | `660`   | Compute units per second budget for the retry layer's rate-limit accounting.                                                         |

### Examples

Replay a transaction with a local cache directory and explicit chain ID (fully offline if the cache is warm):

```bash
mega-evme replay \
  --rpc.chain-id 4326 \
  --rpc.cache-dir ./my-cache \
  0xabc123...
```

Disable on-disk caching but keep the in-memory LRU:

```bash
mega-evme tx --fork --rpc.no-cache-file ...
```

Clear a corrupt cache before replaying:

```bash
mega-evme replay --rpc.clear-cache 0xabc123...
```

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
