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

- **Spec (`MegaSpecId`)** defines EVM behavior.
  Satin has a single spec, `SATIN`, running on Optimism Karst (Ethereum Osaka).
- **Hardfork (`MegaHardfork`)** defines when a spec activates.
  The single fork `Satin` activates `SATIN`; its timestamps are not scheduled yet.

The legacy spec names do not parse: `"Rex6".parse::<MegaSpecId>()` fails with `ParseMegaSpecError::Legacy`.

## Status

Satin is under construction.
Today it runs transactions through its own handler over op-revm's, with EIP-8037 and the EIP-2780 intrinsic cost switched on and a 200,000,000 execution cap; gas above the cap goes to the EIP-8037 reservoir.
It still uses the Osaka gas table, which prices state gas at zero, so no transaction draws state gas yet.

The common execution layer is in place: the frame lifecycle the later mechanisms plug into, the count of data-size bytes and write records per frame, the abort protocol that stops a transaction crossing a limit with a revert, and the inspector admission gate.
No limit is enforced by default; `EvmTxRuntimeLimits` sets a data-size cap and a frame budget to exercise the protocol.
`MegaEvm::execute_transaction` returns the result with the gas split into its regular, state and history ledgers, the usage counted and the limit that stopped the transaction, if any.

SALT pricing, history gas, the resource limits, gas detention, the system contracts and keyless deployment arrive in later changes.

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

A block executor admits an inspected transaction only from an EVM whose inspector is declared read-only:

```rust,ignore
use mega_evm::DeclaredObserver;

let evm = MegaEvm::new(context).with_trusted_inspector(DeclaredObserver(tracer));
assert!(!evm.has_rewriting_inspector());
```

## Documentation

- [Specification](https://megaeth-labs.github.io/mega-evm/) (describes the legacy engine until the Satin pages land)
- [Architecture](../../ARCH.md)
