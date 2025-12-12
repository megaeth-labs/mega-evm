# mega-evme run

A command-line tool for executing arbitrary EVM bytecode, similar to go-ethereum's `evm run` command. This tool provides a convenient way to test and debug EVM bytecode execution with full control over the execution environment.

## Table of Contents

- [Overview](#overview)
- [Basic Usage](#basic-usage)
- [Command Line Arguments](#command-line-arguments)
- [Features](#features)
  - [State Management](#state-management)
  - [SALT Bucket Capacity Configuration](#salt-bucket-capacity-configuration)
  - [Execution Tracing](#execution-tracing)
  - [Benchmarking](#benchmarking)
  - [Contract Creation](#contract-creation)
  - [Block Environment Configuration](#block-environment-configuration)
  - [Fork Selection](#fork-selection)
- [State File Format](#state-file-format)
- [Examples](#examples)
- [Architecture](#architecture)

## Overview

The `mega-evme run` command executes EVM bytecode in a controlled environment with configurable parameters. It supports:

- Arbitrary bytecode execution (call or create mode)
- State management (prestate loading and state dumping)
- SALT bucket capacity configuration
- Execution tracing with Geth-compatible output
- Performance benchmarking
- Full control over transaction and block environment

## Basic Usage

```bash
# Execute bytecode directly
mega-evme run 0x60016000526001601ff3

# Execute bytecode from file
mega-evme run --codefile contract.hex

# Execute with input data
mega-evme run 0x60016000526001601ff3 --input 0x1234567890

# Execute with custom gas limit
mega-evme run 0x60016000526001601ff3 --gas 5000000
```

## Command Line Arguments

### Code Input

- `CODE` (positional): EVM bytecode as hex string
- `--codefile <PATH>`: Path to file containing bytecode (use `-` for stdin)

### Transaction Configuration

- `--create`: Execute in create mode (deploy contract)
- `--gas <AMOUNT>`: Gas limit (default: 10000000)
- `--price <AMOUNT>`: Gas price in wei (default: 0)
- `--priorityfee <AMOUNT>`: EIP-1559 priority fee
- `--tx-type <TYPE>`: Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559)
- `--value <AMOUNT>`: Value to send with transaction (default: 0)
- `--sender <ADDRESS>`: Transaction sender (default: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266)
- `--receiver <ADDRESS>`: Transaction receiver (default: 0x0000000000000000000000000000000000000000)

### Input Data

- `--input <HEX>`: Input data as hex string
- `--inputfile <PATH>`: Path to file containing input data

### State Management

- `--prestate <PATH>`: JSON file with initial state
- `--dump`: Dump final state after execution
- `--dump.output <PATH>`: Output file for state dump (prints to console if not specified)
- `--sender.balance <AMOUNT>`: Initial balance to allocate to sender account

### SALT Configuration

- `--bucket-capacity <ID:CAPACITY>`: Configure bucket capacity (can be specified multiple times)
  - Format: `bucket_id:capacity`
  - Example: `--bucket-capacity 123:1000000`

### Tracing

- `--tracer trace`: Enable Geth-compatible execution tracing
- `--trace.disable-memory`: Disable memory capture in traces
- `--trace.disable-stack`: Disable stack capture in traces
- `--trace.disable-storage`: Disable storage capture in traces
- `--trace.enable-return-data`: Enable return data capture in traces
- `--trace.output <PATH>`: Output file for trace data

### Debugging

- `--statdump`: Display stack and memory information
- `--bench`: Benchmark execution time

### Chain Configuration

- `--state.fork <FORK>`: EVM fork/spec (default: mini-rex)
  - `mini-rex`: MegaETH MiniRex hardfork
  - `equivalence`: Optimism Isthmus compatibility mode
- `--state.chainid <ID>`: Chain ID (default: 6342)

### Block Environment

- `--block.number <NUM>`: Block number (default: 1)
- `--block.coinbase <ADDRESS>`: Block beneficiary/coinbase address
- `--block.timestamp <TIMESTAMP>`: Block timestamp (default: 1)
- `--block.gaslimit <AMOUNT>`: Block gas limit (default: 30000000)
- `--block.basefee <AMOUNT>`: EIP-1559 base fee (default: 0)
- `--block.difficulty <AMOUNT>`: Block difficulty (default: 0)
- `--block.prevrandao <HASH>`: Previous randao value (post-merge)
- `--block.blobexcessgas <AMOUNT>`: EIP-4844 blob excess gas (default: 0)

## Features

### State Management

The tool supports loading initial state from JSON files and dumping final state after execution.

#### Loading Prestate

```bash
# Load initial state from JSON file
mega-evme run 0x60016000526001601ff3 --prestate initial_state.json
```

The prestate file should contain account information in Ethereum JSON format (see [State File Format](#state-file-format)).

#### Dumping State

```bash
# Dump state to console
mega-evme run 0x60016000526001601ff3 --dump

# Save state to file
mega-evme run 0x60016000526001601ff3 --dump --dump.output final_state.json
```

#### Setting Sender Balance

By default, the sender account has zero balance. You can allocate balance explicitly:

```bash
# Allocate 1 ETH to sender
mega-evme run 0x60016000526001601ff3 --sender.balance 1000000000000000000
```

**Note**: Balance is only allocated when `--sender.balance` is specified. This gives you explicit control over account balances.

#### Round-Trip State Management

You can dump state, modify it, and reload it:

```bash
# 1. Execute and dump state
mega-evme run contract.hex --dump --dump.output state1.json

# 2. Modify state1.json as needed

# 3. Execute with modified state
mega-evme run next_contract.hex --prestate state1.json --dump --dump.output state2.json
```

### SALT Bucket Capacity Configuration

Configure SALT bucket capacities for testing storage gas costs:

```bash
# Configure single bucket
mega-evme run contract.hex --bucket-capacity 123:1000000

# Configure multiple buckets
mega-evme run contract.hex \
  --bucket-capacity 123:1000000 \
  --bucket-capacity 456:2000000 \
  --bucket-capacity 789:500000
```

The bucket capacity configuration uses the format `bucket_id:capacity` where:
- `bucket_id`: 32-bit bucket identifier
- `capacity`: Current capacity in bytes

This affects gas costs for storage operations in MiniRex hardfork.

### Execution Tracing

Enable Geth-compatible execution tracing to see opcode-level execution:

```bash
# Basic tracing
mega-evme run 0x60016000526001601ff3 --tracer trace

# Optimized tracing (disable memory and stack)
mega-evme run contract.hex \
  --tracer trace \
  --trace.disable-memory \
  --trace.disable-stack

# Save trace to file
mega-evme run contract.hex --tracer trace --trace.output trace.json
```

Trace output includes:
- Gas costs per operation
- Stack state (unless disabled)
- Memory state (unless disabled)
- Storage changes (unless disabled)
- Return data (if enabled)

### Benchmarking

Measure execution time:

```bash
mega-evme run contract.hex --bench
```

Output includes:
- Total execution time
- Gas used
- Execution result

### Contract Creation

Deploy contracts using create mode:

```bash
# Deploy contract (code is init code)
mega-evme run --create 0x6080604052...

# Deploy with constructor arguments
mega-evme run --create 0x6080604052... --input 0x0000000000000000000000001234...
```

In create mode:
- The bytecode is treated as init code
- Input data is appended to init code
- Returns the deployed contract address

### Block Environment Configuration

Configure block parameters for testing:

```bash
mega-evme run contract.hex \
  --block.number 1000 \
  --block.timestamp 1234567890 \
  --block.coinbase 0x1111111111111111111111111111111111111111 \
  --block.gaslimit 50000000 \
  --block.basefee 1000000000
```

This is useful for:
- Testing time-dependent contracts
- Simulating specific block conditions
- Testing EIP-1559 base fee mechanics

### Fork Selection

Choose between different EVM specifications:

```bash
# Use MiniRex hardfork (default)
mega-evme run contract.hex --state.fork mini-rex

# Use Optimism Isthmus compatibility
mega-evme run contract.hex --state.fork equivalence
```

Differences:
- **mini-rex**: Includes SALT dynamic gas costs, LOG cost increases, large contract support
- **equivalence**: Optimism Isthmus compatibility, parallel execution support

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
  },
  "0x0000000000000000000000000000000000100001": {
    "balance": "0x0",
    "nonce": "0x0",
    "code": "0x",
    "codeHash": "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
    "storage": {}
  }
}
```

### Field Descriptions

- **balance**: Account balance in wei (quantity format: 0x-prefixed hex without leading zeros)
- **nonce**: Transaction nonce (quantity format)
- **code**: Contract bytecode (hex string with 0x prefix)
- **codeHash**: Keccak256 hash of the code (hex string with 0x prefix, always 32 bytes)
- **storage**: Key-value map of storage slots (both keys and values use quantity format)

### Format Notes

- Uses Ethereum "quantity" format: `0x` prefix with no leading zeros (except `0x0` for zero)
- Empty code is represented as `"0x"`
- Empty storage is represented as `{}`
- All addresses are checksummed (though case-insensitive during parsing)

## Examples

### Example 1: Simple Execution

Execute bytecode that returns value 1:

```bash
mega-evme run 0x60016000526001601ff3
# Output: 0x01
```

Bytecode breakdown:
- `60 01`: PUSH1 0x01
- `60 00`: PUSH1 0x00
- `52`: MSTORE (store 0x01 at memory position 0)
- `60 01`: PUSH1 0x01
- `60 1f`: PUSH1 0x1f (31 bytes offset)
- `f3`: RETURN (return 1 byte starting at position 31)

### Example 2: State Persistence

Create initial state, execute, and verify:

```bash
# Create initial state file
cat > state.json << 'EOF'
{
  "0x1000000000000000000000000000000000000001": {
    "balance": "0x1000000",
    "nonce": "0x0",
    "code": "0x",
    "codeHash": "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
    "storage": {}
  }
}
EOF

# Execute with prestate
mega-evme run 0x60016000526001601ff3 --prestate state.json --dump
```

### Example 3: Contract Deployment

Deploy a simple storage contract:

```bash
# Init code that deploys runtime code
mega-evme run --create 0x600a600c600039600a6000f3602a60005260206000f3 --dump
```

### Example 4: Testing with Tracing

Debug contract execution with full traces:

```bash
mega-evme run contract.hex \
  --tracer trace \
  --trace.enable-return-data \
  --trace.output trace.json \
  --gas 10000000
```

### Example 5: SALT Bucket Testing

Test storage operations with bucket capacity limits:

```bash
# Set up account with code
cat > contract_state.json << 'EOF'
{
  "0x0000000000000000000000000000000000000001": {
    "balance": "0x0",
    "nonce": "0x0",
    "code": "0x60aa60005560bb60015560cc600255",
    "codeHash": "0xabcd...",
    "storage": {}
  }
}
EOF

# Execute with bucket capacity limits
mega-evme run 0x00 \
  --receiver 0x0000000000000000000000000000000000000001 \
  --prestate contract_state.json \
  --bucket-capacity 1:1000 \
  --bucket-capacity 2:2000 \
  --dump
```

### Example 6: Benchmarking Gas Costs

Compare gas usage between implementations:

```bash
# Benchmark first version
mega-evme run contract_v1.hex --bench

# Benchmark optimized version
mega-evme run contract_v2.hex --bench
```

## Architecture

### Module Structure

```
run/
├── mod.rs          # Module exports and utility functions
│                   # - load_code() / load_input(): Input loading
│                   # - parse_bucket_capacity(): Bucket config parsing
│                   # - StateDump: State serialization struct
│                   # - AccountState: Account data struct
└── cmd.rs          # Command implementation
                    # - Cmd: CLI argument struct
                    # - RunResult: Execution result struct
                    # - Main execution logic
```

### Key Components

#### `Cmd` Struct

Defines all CLI arguments using `clap`:
- Transaction configuration (gas, value, sender, etc.)
- State management (prestate, dump)
- Tracing options
- Block environment
- Fork selection
- SALT configuration

#### `StateDump` / `AccountState`

Handles state serialization/deserialization:
- **StateDump**: Root struct containing accounts map
- **AccountState**: Individual account data (balance, nonce, code, storage)
- Uses `alloy_serde` for proper Ethereum quantity format
- Implements `from_evm_state()` for conversion from REVM state

#### Execution Flow

1. **Input Loading**: Load bytecode and input data
2. **State Setup**: Create initial state (prestate + code + sender balance)
3. **Environment Configuration**: Setup cfg, block, tx, and external envs
4. **Execution**: Run transaction with optional tracing
5. **Output**: Print results, traces, and state dumps

### Dependencies

- **revm**: Core EVM execution engine
- **alloy-primitives**: Ethereum types (Address, U256, Bytes, B256)
- **alloy-serde**: Proper Ethereum serialization
- **revm-inspectors**: Execution tracing
- **clap**: CLI argument parsing
- **serde_json**: JSON serialization

### Design Decisions

1. **Explicit Balance Allocation**: Only allocate sender balance when `--sender.balance` is specified. This prevents implicit behavior and gives users full control.

2. **Built-in Serialization**: Use `alloy-primitives`' built-in serde implementations instead of custom serialization. This ensures compatibility with Ethereum tooling and reduces maintenance burden.

3. **Structured State**: Use `StateDump` and `AccountState` structs instead of manual JSON manipulation. This provides type safety and better error messages.

4. **Separation of Concerns**: Keep CLI parsing (`Cmd`), execution logic, and state management separate for better testability and maintainability.

5. **Flexible Output**: Support both console and file output for traces and state dumps, making it suitable for both interactive and automated use.

## See Also

- [MiniRex Specification](../../../../specs/MiniRex.md)
- [Project Architecture](../../../../ARCH.md)
- [Development Guide](../../../../DEV.md)
- [Main README](../../../../README.md)
