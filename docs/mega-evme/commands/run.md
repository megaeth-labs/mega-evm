---
description: Run raw EVM bytecode in call or create mode with full environment control.
---

# run

Execute arbitrary EVM bytecode in a controlled local environment.

## Usage

```
mega-evme run [OPTIONS] [CODE]
```

`CODE` is the bytecode to execute, given as a `0x`-prefixed hex string.
You can also supply bytecode from a file with `--codefile`.
One of these two inputs must be provided; if both are given, the positional `CODE` is used and `--codefile` is ignored.

## Code Input

| Argument            | Description                                                     |
| ------------------- | --------------------------------------------------------------- |
| `CODE`              | EVM bytecode as a `0x`-prefixed hex string (positional)         |
| `--codefile <PATH>` | Path to a file containing bytecode. Use `-` to read from stdin. |

## Execution Modes

### Call Mode (default)

By default, `run` operates in call mode.
Before execution, the bytecode is deployed at the receiver address (default: `0x0000000000000000000000000000000000000000`).
The transaction is then a `CALL` to that address.
Input data supplied via `--input` or `--inputfile` is passed as calldata.

Use `--receiver` to target a specific address, or rely on the default zero address for simple bytecode tests.

### Create Mode (`--create`)

Pass `--create true` to treat the bytecode as init code.
Any input data supplied via `--input` or `--inputfile` is appended to the init code before execution, allowing you to pass constructor arguments.
On success, the output is the deployed contract's runtime bytecode, and the tool prints the resulting contract address.

In create mode, `--receiver` must not be set.

## Options

`run` accepts several groups of shared options.
Each group is documented on its own page.

| Group             | Flags                                                                                                                                                                                      | Reference                                                                       |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| Transaction       | `--create`, `--gas`, `--basefee`, `--priority-fee`, `--tx-type`, `--value`, `--sender`, `--receiver`, `--nonce`, `--input`, `--inputfile`, `--source-hash`, `--mint`, `--auth`, `--access` | [Transaction Types](../transaction-types.md)                                    |
| State management  | `--prestate`, `--sender.balance`, `--faucet`, `--balance`, `--storage`, `--block-hash`, `--fork`, `--fork.block`, `--rpc`, `--dump`, `--dump.output`                                       | [State Management](../configuration/state-management.md)                        |
| Chain and spec    | `--spec`, `--chain-id`                                                                                                                                                                     | [Chain and Spec](../configuration/chain-and-spec.md)                            |
| Block environment | `--block.number`, `--block.coinbase`, `--block.timestamp`, `--block.gaslimit`, `--block.basefee`, `--block.difficulty`, `--block.prevrandao`, `--block.blobexcessgas`                      | [Block Environment](../configuration/block-environment.md)                      |
| SALT buckets      | `--bucket-capacity`                                                                                                                                                                        | [SALT Buckets](../configuration/salt-buckets.md)                                |
| RPC cache / retry | `--rpc.cache-max-entries`, `--rpc.cache-dir`, `--rpc.no-cache-file`, `--rpc.clear-cache`, `--rpc.max-retries`, `--rpc.backoff-ms`, `--rpc.cu-per-sec`, `--rpc.request-timeout`             | [RPC Cache and Retry](../configuration/state-management.md#rpc-cache-and-retry) |
| Tracing           | `--trace`, `--tracer`, `--trace.output`, and tracer-specific flags                                                                                                                         | [Tracing Overview](../tracing/overview.md)                                      |
| Output            | `--json`                                                                                                                                                                                   | See [JSON output](#json-output) below                                           |

**Key defaults for `run`:**

| Option       | Default                                      |
| ------------ | -------------------------------------------- |
| `--spec`     | `Rex6`                                       |
| `--gas`      | `10000000`                                   |
| `--sender`   | `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` |
| `--receiver` | `0x0000000000000000000000000000000000000000` |

## JSON Output

Pass `--json` to emit a single `ExecutionSummary` JSON object to stdout instead of the human-readable banner.
No banners or diagnostic text are printed in JSON mode — stdout contains exactly one JSON object.

The object includes these fields:

| Field              | Type             | Description                                                                  |
| ------------------ | ---------------- | ---------------------------------------------------------------------------- |
| `success`          | `bool`           | Whether execution succeeded                                                  |
| `gas_used`         | `number`         | Gas consumed                                                                 |
| `output`           | `string \| null` | Hex-encoded return data (present only on success with non-empty output)      |
| `contract_address` | `string \| null` | Deployed address (present only for successful `--create` transactions)       |
| `logs_count`       | `number`         | Number of log entries emitted                                                |
| `revert_reason`    | `string \| null` | Decoded revert reason (present only on revert)                               |
| `halt_reason`      | `string \| null` | Halt reason (present only on halt)                                           |
| `trace`            | `object \| null` | Execution trace (when `--trace` is enabled without `--trace.output`)         |
| `state`            | `object \| null` | Post-execution state dump (when `--dump` is enabled without `--dump.output`) |

When `--trace` or `--dump` is used with an output file (`--trace.output`, `--dump.output`), data is written to that file and the corresponding JSON field is omitted.
When no output file is specified, the data is inlined into the JSON object.

```bash
mega-evme run 0x60016000526001601ff3 --json
```

## Examples

### Simple bytecode execution

Execute bytecode that stores `1` in memory and returns it:

```bash
mega-evme run 0x60016000526001601ff3
```

### Execute from a file

Read bytecode from a hex file:

```bash
mega-evme run --codefile contract.hex
```

Read from stdin:

```bash
cat contract.hex | mega-evme run --codefile -
```

### Contract deployment with `--create`

Deploy a contract using init code.
The tool prints the deployed contract address on success:

```bash
mega-evme run --create true 0x6080604052...
```

Dump the resulting state to inspect what was deployed:

```bash
mega-evme run --create true 0x6080604052... --dump
```

### Execute with input data

Call the bytecode at the receiver address with calldata:

```bash
mega-evme run 0x60016000526001601ff3 --input 0xabcdef01
```

Pass constructor arguments in create mode (appended to init code):

```bash
mega-evme run --create true 0x6080604052... \
  --input 0x0000000000000000000000001234567890abcdef1234567890abcdef12345678
```

### Trace execution

Run with opcode-level tracing and save the output:

```bash
mega-evme run 0x60016000526001601ff3 \
  --trace --tracer opcode \
  --trace.output trace.json
```

## Full Help Output

```
Run arbitrary EVM bytecode

Usage: mega-evme run [OPTIONS] [CODE]

Arguments:
  [CODE]
          EVM bytecode as hex string (positional argument)

Options:
      --codefile <CODEFILE>
          File containing EVM code. If '-' is specified, code is read from stdin

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

- [Transaction Types](../transaction-types.md) — full option reference for `--tx-type`, `--input`, `--access`, and more
- [Chain and Spec](../configuration/chain-and-spec.md) — choosing a spec and chain ID
- [Block Environment](../configuration/block-environment.md) — controlling block number, timestamp, coinbase, and other block fields
- [State Management](../configuration/state-management.md) — loading prestate and dumping final state
- [SALT Buckets](../configuration/salt-buckets.md) — configuring bucket capacities for storage gas testing
- [Tracing Overview](../tracing/overview.md) — opcode, call, and pre-state tracers
- [Cookbook](../cookbook.md) — worked examples for common use cases
