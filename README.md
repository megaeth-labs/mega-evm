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

The Rust build embeds checked-in system-contract bytecode.

From GitHub:

```bash
cargo install --git https://github.com/megaeth-labs/mega-evm --locked mega-evme
```

From a Git checkout:

```bash
git clone --recursive https://github.com/megaeth-labs/mega-evm
cd mega-evm
cargo install --locked --path bin/mega-evme
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

Cargo checks the system-contract artifacts against the checked-in Rust constants in a repository checkout.
The check is read-only.
Published crates use the same embedded constants.

### System contracts

Run `mise run system-contracts:attest` with Foundry v1.7.1 and initialized submodules.
Attestation checks the contract inventory, reproduces the latest deployed bytecode with the pinned Osaka EVM target, and checks the SequencerRegistry storage layout.
After updating versioned artifacts and their latest aliases, run `mise run system-contracts:generate` to regenerate the Rust constants.
The required `lint` CI job runs attestation, checks that changing a Solidity version in a temporary copy makes attestation fail, and verifies that the Rust constants match the artifacts.
Rust tests pin each versioned artifact's code hash and the version selected by each latest alias.

### Testing

```bash
cargo test --workspace
```

## Documentation

- [mega-evm specification](https://docs.megaeth.com/spec/)
- [Architecture](ARCH.md)

## License

MIT OR Apache-2.0
