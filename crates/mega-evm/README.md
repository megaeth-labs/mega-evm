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

The gas schedule is Amsterdam's with three changes: the entries EIP-8038 repriced go back to their Osaka values, the EIP-8037 state-gas entries are rebuilt from MegaETH's own cost per state byte, so a new storage slot draws 97,920 state gas and a new account 183,600, and a byte of deployed code is priced at MegaETH's cost per history byte.
The schedule also brings the Amsterdam opcodes (`DUPN`, `SWAPN`, `EXCHANGE`, `SLOTNUM`) and raises the code-size limits to 512 KiB of contract and 1 MiB of initcode.
Its byte prices are an input: the `satin-price-override` feature, off by default, lets a measurement build install other ones.
`tests/satin/pricing-table.md` lists every entry next to Osaka's and Amsterdam's, with what a handful of probe transactions spent.

The precompile set is op-revm's Karst set with KZG point evaluation repriced to 100,000.
It is carried as an alloy-evm `PrecompilesMap`, so a node can add or replace an address through `MegaEvmFactory::with_dyn_precompiles_builder`.

The common execution layer is in place: the frame lifecycle the later mechanisms plug into, the count of data-size bytes and write records per frame, the abort protocol that stops a transaction crossing a limit with a revert, and the inspector admission gate.

The data-size limit is in place on top of it.
A transaction is held to a data-size limit, and every frame to a budget: the transaction's own frame gets what its body leaves, and a child 98% of what its parent has left.
The bytes are the ones history gas prices — the body, every write record, every log, every byte of deployed code — plus an Oracle hint's payload.
A frame that crosses its budget reverts alone; a transaction that crosses its limit is stopped with a revert carrying `MegaLimitExceeded`, and pays only for what ran.
A record is checked before its history is charged, so a record the limit rejects costs nothing and the stop is what the transaction reports.
`EvmTxRuntimeLimits` sets the limits on a bare EVM, where they default to none; a block executor installs its `BlockLimits`, whose default holds each transaction and the block to 12.5 MiB.
The block's data size is a packing budget: the transaction that crosses it is packed and the next one refused, and a deposit is never refused but still counts.
`MegaEvm::execute_transaction` returns the result with the gas split into its regular, state and history ledgers, the usage counted and the limit that stopped the transaction, if any.

Block execution is in place too: `MegaBlockExecutor` is alloy-evm's `BlockExecutor` over a `MegaEvm`, with the block rules of the Karst base — a fork's activation block admits only deposit transactions, the data-availability footprint of the block's transactions is held to the block's gas limit and reported as its blob gas, and the L1 block info is read by the first transaction that prices against it, so the block's own L1 info deposit is what the transactions after it are priced with.
Every transaction is held to the block's `BlockLimits`, and the block counts what its transactions spent on each of the three ledgers.
`apply_pre_execution_changes` deploys the six MegaETH system contracts and the EIP-7997 `CREATE2` factory every block, idempotently, hands each pre-block state (the two EIP calls and the seven deploys) to an optional observer before it commits — that sequence is the witness a stateless client needs — and leaves one hook point empty: the pre-block system calls.

SALT pricing is in place: every EIP-8037 state gas charge costs the schedule's entry times the capacity of the SALT bucket it lands in, counted in minimum buckets, so a slot written into a region eight times as crowded as the minimum costs eight times as much.
The multiplier applies to the state dimension only; regular gas never scales.
Capacities come from the transaction's `SaltEnv`, read once per bucket per transaction, and a transaction the protocol itself produced prices at the minimum bucket whatever the bucket holds.
Without a SALT environment every bucket is minimal, so the numbers above are what a transaction pays.
`tests/satin/pricing-table.md` shows two probes at three multipliers.

The six system contracts live at their fixed `0x6342…` addresses and are deployed at Satin activation, together with the EIP-7997 factory at `0x4e59…`.
Four of them answer calls through an interceptor instead of running their bytecode.
A `CALL` or `STATICCALL` is dispatched on its target address, then on the four selector bytes of its input: `CALLCODE` and `DELEGATECALL` never reach an interceptor, and a selector a contract does not intercept falls through to the deployed bytecode, whose answer is that contract's own — the two control contracts revert with `NotIntercepted()` from their fallback, and `KeylessDeploy` and the Oracle, which have none, revert with empty data on a selector they do not declare.
A method that takes no value answers a value-bearing call with `NonZeroTransfer()`.
`MegaAccessControl` and `MegaLimitControl` answer with what the engine knows so far — nothing has switched volatile-data access off, and `remainingComputeGas()` reports the regular gas the call was forwarded — until detention and compute gas fill them in.
The Oracle forwards a `sendHint` payload to the node's oracle service, and a `keylessDeploy` transaction is charged its fixed 100,000 gas and handed to the keyless rewrite hook that native keyless deployment fills in.

The system address (`MEGA_SYSTEM_ADDRESS`) sends the protocol's own transactions: a legacy transaction from it to a whitelisted contract is validated — the whitelist, the chain id, the nonce and EIP-3607 — and promoted to a deposit, which pays no fee and rewards none.
The account such a transaction creates for its caller is charged the account-creation state gas exactly once.

History gas is in place: every byte a transaction appends to the chain is priced at MegaETH's cost per history byte, and the byte counts are the ones the data-size limit meters, so a record's history bytes are its own data size.
A transaction body is 310 bytes — 110 for the envelope and one 40-byte record for each of the five writes every transaction makes, its sender's account and the four accounts its fees are credited to — plus one byte per calldata byte, 20 per access-list address, 32 per key and 101 per EIP-7702 authorization.
A log costs 32 bytes for its address, 32 per topic and its data; every account or storage write a transaction keeps costs one 40-byte record; every byte of deployed code costs a byte.
The body is charged at validation, in the EIP-8037 intrinsic state-gas slot so the reservoir pays it first: a gas limit that cannot cover it is rejected before inclusion, and the receipt's state figure does not include it.
Everything else is charged where the write is made and given back, at the same price, by whoever's failure takes it back.
The records a `CALL`, `CALLCODE`, `CREATE` or `CREATE2` starts a frame for are charged to the caller out of what it kept after forwarding gas: a caller that cannot pay halts, and the frame does not start.
A deposit, a transaction the protocol itself sent and a system call pay no history at all.

The pairing between the two counts is per record, not per transaction.
An Oracle hint's payload is data size that is never history, because the bytes go to the node's oracle service rather than into a block; and the five records a transaction's body carries are an upper bound on the accounts its inclusion writes, so a transfer to the block beneficiary or a fee vault pays a record the body already bound.

A value-transferring `CALL` or `CALLCODE` grants the frame it starts a history allowance of 160 bytes — one three-topic event carrying a word — so a `receive()` hook reached through Solidity's `transfer()` can still emit an event.
The allowance is not gas: it never enters the frame's `Gas`, only a log's charge may draw on it, and what it pays for is on no ledger, because no pool of the transaction's gas paid it.

The state-growth and KV limits, gas detention and keyless deployment arrive in later changes.

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

A node executes a block through the factory, which installs the block's limits on the EVM:

```rust,ignore
use alloy_evm::block::{BlockExecutor as _, BlockExecutorFactory as _};
use mega_evm::{BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory};

let factory = MegaBlockExecutorFactory::new(receipt_builder, chain_spec, MegaEvmFactory::new());
let ctx = MegaBlockExecutionCtx::new(parent_hash, parent_beacon_block_root, extra_data, BlockLimits::no_limits());

let mut executor = factory.create_executor(evm, ctx);
executor.apply_pre_execution_changes()?;
for tx in transactions {
    executor.execute_transaction(tx)?;
}
let (evm, result) = executor.finish_with_counters()?;
```

Block execution admits an inspected transaction only from an EVM whose inspector is declared read-only:

```rust,ignore
use mega_evm::DeclaredObserver;

let evm = MegaEvm::new(context).with_trusted_inspector(DeclaredObserver(tracer));
assert!(!evm.has_rewriting_inspector());
```

## Documentation

- [Specification](https://megaeth-labs.github.io/mega-evm/) (describes the legacy engine until the Satin pages land)
- [Architecture](../../ARCH.md)
