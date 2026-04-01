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

If `RAW_TX` is omitted, `mega-evme` builds the transaction entirely from CLI flags.
The default sender is `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` and the default gas limit is `10000000`.

> **Note:** If the chain ID embedded in the raw transaction doesn't match `--chain-id`, `mega-evme` logs a warning but still proceeds.

## Fork Mode

By default, `tx` runs against local state — either empty or loaded from a `--prestate` file.
Fork mode fetches account balances, contract code, and storage slots on demand from a remote RPC node, so you can call real deployed contracts without manually constructing their state.

| Flag                    | Default                 | Description                                |
| ----------------------- | ----------------------- | ------------------------------------------ |
| `--fork`                | `false`                 | Enable state forking from RPC              |
| `--fork.rpc <URL>`      | `http://localhost:8545` | RPC endpoint to fork from (env: `RPC_URL`) |
| `--fork.block <NUMBER>` | latest                  | Pin the fork to a specific block number    |

When `--fork` is set, `mega-evme` connects to `--fork.rpc` and resolves any state reads that aren't covered by a local `--prestate` file against that node.
`--fork.block` pins the fork to a specific block's post-state, which is useful for reproducing historical behavior or writing deterministic tests.
Without `--fork.block`, the fork uses the latest block at the time of execution.

You can set the RPC URL via the `RPC_URL` environment variable instead of passing `--fork.rpc` every time:

```bash
export RPC_URL=https://mainnet.megaeth.com/rpc
mega-evme tx --fork --sender.balance 1ether --receiver 0x4200000000000000000000000000000000000006 --input 0x06fdde03
```

Local `--prestate` overrides take precedence over forked state.
This lets you patch specific accounts or storage slots while still pulling everything else from the remote node.

## Options

`tx` accepts several shared option groups.
Each group has its own reference page with the full flag table.

| Group             | Description                                                          | Reference                                                  |
| ----------------- | -------------------------------------------------------------------- | ---------------------------------------------------------- |
| Transaction       | Sender, receiver, value, gas, calldata, nonce, tx type               | [Transaction Types](../transaction-types.md)               |
| State management  | Prestate file, sender balance, faucet, storage overrides, state dump | [State Management](../configuration/state-management.md)   |
| Chain / spec      | Spec version, chain ID                                               | [Chain and Spec](../configuration/chain-and-spec.md)       |
| Block environment | Block number, timestamp, coinbase, basefee, gas limit, prevrandao    | [Block Environment](../configuration/block-environment.md) |
| SALT buckets      | Per-bucket capacity overrides for dynamic gas pricing                | [SALT Buckets](../configuration/salt-buckets.md)           |
| Tracing           | Opcode, call, and pre-state tracers with output options              | [Tracing Overview](../tracing/overview.md)                 |

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
  --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266
```

### Fork from a specific block

Pinning to a block number makes the execution fully deterministic regardless of when you run it.

```bash
mega-evme tx \
  --fork \
  --fork.rpc https://mainnet.megaeth.com/rpc \
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
  --fork.rpc https://mainnet.megaeth.com/rpc \
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
          Raw EIP-2718 encoded transaction (hex). When provided, used as the base
          transaction with CLI flags serving as overrides

Options:
      --codefile, -v, --log.file, --log.no-color, -h
          (Same global options as `run`)

Transaction Options:
      --tx-type <TX_TYPE>          Transaction type [default: 0] [aliases: --type, --ty]
      --gas <GAS>                  Gas limit [default: 10000000] [aliases: --gas-limit]
      --basefee <BASEFEE>          Gas price [default: 0] [aliases: --gas-price, --price, --base-fee]
      --priority-fee <FEE>         EIP-1559 priority fee [aliases: --priorityfee, --tip]
      --sender <SENDER>            Transaction origin [default: 0xf39F...2266] [aliases: --from]
      --receiver <RECEIVER>        Transaction receiver [aliases: --to]
      --nonce <NONCE>              Transaction nonce
      --create <CREATE>            Create mode [possible values: true, false]
      --value <VALUE>              Value in wei (supports ether/gwei/wei suffixes)
      --input <INPUT>              Calldata as hex [aliases: --data]
      --inputfile <INPUTFILE>      Calldata from file [aliases: --datafile, --input-file, --data-file]
      --source-hash <HASH>         Source hash for deposit tx [aliases: --sourcehash]
      --mint <MINT>                ETH to mint for deposit tx (wei)
      --auth <AUTH>                EIP-7702 auth: AUTHORITY:NONCE->DELEGATION [aliases: --authorization]
      --access <ACCESS>            EIP-2930 access list: ADDRESS or ADDRESS:KEY1,KEY2,... [aliases: --accesslist]

State Options:
      --fork                             Fork state from remote RPC
      --fork.block <FORK_BLOCK>          Block to fork from (default: latest)
      --fork.rpc <FORK_RPC>              RPC URL [env: RPC_URL=] [default: http://localhost:8545]
      --prestate <PRESTATE>              JSON prestate file [aliases: --pre-state]
      --block-hash <BLOCK_HASHES>        BLOCKHASH overrides (repeatable) [aliases: --blockhash]
      --sender.balance <SENDER_BALANCE>  Sender balance (supports suffixes) [aliases: --from.balance]
      --faucet <FAUCET>                  Add ether: ADDRESS+=VALUE (repeatable)
      --balance <BALANCE>                Set balance: ADDRESS=VALUE (repeatable)
      --storage <STORAGE>                Set storage: ADDRESS:SLOT=VALUE (repeatable)

Chain Options:
      --spec <SPEC>                Spec [default: Rex4]
      --chain-id <CHAIN_ID>       Chain ID [default: 6342] [aliases: --chainid]

Block Options:
      --block.number <NUM>               Block number [default: 1]
      --block.coinbase <ADDR>            Coinbase [default: 0x0...0] [aliases: --block.beneficiary]
      --block.timestamp <TS>             Timestamp [default: 1]
      --block.gaslimit <GAS>             Gas limit [default: 10000000000]
      --block.basefee <FEE>              Base fee [default: 0]
      --block.difficulty <DIFF>          Difficulty [default: 0]
      --block.prevrandao <HASH>          Prevrandao [aliases: --block.random]
      --block.blobexcessgas <GAS>        Blob excess gas [default: 0]

External Environment Options:
      --bucket-capacity <BUCKET_ID:CAPACITY>  Bucket capacity (repeatable)

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
