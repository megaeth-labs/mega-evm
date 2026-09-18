# mega-evm

The EVM of MegaETH, built on [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-revm).

This is the 2.x line: the **Satin** engine.
The legacy engine (specs `Equivalence` through `Rex7`) is the 1.x line.

## Base

- **revm**: 40.0.3, from the [MegaETH fork of revm](https://github.com/megaeth-labs/revm) (the revm 43 gas core with EIP-8037 and EIP-2780, plus MegaETH hooks)
- **op-revm**: 20.0.0, from the [MegaETH fork of op-revm](https://github.com/megaeth-labs/op-revm)
- **alloy-evm**: 0.36; **alloy-op-evm**: 0.32

A consumer redirects all twelve revm crates and `op-revm` to the forks with `[patch]` entries; see this repository's root `Cargo.toml`.

## Spec and hardfork

- **Spec (`MegaSpecId`)** defines EVM behavior. Satin has a single spec, `SATIN`, running on Optimism Karst (Ethereum Osaka).
- **Hardfork (`MegaHardfork`)** defines when a spec activates. The single fork `Satin` activates `SATIN`; its timestamps are not scheduled yet.

The legacy spec names do not parse: `"Rex6".parse::<MegaSpecId>()` fails with `ParseMegaSpecError::Legacy`.

## Status

Satin is under construction.
Today it runs transactions through op-revm with EIP-8037 state gas, the EIP-2780 intrinsic cost and a 200,000,000 execution cap; gas above the cap goes to the EIP-8037 reservoir.
SALT pricing, history gas, resource limits, gas detention, the system contracts and keyless deployment arrive in later changes.

## Quick start

```rust,ignore
use alloy_evm::Evm as _;
use mega_evm::{alloy_op_evm::OpTx, MegaContext, MegaEvm, MegaSpecId};
use revm::{context::TxEnv, database::{CacheDB, EmptyDB}, primitives::TxKind};

let context = MegaContext::new(CacheDB::<EmptyDB>::default(), MegaSpecId::SATIN);
let mut evm = MegaEvm::new(context);

let tx = OpTx(op_revm::OpTransaction {
    base: TxEnv { caller, kind: TxKind::Call(target), gas_limit: 1_000_000, ..Default::default() },
    enveloped_tx: Some(Default::default()),
    ..Default::default()
});
let result = evm.transact_raw(tx)?;
```

## Documentation

- [Specification](https://megaeth-labs.github.io/mega-evm/) (describes the legacy engine until the Satin pages land)
- [Architecture](../../ARCH.md)
