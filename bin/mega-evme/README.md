# mega-evme

A command-line tool for executing and debugging EVM bytecode, similar to go-ethereum's `evm` command. This tool provides a convenient way to test, debug, and replay EVM transactions with full control over the execution environment.

## Table of Contents

- [Overview](#overview)
- [Installation](#installation)
- [Commands](#commands)
  - [run](#run-command)
  - [tx](#tx-command)
  - [replay](#replay-command)
- [Common Options](#common-options)
- [Examples](#examples)

## Overview

`mega-evme` provides three main commands for EVM execution:

| Command  | Description                                     |
| -------- | ----------------------------------------------- |
| `run`    | Execute arbitrary EVM bytecode directly         |
| `tx`     | Run a transaction with full transaction context |
| `replay` | Replay an existing transaction from RPC         |

## Installation

```bash
# Build from source
cargo build --release -p mega-evme

# The binary will be at target/release/mega-evme
```

## Commands

### run Command

Execute arbitrary EVM bytecode in a controlled environment.

```bash
# Execute bytecode directly
mega-evme run 0x60016000526001601ff3

# Execute bytecode from file
mega-evme run --codefile contract.hex

# Execute with input data
mega-evme run 0x60016000526001601ff3 --input 0x1234567890

# Deploy a contract (create mode)
mega-evme run --create 0x6080604052...
```

#### Code Input Options

| Option              | Description                                          |
| ------------------- | ---------------------------------------------------- |
| `CODE`              | EVM bytecode as hex string (positional argument)     |
| `--codefile <PATH>` | Path to file containing bytecode (use `-` for stdin) |

#### Transaction Options

| Option                   | Default                                    | Description                                         |
| ------------------------ | ------------------------------------------ | --------------------------------------------------- |
| `--create`               | false                                      | Execute in create mode (deploy contract)            |
| `--gas <AMOUNT>`         | 10000000                                   | Gas limit                                           |
| `--price <AMOUNT>`       | 0                                          | Gas price in wei                                    |
| `--priorityfee <AMOUNT>` | -                                          | EIP-1559 priority fee                               |
| `--tx-type <TYPE>`       | 0                                          | Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559) |
| `--value <AMOUNT>`       | 0                                          | Value to send with transaction                      |
| `--sender <ADDRESS>`     | 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 | Transaction sender                                  |
| `--receiver <ADDRESS>`   | 0x0000...0000                              | Transaction receiver                                |

#### Input Data Options

| Option               | Description                        |
| -------------------- | ---------------------------------- |
| `--input <HEX>`      | Input data as hex string           |
| `--inputfile <PATH>` | Path to file containing input data |

For detailed documentation, see [run/README.md](src/run/README.md).

---

### tx Command

Run a transaction with full transaction context. Similar to `run`, but with additional support for forking state from remote RPC endpoints.

```bash
# Execute a simple transaction
mega-evme tx --input 0x1234 --receiver 0x1234...

# Fork state from remote RPC
mega-evme tx --fork --fork.rpc https://rpc.example.com --receiver 0x1234...

# Fork from specific block
mega-evme tx --fork --fork.block 12345678 --receiver 0x1234...
```

#### Transaction Options

| Option                   | Default       | Description                          |
| ------------------------ | ------------- | ------------------------------------ |
| `--tx-type <TYPE>`       | 0             | Transaction type                     |
| `--gas <AMOUNT>`         | 10000000      | Gas limit                            |
| `--basefee <AMOUNT>`     | 0             | Gas price (alias: `--gas-price`)     |
| `--priorityfee <AMOUNT>` | -             | EIP-1559 priority fee                |
| `--sender <ADDRESS>`     | 0xf39F...2266 | Transaction origin (alias: `--from`) |
| `--receiver <ADDRESS>`   | 0x0000...0000 | Transaction receiver (alias: `--to`) |
| `--nonce <NONCE>`        | -             | Transaction nonce                    |
| `--create`               | false         | Create mode instead of call          |
| `--value <AMOUNT>`       | 0             | Transaction value                    |
| `--input <HEX>`          | -             | Transaction calldata                 |
| `--inputfile <PATH>`     | -             | File containing calldata             |

#### Fork Options

| Option                  | Default               | Description                          |
| ----------------------- | --------------------- | ------------------------------------ |
| `--fork`                | false                 | Enable state forking from RPC        |
| `--fork.rpc <URL>`      | http://localhost:8545 | RPC URL for forking (env: `RPC_URL`) |
| `--fork.block <NUMBER>` | latest                | Block number to fork from            |

---

### replay Command

Replay an existing transaction from RPC. Fetches the transaction and its execution context from a remote node and re-executes it locally.

```bash
# Replay a transaction
mega-evme replay 0x1234...txhash...5678

# Replay with custom RPC
mega-evme replay 0x1234...txhash --rpc https://rpc.example.com

# Replay with execution trace
mega-evme replay 0x1234...txhash --tracer trace
```

#### Arguments

| Argument  | Description                           |
| --------- | ------------------------------------- |
| `TX_HASH` | Transaction hash to replay (required) |

#### Options

| Option        | Default               | Description                       |
| ------------- | --------------------- | --------------------------------- |
| `--rpc <URL>` | http://localhost:8545 | RPC URL to fetch transaction from |

---

## Common Options

These options are available across all commands.

### State Management

| Option                      | Description                        |
| --------------------------- | ---------------------------------- |
| `--prestate <PATH>`         | JSON file with initial state       |
| `--sender.balance <AMOUNT>` | Initial balance for sender account |
| `--dump`                    | Dump final state after execution   |
| `--dump.output <PATH>`      | Output file for state dump         |

### Chain Configuration

| Option                 | Default | Description                               |
| ---------------------- | ------- | ----------------------------------------- |
| `--state.fork <FORK>`  | MiniRex | Hardfork: `MiniRex`, `Equivalence`, `Rex` |
| `--state.chainid <ID>` | 6342    | Chain ID                                  |

### Block Environment

| Option                        | Default     | Description               |
| ----------------------------- | ----------- | ------------------------- |
| `--block.number <NUM>`        | 1           | Block number              |
| `--block.coinbase <ADDR>`     | 0x0...0     | Block beneficiary address |
| `--block.timestamp <TS>`      | 1           | Block timestamp           |
| `--block.gaslimit <GAS>`      | 10000000000 | Block gas limit           |
| `--block.basefee <FEE>`       | 0           | EIP-1559 base fee         |
| `--block.difficulty <DIFF>`   | 0           | Block difficulty          |
| `--block.prevrandao <HASH>`   | 0x0...0     | Previous randao value     |
| `--block.blobexcessgas <GAS>` | 0           | EIP-4844 blob excess gas  |

### SALT Bucket Configuration

| Option                       | Description                                 |
| ---------------------------- | ------------------------------------------- |
| `--bucket-capacity <ID:CAP>` | Configure bucket capacity (can be repeated) |

```bash
# Configure multiple buckets
mega-evme run contract.hex \
  --bucket-capacity 123:1000000 \
  --bucket-capacity 456:2000000
```

### Execution Tracing

| Option                       | Description                              |
| ---------------------------- | ---------------------------------------- |
| `--tracer trace`             | Enable Geth-compatible execution tracing |
| `--trace.disable-memory`     | Disable memory capture in traces         |
| `--trace.disable-stack`      | Disable stack capture in traces          |
| `--trace.disable-storage`    | Disable storage capture in traces        |
| `--trace.enable-return-data` | Enable return data capture               |
| `--trace.output <PATH>`      | Output file for trace data               |

---

## Examples

### Example 1: Simple Bytecode Execution

```bash
# Execute bytecode that returns value 1
mega-evme run 0x60016000526001601ff3
```

### Example 2: Contract Deployment

```bash
# Deploy a contract with init code
mega-evme run --create 0x6080604052... --dump
```

### Example 3: Transaction with State Fork

```bash
# Fork mainnet state and execute against a contract
mega-evme tx \
  --fork \
  --fork.rpc https://eth-mainnet.example.com \
  --receiver 0xContractAddress \
  --input 0xMethodSelector...
```

### Example 4: Replay with Tracing

```bash
# Replay a transaction with full execution trace
mega-evme replay 0xTransactionHash \
  --rpc https://rpc.example.com \
  --tracer trace \
  --trace.output trace.json
```

### Example 5: State Persistence

```bash
# Execute and dump state
mega-evme run contract.hex --dump --dump.output state1.json

# Continue execution with saved state
mega-evme run next_contract.hex --prestate state1.json --dump
```

### Example 6: Testing with Custom Block Environment

```bash
mega-evme tx \
  --receiver 0x1234... \
  --input 0x... \
  --block.number 1000 \
  --block.timestamp 1234567890 \
  --state.fork MiniRex
```

## State File Format

State files use JSON format with Ethereum quantity encoding:

```json
{
  "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {
    "balance": "0xde0b6b3a7640000",
    "nonce": "0x1",
    "code": "0x60806040...",
    "codeHash": "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
    "storage": {
      "0x0": "0x2a",
      "0x1": "0x64"
    }
  }
}
```

## See Also

- [run Command Documentation](src/run/README.md)
- [MiniRex Specification](../../hardfork-spec/MiniRex.md)
