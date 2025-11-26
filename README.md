# MegaETH EVM

A specialized Ethereum Virtual Machine (EVM) implementation tailored for MegaETH specifications, built on top of [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/bluealloy/op-revm).

## Overview

This repository contains a customized version of the revm EVM implementation specifically designed for MegaETH. The implementation extends the Optimism EVM (op-revm) with MegaETH-specific modifications and optimizations to support high-performance blockchain operations.

## EVM Version

- **Base EVM**: [revm v27.1.0 (v83)](https://github.com/bluealloy/revm)
- **Optimism EVM**: [op-revm v8.1.0 (v83)](https://github.com/bluealloy/op-revm)
- **Alloy EVM**: [alloy-evm v0.15.0](https://github.com/alloy-rs/core)

## Key Features

### EQUIVALENCE Specification

- **Optimism Compatibility**: Maintains full compatibility with Optimism Isthmus EVM
- **Parallel Execution Support**: Block environment access tracking for conflict detection

### MINI_REX Specification (Hardfork)

- **Multidimensional Gas Model**: Independent tracking for compute gas (1B), data size (3.125 MB), and KV updates (125K)
- **Compute Gas Tracking**: Separate limit for computational work with gas detention for volatile data access
- **Dynamic Gas Costs**: SALT bucket-based scaling preventing state bloat
- **Split LOG Costs**: Compute gas (standard) + storage gas (10× multiplier) for independent resource pricing
- **SELFDESTRUCT Prohibition**: Complete disabling for contract integrity
- **Large Contract Support**: 512 KB contracts (21x increase from 24 KB)
- **Gas Detention**: Volatile data access (block env, beneficiary, oracle) triggers gas limiting with refunds
- **Enhanced Security**: Comprehensive limit enforcement preserving remaining gas on limit violations

For complete MiniRex specification, see **[MiniRex.md](./hardfork-spec/MiniRex.md)**.

### REX Specification (Hardfork)

- **Refined Storage Gas Economics**: Optimized storage gas formulas with gradual scaling (20K-32K base costs vs. MiniRex's 2M)
- **Transaction Intrinsic Storage Gas**: 39,000 storage gas baseline for all transactions (total 60K with compute gas)
- **Zero Cost Fresh Storage**: Storage operations in minimum-sized SALT buckets charge 0 storage gas
- **Separate Contract Creation Cost**: Distinct storage gas for contract creation (32K base) vs. account creation (25K base)
- **Critical Security Fixes**: DELEGATECALL, STATICCALL, and CALLCODE now properly enforce 98/100 gas forwarding and oracle access detection
- **MiniRex Foundation**: Inherits all MiniRex features including multidimensional gas model, compute gas detention, and enhanced security

For complete Rex specification, see **[Rex.md](./hardfork-spec/Rex.md)**.

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

The `mega-evme` binary provides a command-line interface for executing arbitrary EVM bytecode, similar to go-ethereum's `evm` tool.

### Installation

```bash
cargo build --release
# Binary will be at ./target/release/mega-evme
```

### Basic Usage

```bash
# Execute bytecode directly
mega-evme run 0x60016000526001601ff3

# Execute bytecode from file
mega-evme run --codefile contract.hex

# Execute with custom parameters
mega-evme run 0x60016000526001601ff3 \
  --gas 1000000 \
  --sender 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
```

More details can be found [here](bin/mega-evme/src/run/README.md).

## Development

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
- **[DEV.md](DEV.md)**: Development guide and testing information

## License

MIT OR Apache-2.0
