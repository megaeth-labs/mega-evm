# MegaETH EVM

A specialized Ethereum Virtual Machine (EVM) implementation tailored for MegaETH specifications, built on top of [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/bluealloy/op-revm).

## Crates

| Crate                                            | Description                                                               |
| ------------------------------------------------ | ------------------------------------------------------------------------- |
| [mega-evm](crates/mega-evm)                      | Core EVM implementation with MegaETH specs (`EQUIVALENCE` through `REX4`) |
| [mega-system-contracts](crates/system-contracts) | Solidity system contracts with Rust bindings                              |
| [mega-evme](bin/mega-evme)                       | CLI tool for EVM execution (`run`, `tx`, `replay`)                        |
| [mega-t8n](bin/mega-t8n)                         | Standalone state transition (t8n) tool                                    |
| [state-test](crates/state-test)                  | Ethereum state test runner                                                |

## Installation

Install the CLI tool from crates.io:

```bash
cargo install mega-evme --locked
```

The `--locked` flag ensures the exact tested dependency versions are used.

Or build from source:

```bash
cargo build --release -p mega-evme
```

## Development

This repository uses git submodules.
Clone with submodules:

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

## Documentation

- [mega-evm specification](https://megaeth-labs.github.io/mega-evm/)
- [Architecture](ARCH.md)

## License

MIT OR Apache-2.0
