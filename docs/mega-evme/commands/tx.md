---
description: Execute a transaction against local or fork-from-RPC state.
---

# tx

Run a transaction with full transaction context and optional RPC state forking.

`tx` is similar to [`run`](run.md) but operates at the transaction level rather than the bytecode level.
It handles sender nonces, transaction receipts, and — most importantly — can fork live chain state from a remote RPC endpoint so you can test against real contracts and real storage.

## Usage

```
mega-evme tx [OPTIONS] [RAW_TX]
```

## Raw Transaction Input

The optional `RAW_TX` positional argument accepts a raw EIP-2718 encoded transaction as a hex string.
When you provide it, `mega-evme` decodes the transaction and uses it as the base for execution.
Any CLI flags you pass alongside it act as **field overrides** on top of the decoded transaction — so you can replay a signed transaction while changing just the gas limit, input data, or any other field.

```bash
# Replay a raw signed transaction as-is
mega-evme tx 0x02f8...

# Replay the same transaction but override the input data
mega-evme tx 0x02f8... --input 0xdeadbeef
```

The overrides are checked against the transaction they produce, with the same rules a flags-only transaction follows, before anything executes.
The transaction's type is `--tx-type` when given and the decoded type otherwise, and only flags you pass are checked, so an omitted flag keeps the decoded value.
`--source-hash` and `--mint` need a deposit, `--priority-fee` a type other than legacy or EIP-2930, `--auth` an EIP-7702 transaction, and `--access` an EIP-2930, EIP-1559, or EIP-7702 transaction; `--create` cannot be combined with `--receiver`.
When `--tx-type` changes the type, the fields the transaction keeps from its decoded form are held to the same rules: a priority fee, an access list, or an authorization list the new type cannot carry is rejected rather than silently dropped.
`--tx-type` also cannot turn the transaction into a deposit or a deposit into another type, since a deposit's fields come from the decoded transaction.

If `RAW_TX` is omitted, `mega-evme` builds the transaction entirely from CLI flags.
The default sender is `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` and the default gas limit is `10000000`.

> **Note:** If the chain ID embedded in the raw transaction doesn't match `--chain-id`, `mega-evme` logs a warning but still proceeds.

## Fork Mode

By default, `tx` runs against local state — either empty or loaded from a `--prestate` file.
Fork mode fetches account balances, contract code, and storage slots on demand from a remote RPC node, so you can call real deployed contracts without manually constructing their state.

| Flag                    | Default  | Description                                                    |
| ----------------------- | -------- | -------------------------------------------------------------- |
| `--fork`                | `false`  | Enable state forking from RPC                                  |
| `--rpc <URL>`           | required | RPC endpoint to fork from (aliases: `--rpc-url`, `--fork.rpc`) |
| `--fork.block <NUMBER>` | latest   | Pin the fork to a specific block number                        |

When `--fork` is set, `mega-evme` connects to `--rpc` and resolves any state reads that aren't covered by a local `--prestate` file against that node.
`--fork.block` pins the fork to a specific block's post-state, which is useful for reproducing historical behavior or writing deterministic tests.
Without `--fork.block`, the fork uses the latest block at the time of execution.

`--fork` requires `--rpc` — there is no default endpoint, and the `RPC_URL` environment variable is not consulted:

```bash
mega-evme tx --fork --rpc https://mainnet.megaeth.com/rpc --sender.balance 1ether --receiver 0x4200000000000000000000000000000000000006 --input 0x06fdde03
```

Local `--prestate` overrides take precedence over forked state.
This lets you patch specific accounts or storage slots while still pulling everything else from the remote node.

## Options

`tx` accepts several shared option groups.
Each group has its own reference page with the full flag table.

| Group             | Description                                                          | Reference                                                                       |
| ----------------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Transaction       | Sender, receiver, value, gas, calldata, nonce, tx type               | [Transaction Types](../transaction-types.md)                                    |
| State management  | Prestate file, sender balance, faucet, storage overrides, state dump | [State Management](../configuration/state-management.md)                        |
| Chain / spec      | Spec version, chain ID                                               | [Chain and Spec](../configuration/chain-and-spec.md)                            |
| Block environment | Block number, timestamp, coinbase, basefee, gas limit, prevrandao    | [Block Environment](../configuration/block-environment.md)                      |
| SALT buckets      | Per-bucket capacity overrides for dynamic gas pricing                | [SALT Buckets](../configuration/salt-buckets.md)                                |
| RPC cache / retry | Cache entry ceiling, cache dir, retry and rate-limit                 | [RPC Cache and Retry](../configuration/state-management.md#rpc-cache-and-retry) |
| Tracing           | Opcode, call, and pre-state tracers with output options              | [Tracing Overview](../tracing/overview.md)                                      |
| Output            | JSON output mode                                                     | See [JSON output](#json-output) below                                           |

## JSON Output

Pass `--json` to emit a single `ExecutionSummary` JSON object to stdout instead of the human-readable banner.
No banners or diagnostic text are printed in JSON mode — stdout contains exactly one JSON object.

The output includes the same fields as [`run --json`](run.md#json-output), plus one additional field:

| Field     | Type             | Description                                                                 |
| --------- | ---------------- | --------------------------------------------------------------------------- |
| `receipt` | `object \| null` | Full transaction receipt with status, logs, gas usage, and contract address |

The receipt's `contractAddress` is set for every contract creation, including one whose init code reverted or halted, as an execution client's receipt reports it, while the summary's `contract_address` names only a contract that was actually deployed.

```bash
mega-evme tx --fork --rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x06fdde03 \
  --json
```

## Examples

### Simple call to a contract

```bash
# Check WETH balance of the default sender
mega-evme tx \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266
```

### Fork from a remote RPC

```bash
# Call WETH.balanceOf against live mainnet state
mega-evme tx \
  --fork \
  --rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266
```

### Fork from a specific block

Pinning to a block number makes the execution fully deterministic regardless of when you run it.

```bash
mega-evme tx \
  --fork \
  --rpc https://mainnet.megaeth.com/rpc \
  --fork.block 21000000 \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266
```

### Replay a raw transaction with an override

Decode a signed transaction from the mempool or a block, then re-run it with a different gas limit.

```bash
mega-evme tx 0x02f8ac... --gas 500000
```

### Fork with a patched storage slot

Fork live state but override a specific storage slot before execution — useful for testing access-controlled functions.

```bash
# Override WETH slot 0 (total supply) and call totalSupply() to verify
mega-evme tx \
  --fork \
  --rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --storage "0x4200000000000000000000000000000000000006:0x0=0x0000000000000000000000000000000000000000000000056bc75e2d63100000" \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x18160ddd
```

For more complex scenarios — multi-step state transitions, contract deployment followed by interaction, and EIP-7702 delegation flows — see the [Cookbook](../cookbook.md).

## Full Help Output

```
Run arbitrary transaction

Usage: mega-evme tx [OPTIONS] [RAW_TX]

Arguments:
  [RAW_TX]
          Raw EIP-2718 encoded transaction (hex). When provided, used as the base transaction with CLI flags serving as overrides

Options:
  -v...
          Increase logging verbosity (-v = error, -vv = warn, -vvv = info, -vvvv = debug, -vvvvv = trace)

      --log.file <LOG_FILE>
          Log file path. If specified, logs are written to this file instead of stderr

          [alias: --log-file]

      --log.no-color
          Disable colorful console logging. Only applies when logging to stderr (no --log.file)

          [alias: --log-no-color]

  -h, --help
          Print help (see a summary with '-h')

Transaction Options:
      --tx-type <TX_TYPE>
          Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, etc.) [default: 0]

          [aliases: --type, --ty]

      --gas <GAS>
          Gas limit for the evm [default: 10000000]

          [alias: --gas-limit]

      --basefee <BASEFEE>
          Price set for the evm (gas price) [default: 0]

          [aliases: --gas-price, --price, --base-fee]

      --priority-fee <PRIORITY_FEE>
          Gas priority fee (EIP-1559)

          [aliases: --priorityfee, --tip]

      --sender <SENDER>
          The transaction origin [default: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266]

          [alias: --from]

      --receiver <RECEIVER>
          The transaction receiver (execution context)

          [alias: --to]

      --nonce <NONCE>
          The transaction nonce

      --create <CREATE>
          Indicates the action should be create rather than call

          [possible values: true, false]

      --value <VALUE>
          Value set for the evm. VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei). Examples: `--value 1ether`, `--value 100gwei`, `--value 1000000000000000000`

      --input <INPUT>
          Transaction data (input) as hex string

          [alias: --data]

      --inputfile <INPUTFILE>
          File containing transaction data (input). If '-' is specified, input is read from stdin

          [aliases: --datafile, --input-file, --data-file]

      --source-hash <HASH>
          Source hash for deposit transactions (tx-type 126)

          [alias: --sourcehash]

      --mint <MINT>
          Amount of ETH to mint for deposit transactions (wei)

      --auth <AUTH>
          EIP-7702 authorization in format `AUTHORITY:NONCE->DELEGATION` (can be repeated)

          [alias: --authorization]

      --access <ACCESS>
          EIP-2930 access list entry in format `ADDRESS` or `ADDRESS:KEY1,KEY2,...` (can be repeated)

          [aliases: --accesslist, --access-list]

State Options:
      --fork
          Fork state from a remote RPC endpoint

      --fork.block <FORK_BLOCK>
          Block number of the state (post-block state) to fork from. If not specified, the latest block is used. Only used if `fork` is true

      --prestate <PRESTATE>
          JSON file with prestate (genesis) config. This overrides the state in the forked remote state (if applicable)

          [alias: --pre-state]

      --block-hash <BLOCK_HASHES>
          History block hashes to serve `BLOCKHASH` opcode. This overrides the block hashes in the forked remote state (if applicable). Each entry should be in the format `block_number:block_hash` (can be repeated)

          [aliases: --blockhash, --block-hashes, --blockhashes]

      --sender.balance <SENDER_BALANCE>
          Balance to allocate to the sender account. VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei). Examples: `--sender.balance 1ether`, `--sender.balance 1000000000000000000` If not specified, sender balance is not set (fallback to `prestate` if specified, otherwise 0)

          [alias: --from.balance]

      --faucet <FAUCET>
          Add ether to specified addresses. Each entry format: `ADDRESS+=VALUE` VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei). Examples: `--faucet 0x1234+=100ether`, `--faucet 0x5678+=1000000gwei` Can be repeated for multiple addresses

      --balance <BALANCE>
          Override balance for specified addresses. Each entry format: `ADDRESS=VALUE` VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei). Examples: `--balance 0x1234=100ether`

      --storage <STORAGE>
          Override storage slots. Each entry format: `ADDRESS:SLOT=VALUE` SLOT and VALUE are U256 (hex or decimal). Examples: `--storage 0x1234:0x0=0x1`

RPC Options:
      --rpc <RPC_URL>
          RPC URL. Required for networked operation (replay, run --fork, tx --fork)

          [alias: --rpc-url]

      --rpc.capture-file <CAPTURE_FILE>
          Capture JSON-RPC responses to a single file for later offline replay. Requires `--rpc`. If the file already exists, its entries are loaded and merged; missing entries are fetched via the RPC endpoint and persisted on clean exit. Cannot be used with --rpc.replay-file, --rpc.cache-dir, --rpc.clear-cache, --rpc.no-cache-file, or --rpc.cache-max-entries

      --rpc.replay-file <REPLAY_FILE>
          Replay from a previously captured JSON-RPC fixture file (offline). Cannot be used with `--rpc`. Any RPC miss is a hard error; the file is never written. Cannot be used with --rpc.capture-file, --rpc.cache-dir, --rpc.clear-cache, --rpc.no-cache-file, or --rpc.cache-max-entries

      --rpc.cache-max-entries <cache_max_entries>
          Maximum number of items in the in-memory RPC LRU cache (and therefore what gets persisted to the cache file). `0` = effectively unlimited, which caps at 1,048,576 entries so a long-running process cannot grow without bound. The ceiling is not an allocation: memory grows with the entries actually cached, so a high value costs nothing until that many responses arrive. Default is `0`

          [default: 0]

      --rpc.cache-dir <CACHE_DIR>
          Directory for per-chain RPC cache files.

          Each chain's cache is stored as `{cache_dir}/rpc-cache-{chain_id}.json`. Different chains cannot share a file, so cross-chain contamination is impossible by construction.

          Defaults to the platform cache directory (`$XDG_CACHE_HOME/mega-evme/rpc` on Linux, `~/Library/Caches/mega-evme/rpc` on macOS). Pass `--rpc.no-cache-file` to disable on-disk persistence entirely. Batch replay (`--tx-file` / `--block`) uses the on-disk cache only when this flag or `--rpc.clear-cache` is passed explicitly.

      --rpc.no-cache-file
          Disable on-disk cache persistence. The in-memory LRU cache still applies. Takes precedence over `--rpc.clear-cache`: with no cache file in play there is nothing to delete, load, or persist. This is already the default for batch replay (`--tx-file` / `--block`) unless `--rpc.cache-dir` or `--rpc.clear-cache` is passed

      --rpc.clear-cache
          Delete the current chain's cache file before loading it. Recovery path for a polluted or corrupt cache file. If the unlink itself fails (e.g. insufficient permissions), `mega-evme` aborts rather than silently reloading the stale file. Passing this flag engages the on-disk cache, including in batch replay (`--tx-file` / `--block`), where it is otherwise off by default. Has no effect alongside `--rpc.no-cache-file`

      --rpc.max-retries <MAX_RETRIES>
          Maximum number of times the transport layer will retry a failing RPC request. Retries trigger on HTTP 429 / 503, JSON-RPC rate-limit error responses, and transport failures surfaced as `TransportErrorKind::Custom` (connection refused, DNS failure, TLS handshake, request timeout, etc.). Set to 0 to disable retries entirely

          [default: 5]

      --rpc.backoff-ms <BACKOFF_MS>
          Fixed sleep duration, in milliseconds, inserted between retry attempts unless the server supplies its own backoff hint. This layer does not perform exponential backoff

          [default: 1000]

      --rpc.cu-per-sec <COMPUTE_UNITS_PER_SEC>
          Compute-unit budget (CU/s) for the retry layer's rate-limit accounting. This is NOT requests per second: each RPC method costs multiple compute units. A single-digit value will heavily self-throttle. Default (660) matches typical public-endpoint budgets

          [default: 660]
          [alias: --rpc.rate-limit]

      --rpc.request-timeout <REQUEST_TIMEOUT>
          Total per-HTTP-request timeout in seconds (connect + response). `0` disables the timeout (previous behavior: a hung endpoint can block forever). A non-zero timeout surfaces a hung endpoint as a retryable transport error

          [default: 30]

Chain Options:
      --spec <SPEC>
          Name of spec to use, possible values: `Equivalence`, `MiniRex`, `MiniRex1`, `MiniRex2`, `Rex`, `Rex1`, `Rex2`, `Rex3`, `Rex4`, `Rex5`, `Rex6` (`MiniRex1`/`MiniRex2` are alias specs executing `Equivalence`/`MiniRex` behavior)

          [default: Rex6]

      --chain-id <CHAIN_ID>
          `ChainID` to use

          [default: 6342]
          [alias: --chainid]

Block Options:
      --block.number <BLOCK_NUMBER>
          Block number

          [default: 1]

      --block.coinbase <BLOCK_COINBASE>
          Block coinbase/beneficiary address

          [default: 0x0000000000000000000000000000000000000000]
          [alias: --block.beneficiary]

      --block.timestamp <BLOCK_TIMESTAMP>
          Block timestamp

          [default: 1]

      --block.gaslimit <BLOCK_GAS_LIMIT>
          Block gas limit

          [default: 10000000000]
          [aliases: --block.gas-limit, --block.gas]

      --block.basefee <BLOCK_BASEFEE>
          Block base fee per gas (EIP-1559)

          [default: 0]
          [alias: --block.base-fee]

      --block.difficulty <BLOCK_DIFFICULTY>
          Block difficulty

          [default: 0]

      --block.prevrandao <BLOCK_PREVRANDAO>
          Block prevrandao (replaces difficulty post-merge). Required for post-merge blocks

          [default: 0x0000000000000000000000000000000000000000000000000000000000000000]
          [alias: --block.random]

      --block.blobexcessgas <BLOCK_BLOB_EXCESS_GAS>
          Excess blob gas for EIP-4844, from which the blob base fee is derived

          [default: 0]
          [alias: --block.blob-excess-gas]

External Environment Options:
      --bucket-capacity <BUCKET_ID:CAPACITY>
          Bucket capacity configuration in format "`bucket_id:capacity`" Can be specified multiple times for different buckets. Example: --bucket-capacity 123:1000000 --bucket-capacity 456:2000000

State Dump Options:
      --dump
          Dumps the state after the run

      --dump.output <DUMP_OUTPUT_FILE>
          Output file for state dump (if not specified, prints to console)

Trace Options:
      --trace
          Enable tracing

      --trace.output <TRACE_OUTPUT_FILE>
          Output file for trace data (if not specified, prints to console)

      --tracer <TRACER>
          Tracer type to use (defaults to struct logger if not specified)

          Possible values:
          - opcode:    Enable execution tracing (opcode-level trace in Geth format)
          - call:      Enable call tracing (tracks call frames in nested tree structure)
          - pre-state: Enable pre-state tracing (retrieves account state before execution)

          [default: opcode]

      --trace.opcode.disable-memory
          Disable memory capture in traces (opcode tracer only)

      --trace.opcode.disable-stack
          Disable stack capture in traces (opcode tracer only)

      --trace.opcode.disable-storage
          Disable storage capture in traces (opcode tracer only)

      --trace.opcode.enable-return-data
          Enable return data capture in traces (opcode tracer only)

      --trace.call.only-top-call
          Only trace top-level call (call tracer only)

      --trace.call.with-log
          Include logs in call trace (call tracer only)

      --trace.prestate.diff-mode
          Show state diff instead of prestate (pre-state tracer only)

          [alias: --trace.pre-state.diff-mode]

      --trace.prestate.disable-code
          Disable code in prestate output (pre-state tracer only)

          [alias: --trace.pre-state.disable-code]

      --trace.prestate.disable-storage
          Disable storage in prestate output (pre-state tracer only)

          [alias: --trace.pre-state.disable-storage]

Output Options:
      --json
          Output results as JSON instead of human-readable text
```

## See Also

- [Transaction Types](../transaction-types.md) — full flag reference for all transaction types
- [State Management](../configuration/state-management.md) — prestate files, balance overrides, state dump
- [Chain and Spec](../configuration/chain-and-spec.md) — spec selection and chain ID
- [Block Environment](../configuration/block-environment.md) — block number, timestamp, coinbase, and more
- [SALT Buckets](../configuration/salt-buckets.md) — dynamic gas pricing configuration
- [Tracing Overview](../tracing/overview.md) — opcode, call, and pre-state tracers
- [Cookbook](../cookbook.md) — worked examples and real-world recipes
- [`run`](run.md) — execute raw bytecode without a full transaction context
- [`replay`](replay.md) — fetch and re-execute an on-chain transaction by hash
