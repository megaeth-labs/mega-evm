# MegaETH EVM

A specialized Ethereum Virtual Machine (EVM) implementation tailored for MegaETH specifications, built on top of [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/bluealloy/op-revm).

## Overview

This repository contains a customized version of the revm EVM implementation specifically designed for MegaETH. The implementation extends the Optimism EVM (op-revm) with MegaETH-specific modifications and optimizations to support high-performance blockchain operations.

## EVM Version

- **Base EVM**: [revm v27.1.0 (v83)](https://github.com/bluealloy/revm)
- **Optimism EVM**: [op-revm v8.1.0 (v83)](https://github.com/bluealloy/op-revm)
- **Alloy EVM**: [alloy-evm v0.15.0](https://github.com/alloy-rs/core)

## Terminology: Spec vs Hardfork

This codebase distinguishes between two related concepts:

- **Spec (`MegaSpecId`)**: Defines EVM behavior - what the EVM does. Values: `EQUIVALENCE`, `MINI_REX`, `REX`, `REX1`, `REX2`
- **Hardfork (`MegaHardfork`)**: Defines network upgrade events - when specs are activated. Values: `MiniRex`, `MiniRex1`, `MiniRex2`, `Rex`, `Rex1`, `Rex2`

Multiple hardforks can map to the same spec. For example, both `MiniRex` and `MiniRex2` hardforks use the `MINI_REX` spec.

## Key Features

### EQUIVALENCE Spec

- **Optimism Compatibility**: Maintains full compatibility with Optimism Isthmus EVM
- **Parallel Execution Support**: Block environment access tracking for conflict detection

### MINI_REX Spec

- **Multidimensional Gas Model**: Independent tracking for compute gas (1B), data size (3.125 MB), and KV updates (125K)
- **Compute Gas Tracking**: Separate limit for computational work with gas detention for volatile data access
- **Dynamic Gas Costs**: SALT bucket-based scaling preventing state bloat
- **Split LOG Costs**: Compute gas (standard) + storage gas (10× multiplier) for independent resource pricing
- **SELFDESTRUCT Prohibition**: Complete disabling for contract integrity
- **Large Contract Support**: 512 KB contracts (21x increase from 24 KB)
- **Gas Detention**: Volatile data access (block env, beneficiary, oracle) triggers gas limiting with refunds
- **Enhanced Security**: Comprehensive limit enforcement preserving remaining gas on limit violations

For complete MiniRex specification, see **[MiniRex.md](./specs/MiniRex.md)**.

### REX Spec

- **Refined Storage Gas Economics**: Optimized storage gas formulas with gradual scaling (20K-32K base costs vs. MiniRex's 2M)
- **Transaction Intrinsic Storage Gas**: 39,000 storage gas baseline for all transactions (total 60K with compute gas)
- **Zero Cost Fresh Storage**: Storage operations in minimum-sized SALT buckets charge 0 storage gas
- **Separate Contract Creation Cost**: Distinct storage gas for contract creation (32K base) vs. account creation (25K base)
- **Critical Security Fixes**: DELEGATECALL, STATICCALL, and CALLCODE now properly enforce 98/100 gas forwarding and oracle access detection
- **MiniRex Foundation**: Inherits all MiniRex features including multidimensional gas model, compute gas detention, and enhanced security

For complete Rex specification, see **[Rex.md](./specs/Rex.md)**.

### REX1 Spec

- **Limit Reset Fix**: Resets compute gas limits at the start of each transaction
- **No Other Behavioral Changes**: Inherits Rex semantics fully

For complete Rex1 specification, see **[Rex1.md](./specs/Rex1.md)**.

### REX2 Spec

- **SELFDESTRUCT Restored**: Re-enabled with EIP-6780 semantics
- **KeylessDeploy System Contract**: Enables keyless deployment (Nick's Method) with custom gas limits
- **Rex1 Baseline**: Inherits Rex1 behavior for all other features

For complete Rex2 specification, see **[Rex2.md](./specs/Rex2.md)**.

## Quick Start

### Basic Usage

```rust
use mega_evm::{Context, Evm, SpecId, Transaction};
use revm::{
    context::TxEnv,
    database::{CacheDB, EmptyDB},
    inspector::NoOpInspector,
    primitives::TxKind,
};

// Create EVM instance with MINI_REX spec
let mut db = CacheDB::<EmptyDB>::default();
let spec = SpecId::MINI_REX;
let mut context = Context::new(db, spec);
let mut evm = Evm::new(context, NoOpInspector);

// Execute transaction
let tx = Transaction {
    base: TxEnv {
        caller: address!("..."),
        kind: TxKind::Call(target_address),
        data: Bytes::default(),
        value: U256::ZERO,
        gas_limit: 1000000,
        ..Default::default()
    },
    ..Default::default()
};

let result = alloy_evm::Evm::transact_raw(&mut evm, tx)?;
```

## Command Line Tool: `mega-evme`

The `mega-evme` binary provides a command-line interface for executing and debugging EVM transactions, similar to go-ethereum's `evm` tool.

### Installation

```bash
cargo build --release -p mega-evme
# Binary will be at ./target/release/mega-evme
```

### Commands

| Command  | Description                                     |
| -------- | ----------------------------------------------- |
| `run`    | Execute arbitrary EVM bytecode directly         |
| `tx`     | Run a transaction with full transaction context |
| `replay` | Replay an existing transaction from RPC         |

### Basic Usage

```bash
# Execute bytecode directly
mega-evme run 0x60016000526001601ff3

# Run a transaction with state forking
mega-evme tx --fork --fork.rpc https://rpc.example.com \
  --receiver 0x1234... --input 0x...

# Replay a transaction from RPC
mega-evme replay 0xTxHash --rpc https://rpc.example.com
```

### Spec Selection

Spec names are case-sensitive and match `MegaSpecId` strings: `Equivalence`, `MiniRex`, `Rex`,
`Rex1`, `Rex2`. Use `--spec` for `run`/`tx`, and `--override.spec` for `replay`.

```bash
# Run with a specific spec
mega-evme run 0x60016000526001601ff3 --spec Rex2
```

### Transaction Types

The `tx` command supports multiple transaction types with type-specific options:

| Type | Name     | Specific Options                                |
| ---- | -------- | ----------------------------------------------- |
| 0    | Legacy   | -                                               |
| 1    | EIP-2930 | `--access ADDRESS:KEY1,KEY2,...`                |
| 2    | EIP-1559 | `--priority-fee`, `--access`                    |
| 4    | EIP-7702 | `--auth AUTHORITY:NONCE->DELEGATION`, `--access`|
| 126  | Deposit  | `--source-hash`, `--mint`                       |

For detailed documentation, see [mega-evme README](bin/mega-evme/README.md).

## Development

### Cloning the Repository

This repository uses git submodules. Clone with submodules:

```bash
git clone --recursive https://github.com/megaeth-labs/mega-evm.git
```

Or if you've already cloned, initialize submodules:

```bash
git submodule update --init --recursive
```

### Building

```bash
cargo build
```

### Testing

```bash
cargo test
```

### Running Examples

```bash
cargo run --example block_env_tracking
```

## Documentation

- **[ARCH.md](ARCH.md)**: Detailed implementation architecture and technical specifications

## License

MIT OR Apache-2.0
