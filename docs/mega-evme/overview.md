---
description: Execute and debug EVM bytecode locally with MegaETH's gas model, tracing, and state forking.
---

# mega-evme

A command-line tool for executing and debugging EVM bytecode on MegaETH specs.
Similar to go-ethereum's `evm` command, `mega-evme` provides full control over the execution environment — including MegaETH-specific features like SALT bucket configuration, multidimensional resource limits, and gas detention.

`mega-evme` carries two engines and runs every command on the one its spec names.
`Satin` runs on the Satin engine.
`Equivalence` through `Rex6`, the specs the chain has run, run on the legacy engine, the released `mega-evme` and `mega-evm` 1.7.1, so they print exactly what the 1.7.1 tool printed.
See [Chain and Spec](configuration/chain-and-spec.md#engines).

## Installation

```bash
cargo build --release --manifest-path bin/mega-evme/Cargo.toml
# Binary: target/release/mega-evme
```

The tool is selected by its manifest path rather than with `-p mega-evme`, because it links the released `mega-evme` 1.7.1, a package of the same name.
`--no-default-features` builds a binary that runs Satin only, without the legacy engine.

### Prebuilt binary

Every release also attaches a prebuilt `mega-evme` (Linux x86_64, built on `ubuntu-24.04`) and a `SHA256SUMS` file to its [GitHub Release](https://github.com/megaeth-labs/mega-evm/releases).
Download both, verify the checksum, and make the binary executable:

```bash
sha256sum --check --ignore-missing SHA256SUMS
chmod +x mega-evme
./mega-evme --version
```

The same binary is published to the MegaETH artifact registry for internal use.

## Commands

| Command                        | Description                                                                    |
| ------------------------------ | ------------------------------------------------------------------------------ |
| [`run`](commands/run.md)       | Execute arbitrary EVM bytecode directly                                        |
| [`tx`](commands/tx.md)         | Run a transaction with full transaction context and optional RPC state forking |
| [`replay`](commands/replay.md) | Replay an on-chain transaction, or whole blocks compared with the chain        |

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
  --fork --rpc https://mainnet.megaeth.com/rpc \
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

Run the same bytecode on Satin, which adds the gas by ledger to the output:

```bash
mega-evme run 0x60016000526001601ff3 --spec Satin --json
```

Replay a range of blocks and check every receipt against the chain:

```bash
mega-evme replay --block 26400000..26400009 \
  --rpc https://mainnet.megaeth.com/rpc --block-cache ./blocks --verify
```

## Global Options

These flags apply to all commands.

| Flag                | Default | Aliases          | Description                                                                                 |
| ------------------- | ------- | ---------------- | ------------------------------------------------------------------------------------------- |
| `-v`                | 0       | N/A              | Increase log verbosity (`-v`=error, `-vv`=warn, `-vvv`=info, `-vvvv`=debug, `-vvvvv`=trace) |
| `--log.file <PATH>` | stderr  | `--log-file`     | Write logs to a file instead of stderr                                                      |
| `--log.no-color`    | `false` | `--log-no-color` | Disable colored console output                                                              |

## Output and Exit Status

An error is written once, as one line on stderr, and nothing is written to stdout.
The exit code is 1, whether or not stderr could take the line.
A command that prints its result to a stdout that was closed (`mega-evme … | head -0`) ends quietly with the status it had.

Text a person reads on a terminal is filtered: escape sequences, control characters (the C0 and C1 ranges, including the one-character CSI and OSC introducers), DEL and the bidirectional overrides are dropped, and line breaks and tabs stay.
This covers error lines and a transaction's revert reason.
`--json` output is not filtered: a JSON string carries the same characters, escaped.

The URL given to `--rpc` is written to no log line, error or cache file name with its userinfo (`user:password@`).
The userinfo is the part of the authority before its last `@`, and it is removed from a URL written without its scheme too.
A credential anywhere else in the URL, such as an API key in the path or the query, is not recognised and is printed with the URL.
The URL has to start with `http://` or `https://`.

## Read more

- **[Cookbook](cookbook.md)** — Real-world recipes and worked examples.
- **[Commands](commands/run.md)** — Detailed reference for each subcommand.
- **[Configuration](configuration/chain-and-spec.md)** — Shared options: spec selection, block environment, state management, SALT buckets.
- **[Tracing](tracing/overview.md)** — Execution tracing with opcode, call, and pre-state tracers.
- **[Transaction Types](transaction-types.md)** — Legacy, EIP-2930, EIP-1559, EIP-7702, and Deposit transactions.
