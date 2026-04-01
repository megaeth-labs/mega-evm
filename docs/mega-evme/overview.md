---
description: Execute and debug EVM bytecode locally with MegaETH's gas model, tracing, and state forking.
---

# mega-evme

A command-line tool for executing and debugging EVM bytecode on MegaETH specs.
Similar to go-ethereum's `evm` command, `mega-evme` provides full control over the execution environment — including MegaETH-specific features like SALT bucket configuration, multidimensional resource limits, and gas detention.

## Installation

```bash
cargo build --release -p mega-evme
# Binary: target/release/mega-evme
```

## Commands

| Command                        | Description                                                                    |
| ------------------------------ | ------------------------------------------------------------------------------ |
| [`run`](commands/run.md)       | Execute arbitrary EVM bytecode directly                                        |
| [`tx`](commands/tx.md)         | Run a transaction with full transaction context and optional RPC state forking |
| [`replay`](commands/replay.md) | Replay an existing on-chain transaction from RPC                               |

## Quick Start

Execute bytecode that returns the value `1`:

```bash
mega-evme run 0x60016000526001601ff3
```

Deploy a contract and dump the resulting state:

```bash
mega-evme run --create true 0x6080604052... --dump
```

Fork remote state and call a contract:

```bash
# Call WETH.name() on MegaETH mainnet
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x06fdde03
```

Replay an on-chain transaction with execution tracing:

```bash
# Replace 0xabc123... with a real transaction hash from MegaETH mainnet
mega-evme replay 0xabc123... \
  --rpc https://mainnet.megaeth.com/rpc \
  --trace --tracer opcode
```

## Global Options

These flags apply to all commands.

| Flag                | Default | Aliases          | Description                                                                                 |
| ------------------- | ------- | ---------------- | ------------------------------------------------------------------------------------------- |
| `-v`                | 0       | N/A              | Increase log verbosity (`-v`=error, `-vv`=warn, `-vvv`=info, `-vvvv`=debug, `-vvvvv`=trace) |
| `--log.file <PATH>` | stderr  | `--log-file`     | Write logs to a file instead of stderr                                                      |
| `--log.no-color`    | `false` | `--log-no-color` | Disable colored console output                                                              |

## Read more

- **[Cookbook](cookbook.md)** — Real-world recipes and worked examples.
- **[Commands](commands/run.md)** — Detailed reference for each subcommand.
- **[Configuration](configuration/chain-and-spec.md)** — Shared options: spec selection, block environment, state management, SALT buckets.
- **[Tracing](tracing/overview.md)** — Execution tracing with opcode, call, and pre-state tracers.
- **[Transaction Types](transaction-types.md)** — Legacy, EIP-2930, EIP-1559, EIP-7702, and Deposit transactions.
