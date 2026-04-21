---
description: Fetch and re-execute an on-chain transaction with optional overrides and tracing.
---

# replay

Re-execute a historical transaction locally using state and block context fetched from an RPC endpoint.

## Usage

```
mega-evme replay [OPTIONS] <TX_HASH>
```

## Arguments

### `TX_HASH`

The transaction hash to replay (32-byte hex, required).

`mega-evme` fetches the transaction, its block environment, and the relevant pre-state from the RPC, then re-executes it locally.
This gives you a fully reproducible execution without needing a local archive node.

### `--rpc <URL>`

Aliases: `--rpc-url`
Environment variable: `RPC_URL`
Default: `http://localhost:8545`

The RPC endpoint to fetch the transaction and state from.
Any standard Ethereum JSON-RPC provider works here.

```
mega-evme replay --rpc https://mainnet.megaeth.com/rpc <TX_HASH>
```

## RPC Cache File

`mega-evme replay` supports a transport-level JSON-RPC fixture mechanism that records every request/response pair to a single file and serves them back on later runs without touching the network.
It is useful for pinning a reproducible replay (e.g. for regression tests, debugging sessions, or offline review) and for running `replay` in environments that cannot reach the RPC endpoint.

Unlike the generic [RPC Cache](../configuration/state-management.md#rpc-cache-and-retry), which is keyed on a small allow-list of cacheable methods and stored per chain under the platform cache directory, the cache file covers **every** JSON-RPC call issued during the replay and lives at a user-chosen path.

The mechanism has two modes, selected by two mutually exclusive flags.

### `--rpc.capture-file <PATH>`

Capture mode. Requires `--rpc`.

On the first run, every JSON-RPC request and response issued while serving the replay is appended to `<PATH>`.
On subsequent runs the existing file is loaded, its entries are merged into the in-memory transport cache, and only missing requests are fetched from the RPC endpoint.
The updated set of entries is persisted back to the same file on clean exit.

The file also embeds an external-environment snapshot — currently the set of `--bucket-capacity` values in effect — so the captured fixture is self-contained.
If `--bucket-capacity` is not passed on a subsequent run, the previous envelope's values are reused; passing `--bucket-capacity` overrides them.

`--rpc.capture-file` is mutually exclusive with `--rpc.replay-file`, `--rpc.cache-dir`, `--rpc.clear-cache`, `--rpc.no-cache-file`, and `--rpc.cache-size`.

### `--rpc.replay-file <PATH>`

Replay mode. Requires neither `--rpc` nor network access.

The envelope at `<PATH>` is loaded and serves as the only source of JSON-RPC responses.
Any request that is not present in the fixture aborts the run with a hard error — there is no fall-through to a live RPC endpoint.

Bucket-capacity data is read from the fixture envelope, so `--bucket-capacity` is neither required nor accepted with `--rpc.replay-file`.
Passing `--bucket-capacity` together with `--rpc.replay-file` is rejected; to regenerate a fixture with new capacities, re-run in capture mode.

`--rpc.replay-file` is mutually exclusive with `--rpc`, `--rpc.capture-file`, `--rpc.cache-dir`, `--rpc.clear-cache`, `--rpc.no-cache-file`, and `--rpc.cache-size`.

### Examples

Capture a transaction's RPC traffic to a fixture file (first run hits the endpoint, later runs with the same flag reuse and top up the file):

```bash
mega-evme replay \
  --rpc https://mainnet.megaeth.com/rpc \
  --rpc.capture-file ./fixtures/tx.json \
  0xabc123...
```

Replay the captured transaction fully offline:

```bash
mega-evme replay --rpc.replay-file ./fixtures/tx.json 0xabc123...
```

## Spec Auto-Detection

The EVM spec controls which opcodes, gas rules, and MegaETH-specific behaviors are active during execution.
`replay` auto-detects the spec from the chain ID and the block timestamp of the replayed transaction.
Hardcoded hardfork configs exist for:

- **Chain 6343** — MegaETH testnet v2
- **Chain 4326** — MegaETH mainnet

For any other chain, `replay` enables all hardforks at genesis (currently equivalent to `Rex4`).

### `--override.spec <SPEC>`

Override the auto-detected spec.
Useful when you want to test how the transaction would behave under a different spec, or when replaying against a chain that isn't recognized.

```
mega-evme replay --override.spec Rex2 <TX_HASH>
```

## Transaction Overrides

Override flags let you modify the transaction before re-executing it.
The original on-chain transaction is unchanged; only the local replay is affected.
This is useful for "what-if" testing: what happens with less gas, different calldata, or a different ETH value?

| Flag                           | Aliases                | Description                                                        |
| ------------------------------ | ---------------------- | ------------------------------------------------------------------ |
| `--override.gas-limit <GAS>`   | `--override.gaslimit`  | Replace the transaction's gas limit                                |
| `--override.value <VALUE>`     |                        | Replace the ETH value sent (supports `1ether`, `100gwei` suffixes) |
| `--override.input <HEX>`       | `--override.data`      | Replace the calldata with a hex-encoded byte string                |
| `--override.input-file <FILE>` | `--override.data-file` | Replace the calldata with the contents of a file                   |

Note that `replay` does not accept `--chain`, block environment flags, or pre-state flags.
All of that context comes from the RPC.

## Options

`replay` supports the following shared option groups.
See the linked pages for full details.

- **SALT buckets** — Configure SALT bucket capacity for dynamic storage gas pricing.
  See [SALT Buckets](../configuration/salt-buckets.md).
- **State dump** — Dump or load pre/post-state snapshots.
  See [State Management](../configuration/state-management.md).
- **RPC cache file** — Single-file JSON-RPC capture and offline replay via `--rpc.capture-file` / `--rpc.replay-file`.
  See [RPC Cache File](#rpc-cache-file) above.
- **RPC cache / retry** — Per-chain response cache, chain-id override, retry and rate-limit settings.
  See [RPC Cache and Retry](../configuration/state-management.md#rpc-cache-and-retry).
- **Tracing** — Emit execution traces (call traces, opcode traces, gas profiles, etc.).
  See [Tracing Overview](../tracing/overview.md).

## Examples

In all examples below, replace `0xabc123...` with a real transaction hash from MegaETH mainnet.

**Basic replay**

```bash
mega-evme replay 0xabc123...
```

**Replay against a custom RPC**

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc 0xabc123...
```

**Replay with call tracing**

```bash
mega-evme replay --trace --tracer call 0xabc123...
```

**Replay with a tighter gas limit**

```bash
mega-evme replay --override.gas-limit 50000 0xabc123...
```

**Replay with different calldata**

```bash
mega-evme replay --override.input 0xdeadbeef 0xabc123...
```

**Replay under a specific spec**

```bash
mega-evme replay --override.spec Rex2 0xabc123...
```

## Full Help Output

```
Replay a transaction from RPC

Usage: mega-evme replay [OPTIONS] <TX_HASH>

Arguments:
  <TX_HASH>
          Transaction hash to replay

Options:
      --rpc <RPC>
          RPC URL to fetch transaction from
          [env: RPC_URL=] [default: http://localhost:8545] [aliases: --rpc-url]

      --rpc.capture-file <PATH>
          Capture JSON-RPC responses to a single file for later offline replay.
          Requires `--rpc`. If the file already exists, its entries are loaded
          and merged; missing entries are fetched via the RPC endpoint and
          persisted on clean exit. Mutually exclusive with --rpc.replay-file,
          --rpc.cache-dir, --rpc.clear-cache, --rpc.no-cache-file, and
          --rpc.cache-size.

      --rpc.replay-file <PATH>
          Replay from a previously captured JSON-RPC fixture file (offline).
          Cannot be used with `--rpc`. Any RPC miss is a hard error; the file
          is never written. Mutually exclusive with --rpc.capture-file,
          --rpc.cache-dir, --rpc.clear-cache, --rpc.no-cache-file, and
          --rpc.cache-size.

  -v...
          Increase logging verbosity

      --log.file <LOG_FILE>
          Log file path [aliases: --log-file]

      --log.no-color
          Disable colorful console logging [aliases: --log-no-color]

  -h, --help
          Print help

External Environment Options:
      --bucket-capacity <BUCKET_ID:CAPACITY>
          Bucket capacity configuration (repeatable)

State Dump Options:
      --dump                           Dump state after the run
      --dump.output <DUMP_OUTPUT_FILE> Output file for state dump

Trace Options:
      --trace                                    Enable tracing
      --trace.output <TRACE_OUTPUT_FILE>         Output file for trace data
      --tracer <TRACER>                          Tracer: opcode, call, pre-state [default: opcode]
      --trace.opcode.disable-memory              Disable memory capture
      --trace.opcode.disable-stack               Disable stack capture
      --trace.opcode.disable-storage             Disable storage capture
      --trace.opcode.enable-return-data          Enable return data capture
      --trace.call.only-top-call                 Only trace top-level call
      --trace.call.with-log                      Include logs in call trace
      --trace.prestate.diff-mode                 Show state diff
      --trace.prestate.disable-code              Disable code in prestate output
      --trace.prestate.disable-storage           Disable storage in prestate output

      --override.spec <SPEC>
          Override the spec (default: auto-detect from chain ID and block timestamp)

Transaction Override Options:
      --override.gas-limit <GAS>       Override gas limit [aliases: --override.gaslimit]
      --override.value <VALUE>         Override value (supports ether/gwei/wei suffixes)
      --override.input <HEX>           Override input data [aliases: --override.data]
      --override.input-file <FILE>     Override input from file [aliases: --override.data-file]
```

## See Also

- [`run`](./run.md) — Execute raw EVM bytecode locally without fetching from RPC
- [`tx`](./tx.md) — Send a new transaction against a local state
- [Tracing Overview](../tracing/overview.md)
- [SALT Buckets](../configuration/salt-buckets.md)
- [State Management](../configuration/state-management.md)
