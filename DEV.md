# MegaETH EVM Development Guide

This document provides development-related information for the MegaETH EVM project.

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
│   ├── instructions.rs # Custom instruction implementations
│   ├── context.rs      # Extended EVM context with tracking
│   ├── handler.rs      # Custom EVM handler
│   ├── evm.rs          # Main EVM implementation
│   ├── block.rs        # Block environment tracking
│   ├── host.rs         # Host interface extensions
│   ├── types.rs        # Type definitions and enums
│   └── test_utils/     # Testing utilities
└── examples/
    └── block_env_tracking.rs # Block environment access tracking demo
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