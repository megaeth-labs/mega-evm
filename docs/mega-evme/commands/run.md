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
Exactly one of these two inputs must be provided.

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

| Group             | Flags                                                                                                                                                                                      | Reference                                                  |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------- |
| Transaction       | `--create`, `--gas`, `--basefee`, `--priority-fee`, `--tx-type`, `--value`, `--sender`, `--receiver`, `--nonce`, `--input`, `--inputfile`, `--source-hash`, `--mint`, `--auth`, `--access` | [Transaction Types](../transaction-types.md)               |
| State management  | `--prestate`, `--sender.balance`, `--dump`, `--dump.output`                                                                                                                                | [State Management](../configuration/state-management.md)   |
| Chain and spec    | `--spec`, `--chain-id`                                                                                                                                                                     | [Chain and Spec](../configuration/chain-and-spec.md)       |
| Block environment | `--block.number`, `--block.coinbase`, `--block.timestamp`, `--block.gaslimit`, `--block.basefee`, `--block.difficulty`, `--block.prevrandao`, `--block.blobexcessgas`                      | [Block Environment](../configuration/block-environment.md) |
| SALT buckets      | `--bucket-capacity`                                                                                                                                                                        | [SALT Buckets](../configuration/salt-buckets.md)           |
| Tracing           | `--trace`, `--tracer`, `--trace.output`, and tracer-specific flags                                                                                                                         | [Tracing Overview](../tracing/overview.md)                 |
| Output            | `--json`                                                                                                                                                                                   | See [JSON output](#json-output) below                      |

**Key defaults for `run`:**

| Option       | Default                                      |
| ------------ | -------------------------------------------- |
| `--spec`     | `Rex4`                                       |
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

          [aliases: --log-file]

      --log.no-color
          Disable colorful console logging. Only applies when logging to stderr (no --log.file)

          [aliases: --log-no-color]

  -h, --help
          Print help (see a summary with '-h')

Transaction Options:
      --tx-type <TX_TYPE>
          Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, etc.) [default: 0]

          [aliases: --type, --ty]

      --gas <GAS>
          Gas limit for the evm [default: 10000000]

          [aliases: --gas-limit]

      --basefee <BASEFEE>
          Price set for the evm (gas price) [default: 0]

          [aliases: --gas-price, --price, --base-fee]

      --priority-fee <PRIORITY_FEE>
          Gas priority fee (EIP-1559)

          [aliases: --priorityfee, --tip]

      --sender <SENDER>
          The transaction origin [default: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266]

          [aliases: --from]

      --receiver <RECEIVER>
          The transaction receiver (execution context)

          [aliases: --to]

      --nonce <NONCE>
          The transaction nonce

      --create <CREATE>
          Indicates the action should be create rather than call

          [possible values: true, false]

      --value <VALUE>
          Value set for the evm. VALUE can be: plain number (wei), or number with suffix
          (ether, gwei, wei). Examples: `--value 1ether`, `--value 100gwei`

      --input <INPUT>
          Transaction data (input) as hex string

          [aliases: --data]

      --inputfile <INPUTFILE>
          File containing transaction data (input). If '-' is specified, input is read from stdin

          [aliases: --datafile, --input-file, --data-file]

      --source-hash <HASH>
          Source hash for deposit transactions (tx-type 126)

          [aliases: --sourcehash]

      --mint <MINT>
          Amount of ETH to mint for deposit transactions (wei)

      --auth <AUTH>
          EIP-7702 authorization in format `AUTHORITY:NONCE->DELEGATION` (can be repeated)

          [aliases: --authorization]

      --access <ACCESS>
          EIP-2930 access list entry in format `ADDRESS` or `ADDRESS:KEY1,KEY2,...` (can be repeated)

          [aliases: --accesslist, --access-list]

State Options:
      --fork
          Fork state from a remote RPC endpoint

      --fork.block <FORK_BLOCK>
          Block number of the state (post-block state) to fork from

      --rpc <RPC_URL>
          RPC URL [env: RPC_URL=] [default: http://localhost:8545]

          [aliases: --rpc-url] [compat alias: --fork.rpc]

      --prestate <PRESTATE>
          JSON file with prestate (genesis) config

          [aliases: --pre-state]

      --block-hash <BLOCK_HASHES>
          History block hashes for BLOCKHASH opcode. Format: `block_number:block_hash` (repeatable)

          [aliases: --blockhash, --block-hashes, --blockhashes]

      --sender.balance <SENDER_BALANCE>
          Balance to allocate to the sender account. Supports suffixes: ether, gwei, wei

          [aliases: --from.balance]

      --faucet <FAUCET>
          Add ether to addresses. Format: `ADDRESS+=VALUE` (repeatable, supports suffixes)

      --balance <BALANCE>
          Override balance for addresses. Format: `ADDRESS=VALUE` (repeatable, supports suffixes)

      --storage <STORAGE>
          Override storage slots. Format: `ADDRESS:SLOT=VALUE` (repeatable)

Chain Options:
      --spec <SPEC>
          Spec to use: MiniRex, Equivalence, Rex, Rex1, Rex2, Rex3, Rex4 [default: Rex4]

      --chain-id <CHAIN_ID>
          Chain ID [default: 6342] [aliases: --chainid]

Block Options:
      --block.number <BLOCK_NUMBER>          Block number [default: 1]
      --block.coinbase <BLOCK_COINBASE>      Block coinbase [default: 0x0...0] [aliases: --block.beneficiary]
      --block.timestamp <BLOCK_TIMESTAMP>    Block timestamp [default: 1]
      --block.gaslimit <BLOCK_GAS_LIMIT>     Block gas limit [default: 10000000000]
      --block.basefee <BLOCK_BASEFEE>        Block base fee [default: 0]
      --block.difficulty <BLOCK_DIFFICULTY>   Block difficulty [default: 0]
      --block.prevrandao <BLOCK_PREVRANDAO>  Previous randao [aliases: --block.random]
      --block.blobexcessgas <GAS>            Blob excess gas [default: 0]

External Environment Options:
      --bucket-capacity <BUCKET_ID:CAPACITY>
          Bucket capacity configuration. Format: `bucket_id:capacity` (repeatable)

State Dump Options:
      --dump                           Dump state after the run
      --dump.output <DUMP_OUTPUT_FILE> Output file for state dump

Output Options:
      --json                           Output results as JSON instead of human-readable text

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
```

## See Also

- [Transaction Types](../transaction-types.md) — full option reference for `--tx-type`, `--input`, `--access`, and more
- [Chain and Spec](../configuration/chain-and-spec.md) — choosing a spec and chain ID
- [Block Environment](../configuration/block-environment.md) — controlling block number, timestamp, coinbase, and other block fields
- [State Management](../configuration/state-management.md) — loading prestate and dumping final state
- [SALT Buckets](../configuration/salt-buckets.md) — configuring bucket capacities for storage gas testing
- [Tracing Overview](../tracing/overview.md) — opcode, call, and pre-state tracers
- [Cookbook](../cookbook.md) — worked examples for common use cases
