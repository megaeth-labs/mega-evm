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
| [`cache`](commands/cache.md)   | Offline RPC cache utilities (merge provider caches or capture envelopes)       |

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

## Global Options

These flags apply to all commands.

| Flag                | Default | Aliases          | Description                                                                                 |
| ------------------- | ------- | ---------------- | ------------------------------------------------------------------------------------------- |
| `-v`                | 0       | N/A              | Increase log verbosity (`-v`=error, `-vv`=warn, `-vvv`=info, `-vvvv`=debug, `-vvvvv`=trace) |
| `--log.file <PATH>` | stderr  | `--log-file`     | Write logs to a file instead of stderr                                                      |
| `--log.no-color`    | `false` | `--log-no-color` | Disable colored console output                                                              |

## Exit codes

Every command reports its outcome through the same set of exit codes, so a pipeline can branch on the process status without parsing output.

| Code | Class                   | Meaning                                                                                                                                                      |
| ---- | ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `0`  | success                 | The command completed; with [`--verify-receipt`](commands/replay.md#receipt-verification), every verification matched.                                       |
| `1`  | `execution-error`       | Execution or internal error: an EVM or setup failure, bad input (including a usage error), or a definitive negative answer such as an unknown transaction.   |
| `2`  | `verification-mismatch` | The run completed, but at least one replay did not reproduce its on-chain receipt.                                                                           |
| `3`  | `rpc-failure`           | An RPC or transport call failed — endpoint unreachable, transport error, or an offline replay file that holds no response for a request the run had to make. |

Codes `1` and `3` separate the two ways a question can go wrong: `1` means the tool answered, and the answer is negative; `3` means the question went unanswered, so retrying against a healthy endpoint may still produce a result.
A state read that fails while the EVM is executing — an offline replay file without the response, or an endpoint that dies mid-transaction — belongs to `3` as well, even though it surfaces as a block execution error.
A hash the endpoint itself listed in a block body but then resolves to null belongs to `3` too: the null contradicts an answer the endpoint already gave, so it describes an inconsistent endpoint (a reorg, or a load-balanced backend serving divergent views) rather than an unknown transaction.
Only a hash the caller supplied directly stays in `1` when it resolves to null, since nothing the endpoint served claimed it existed.
Two paths cannot be classified that way: a read that fails inside the pre-block system calls (EIP-4788 beacon root, EIP-2935 block hashes) or inside the sandboxed execution of the keyless-deploy system contract has its cause rendered into a message by the layer that raises it, so `mega-evme` cannot tell it from an execution failure and reports `1`.

A batch run (`--tx-file` / `--block`) reports every target on its own line and then exits once for the run as a whole, ranking the failure classes it saw: any execution or internal failure exits `1`, otherwise any RPC failure exits `3`, otherwise any verification mismatch exits `2`.
A target that never replayed was also never verified, which is why an infrastructure failure outranks a mismatch.

On failure the run also prints a report: one `error: <message>` line per failure on stderr, plus — with `--json` — a structured object as the last line of stdout, so a machine-readable run never ends with empty output.
A run reports more than one line when a secondary failure must not go unnoticed but does not own the exit code — an unwritable [`--rpc.capture-file`](commands/replay.md#--rpccapture-file-path) behind an earlier replay failure, for instance.
The structured object always carries the failure the exit code came from.

```json
{ "error": { "code": 3, "kind": "rpc-failure", "message": "RPC error: …" } }
```

In batch mode that object follows the per-target lines, whose own `error.kind` (`not_found`, `pending`, `rpc`, `execution`) describes why one target failed and is independent of the run-level class above.
A usage error is reported the same way: the argument parser prints its own report and usage block on stderr, and a `--json` run still ends its stdout with the object, whose `message` is a one-line summary of the parse failure.

New failure classes are added as new codes; the meaning of an existing code does not change.

## Read more

- **[Cookbook](cookbook.md)** — Real-world recipes and worked examples.
- **[Commands](commands/run.md)** — Detailed reference for each subcommand.
- **[Configuration](configuration/chain-and-spec.md)** — Shared options: spec selection, block environment, state management, SALT buckets.
- **[Tracing](tracing/overview.md)** — Execution tracing with opcode, call, and pre-state tracers.
- **[Transaction Types](transaction-types.md)** — Legacy, EIP-2930, EIP-1559, EIP-7702, and Deposit transactions.
