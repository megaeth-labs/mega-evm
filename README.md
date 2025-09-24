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
- **Dynamic Gas Costs**: SALT bucket-based scaling preventing state bloat
- **Massive Gas Increases**: 100x increases for LOG operations and calldata costs
- **SELFDESTRUCT Prohibition**: Complete disabling for contract integrity
- **Large Contract Support**: 512 KB contracts (21x increase from 24 KB)
- **Transaction Limits**: 3.125 MB data and 1,000 KV update limits with enforcement
- **Enhanced Security**: Comprehensive limit enforcement with OutOfGas penalties

For complete MiniRex specification, see **[MiniRex.md](./MiniRex.md)**.

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

- **[MiniRex.md](MiniRex.md)**: Complete MiniRex hardfork specification and semantic changes
- **[ARCH.md](ARCH.md)**: Detailed implementation architecture and technical specifications
- **[DEV.md](DEV.md)**: Development guide and testing information

## License

MIT OR Apache-2.0
