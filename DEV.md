# MegaETH EVM Development Guide

This document provides development-related information for the MegaETH EVM project.

## MiniRex Hardfork Specification

The MegaETH EVM implements the **MiniRex hardfork** which introduces significant changes:

### Key Features
- **Contract Size Limits**: Increased to 512 KB (from 24 KB)
- **SELFDESTRUCT Prohibition**: Complete disabling of SELFDESTRUCT opcode
- **Dynamic Gas Costs**: SALT bucket-based scaling for storage and account operations
- **Data/KV Limits**: Transaction limits of 3.125 MB data and 1,000 KV updates
- **100x Gas Increases**: LOG operations, calldata costs dramatically increased

For complete specification details, see [MiniRex.md](./MiniRex.md).

## Development Setup

### Building

```bash
cargo build
```

### Testing

```bash
cargo test
```

## Project Structure

```
mega-evm/
├── src/
│   ├── lib.rs          # Main library entry point and public API
│   ├── spec.rs         # EVM specification definitions and constants
│   ├── constants.rs    # MiniRex specification constants
│   ├── instructions.rs # Custom instruction implementations (LOG, SSTORE, CREATE, CALL)
│   ├── context.rs      # Extended EVM context with tracking
│   ├── handler.rs      # Custom EVM handler with MiniRex modifications
│   ├── evm.rs          # Main EVM implementation
│   ├── gas.rs          # Dynamic gas cost oracle (SALT bucket-based)
│   ├── block.rs        # Block environment tracking
│   ├── host.rs         # Host interface extensions
│   ├── types.rs        # Type definitions and enums
│   ├── limit/          # Data size and KV update limit enforcement
│   │   ├── mod.rs      # Main limit coordination
│   │   ├── data_size.rs # Transaction data size tracking
│   │   └── kv_update.rs # Key-value update counting
│   └── test_utils/     # Testing utilities
├── tests/              # Integration tests
│   ├── contract_size_limit.rs # 512 KB contract size tests
│   ├── disallow_selfdestruct.rs # SELFDESTRUCT prohibition tests
│   ├── gas.rs          # Dynamic gas cost tests
│   └── additional_limit.rs # Data/KV limit tests
├── examples/
│   └── block_env_tracking.rs # Block environment access tracking demo
└── MiniRex.md          # Complete MiniRex specification
```

## Development Workflow

### Adding New Features

1. Create a feature branch from main
2. Implement the feature with appropriate tests
3. Update documentation in ARCH.md if needed
4. Run tests to ensure everything works
5. Submit a pull request

### Testing Guidelines

- Write unit tests for new functionality
- Include integration tests for complex features
- Ensure all tests pass before submitting PRs
- Use the `test-utils` feature for testing utilities

### MiniRex-Specific Testing

Key test categories for MiniRex features:

- **Gas Cost Tests**: Verify dynamic scaling based on SALT bucket capacity
- **Contract Size Tests**: Ensure 512 KB limit enforcement
- **SELFDESTRUCT Tests**: Confirm complete prohibition (InvalidFEOpcode)
- **Limit Tests**: Validate data size (3.125 MB) and KV update (1,000) limits
- **Instruction Tests**: Test LOG, SSTORE, CREATE, CALL modifications

### Code Style

Run formatting and linting:
```bash
cargo fmt
cargo check
cargo clippy
```

## Dependencies

### Core Dependencies

- **revm**: 27.1.0 (v83) - Core EVM implementation
- **op-revm**: 8.1.0 (v83) - Optimism EVM extensions
- **alloy-evm**: 0.15.0 - EVM primitives and utilities
- **alloy-primitives**: 1.3.0 - Ethereum primitives