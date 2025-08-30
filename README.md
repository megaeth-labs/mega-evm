# MegaETH EVM

A specialized Ethereum Virtual Machine (EVM) implementation tailored for MegaETH specifications, built on top of [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/bluealloy/op-revm).

## Overview

This repository contains a customized version of the revm EVM implementation specifically designed for MegaETH. The implementation extends the Optimism EVM (op-revm) with MegaETH-specific modifications and optimizations to support high-performance blockchain operations.

## EVM Version

- **Base EVM**: [revm v27.1.0 (v83)](https://github.com/bluealloy/revm)
- **Optimism EVM**: [op-revm v8.1.0 (v83)](https://github.com/bluealloy/op-revm)
- **Alloy EVM**: [alloy-evm v0.15.0](https://github.com/alloy-rs/core)

## Key Features

- **Custom EVM Specifications**: EQUIVALENCE and MINI_REX specs - different EVM versions defined by mega-evm (EQUIVALENCE has the same semantics as Optimism EVM)
- **Enhanced Security**: Modified LOG opcodes with quadratic data cost and disabled SELFDESTRUCT
- **Increased Limits**: Support for larger contracts (512KB vs standard 24KB)
- **Parallel Execution Support**: Block environment access tracking to facilitate parallel execution conflict detection
- **Optimism Compatibility**: Built on top of op-revm for seamless integration

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

- **[ARCH.md](ARCH.md)**: Detailed implementation architecture and technical specifications
- **[DEV.md](DEV.md)**: Development guide.

## License

MIT OR Apache-2.0
