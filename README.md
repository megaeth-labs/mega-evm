# MegaETH EVM

A specialized Ethereum Virtual Machine (EVM) implementation tailored for MegaETH specifications, built on top of [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-revm).

## Crates

| Crate                                            | Description                                                               |
| ------------------------------------------------ | ------------------------------------------------------------------------- |
| [mega-evm](crates/mega-evm)                      | Core EVM implementation with MegaETH specs (`EQUIVALENCE` through `REX6`) |
| [mega-system-contracts](crates/system-contracts) | Solidity system contracts with Rust bindings                              |
| [mega-evme](bin/mega-evme)                       | CLI tool for EVM execution (`run`, `tx`, `replay`, `cache`)               |
| [mega-t8n](bin/mega-t8n)                         | Standalone state transition (t8n) tool                                    |
| [mega-state-test](crates/mega-state-test)        | Ethereum state test fixtures and runner library (EEST-compatible)         |
| [state-test](crates/state-test)                  | Thin CLI front-end over `mega-state-test`                                 |

## Installation

crates.io publishing is paused, and the last release there is 1.7.2.
Install the CLI tool from a release tag instead, taking the latest tag from [Releases](https://github.com/megaeth-labs/mega-evm/releases).
This builds from the repository, so it needs Foundry (`forge`) on `PATH`.

```bash
cargo install --git https://github.com/megaeth-labs/mega-evm --tag vX.Y.Z --locked mega-evme
```

The `--locked` flag ensures the exact tested dependency versions are used.

Or build from source:

```bash
cargo build --release -p mega-evme
```

### Prebuilt binary

Every release also attaches a prebuilt `mega-evme` (Linux x86_64, built on `ubuntu-24.04`) and a `SHA256SUMS` file to its [GitHub Release](https://github.com/megaeth-labs/mega-evm/releases).
Download both, verify the checksum, and make the binary executable:

```bash
sha256sum --check --ignore-missing SHA256SUMS
chmod +x mega-evme
./mega-evme --version
```

The same binary is published to the MegaETH artifact registry for internal use.

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
cargo test --workspace
```

## Documentation

- [mega-evm specification](https://docs.megaeth.com/spec/)
- [Architecture](ARCH.md)

## License

MIT OR Apache-2.0
