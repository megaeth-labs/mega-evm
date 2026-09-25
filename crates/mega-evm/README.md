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
`SLOTNUM` pushes the slot number the node supplies in `BlockEnv::slot_num`, zero when it leaves it unset.
Satin takes EIP-7708 from Amsterdam too: every value movement — a transaction's value, a value `CALL`, a creation's endowment, a `SELFDESTRUCT`'s balance moved to another account — emits a `Transfer(from, to, amount)` log from `0xff…fe` into the receipt, in execution order among the contracts' own logs.
A deposit's value is logged and its mint is not, and a destruction to itself burns the balance without a log.
Its byte prices are an input: the `satin-price-override` feature, off by default, lets a measurement build install other ones.
`tests/satin/pricing-table.md` lists every entry next to Osaka's and Amsterdam's, with what a handful of probe transactions spent.

The precompile set is op-revm's Karst set with KZG point evaluation repriced to 100,000.
It is carried as an alloy-evm `PrecompilesMap`, so a node can add or replace an address through `MegaEvmFactory::with_dyn_precompiles_builder`.

The common execution layer is in place: the frame lifecycle the later mechanisms plug into, the count of data-size bytes and write records per frame, the abort protocol that stops a transaction crossing a limit with a revert, and the inspector admission gate.

The data-size limit is in place on top of it.
A transaction is held to a data-size limit, and every frame to a budget: the transaction's own frame gets what its body leaves, and a child 98% of what its parent has left.
The bytes are the ones history gas prices — the body, every write record, every log, every byte of deployed code — plus an Oracle hint's payload and the EIP-7708 transfer logs.
A transfer log counts what a `LOG3` of one word counts, 160 bytes, where the value moves: with the records of the frame start that moves it, before any value moves, or with a `SELFDESTRUCT`'s beneficiary.
A frame start revm refuses on its caller's account — a value the caller cannot fund, a creation whose creator's nonce cannot be bumped — counts nothing; a creation onto an occupied address is counted, and revm refuses it after the count.
A frame that crosses its budget reverts alone; a transaction that crosses its limit is stopped with a revert carrying `MegaLimitExceeded`, and pays only for what ran.
A record is checked before its history is charged, so a record the limit rejects costs nothing and the stop is what the transaction reports.
`EvmTxRuntimeLimits` sets the limits on a bare EVM, where they default to none; a block executor installs its `BlockLimits`, whose default holds each transaction and the block to 12.5 MiB of data size.
The block's data size is a packing budget: the transaction that crosses it is packed and the next one refused.
`MegaEvm::execute_transaction` returns the result with the gas split into its regular, state and history ledgers beside the history bytes, the usage counted and the limit that stopped the transaction, if any.

Block execution is in place too: `MegaBlockExecutor` is alloy-evm's `BlockExecutor` over a `MegaEvm`, with the block rules of the Karst base — a fork's activation block admits only deposit transactions, the data-availability footprint of the block's transactions is held to the block's gas limit and reported as its blob gas, and the L1 block info is read by the first transaction that prices against it, so the block's own L1 info deposit is what the transactions after it are priced with.
Every transaction is held to the block's `BlockLimits`, and the block counts what its transactions spent on each of the three ledgers and the history bytes they appended.
The execution figure a block counts for a transaction is its regular ledger — its gas less state and history, read off revm's `Gas` — at least its EIP-7623 floor.
A block's state gas can be capped: the transaction that reaches the cap is packed, and after it only a transaction that adds no state gas is.
No block cap on execution gas, state gas, data size or write records refuses a deposit, which the block must include; a deposit still counts towards all four.
A builder that executes candidates and chooses among them commits through `commit_transaction_outcome`, which checks the block's counters again; alloy-evm's `commit_transaction` cannot fail and expects each outcome to commit before the next transaction executes, and a debug build asserts it.
`apply_pre_execution_changes` deploys the six MegaETH system contracts and the EIP-7997 `CREATE2` factory every block, idempotently, hands each pre-block state (the two EIP calls and the seven deploys) to an optional observer before it commits — that sequence is the witness a stateless client needs — and leaves one hook point empty: the pre-block system calls.

SALT pricing is in place: every EIP-8037 state gas charge costs the schedule's entry times the capacity of the SALT bucket it lands in, counted in minimum buckets, so a slot written into a region eight times as crowded as the minimum costs eight times as much.
The multiplier applies to the state dimension only; regular gas never scales.
Capacities come from the transaction's `SaltEnv`, read once per bucket per transaction, and a transaction the protocol itself produced prices at the minimum bucket whatever the bucket holds.
Without a SALT environment every bucket is minimal, so the numbers above are what a transaction pays.
`tests/satin/pricing-table.md` shows two probes at three multipliers.

The six system contracts live at their fixed `0x6342…` addresses and are deployed at Satin activation, together with the EIP-7997 factory at `0x4e59…`.
Three of them answer calls through an interceptor instead of running their bytecode, and `KeylessDeploy` turns the `keylessDeploy` calls a transaction makes into the deployments they stand for.
A `CALL` or `STATICCALL` is dispatched on its target address, then on the four selector bytes of its input: `CALLCODE` and `DELEGATECALL` never reach an interceptor, and a selector a contract does not intercept falls through to the deployed bytecode, whose answer is that contract's own — the two control contracts revert with `NotIntercepted()` from their fallback, and `KeylessDeploy` and the Oracle, which have none, revert with empty data on a selector they do not declare.
A method that takes no value answers a value-bearing call with `NonZeroTransfer()`.
`MegaAccessControl` steers gas detention's switch: `disableVolatileDataAccess()` switches volatile-data access off for the calling frame and every frame below it, until that frame switches it back on or returns, `enableVolatileDataAccess()` reverts with `DisabledByParent()` in a frame below the one that switched it off, and `isVolatileDataAccessDisabled()` answers for the caller.
`MegaLimitControl.remainingComputeGas()` answers the compute the calling frame could still spend: the lesser of its own regular gas, with the gas the call forwarded counted back, and what gas detention leaves the transaction once it read volatile data.
It is regular gas only, so a transaction above the execution cap hears at most the cap's share.
Only one property of the legacy engine's answer carries over, that the gas a call forwards is not counted: the legacy answer came from a separate compute ledger and could exceed the caller's gas, while this one is at most the caller's own.
The Oracle forwards a `sendHint` payload to the node's oracle service, unless the calling frame's volatile-data access is off.

The Oracle's storage is read through the node's oracle service: an `SLOAD` in the Oracle's own frame loads the slot from the chain's state, then asks `OracleEnv`, and answers the service's value when it has one and the loaded value otherwise.
A node that replays a block without the service must price and witness it as the node that built it did, and it cannot tell which source the building node read, so nothing may depend on the source: every such read is priced as a cold access, and the slot is loaded either way, so a later write to it finds it warm and a stateless witness carries it.

`KeylessDeploy` deploys a pre-EIP-155 signed creation — Nick's Method — at the address its signer's first creation gets on every chain, with the gas limit the caller chooses.
A `keylessDeploy` call a transaction makes becomes a native creation before any interceptor sees it; a tracer sees the call, and the creation as its child.
The call pays a fixed 100,000 gas for decoding the transaction and recovering its signer, and is held to the legacy engine's nine rules and error ABI, in the legacy engine's order.
It pays for the signer's account when the creation's nonce bump is what creates it, and what a `CREATE` opcode charges its frame: its regular gas, the created account, and the two write records of the creation's start.
It then starts the creation as the signer, below it, with `gasLimitOverride` capped to what it has left.
From there the creation is an ordinary frame, priced, limited, journaled and traced as one, and its `ORIGIN` and `GASPRICE` are the transaction's.
Once the creation returns the call answers in the contract's ABI — the deployed address, or the error the creation failed with; a transaction limit the deployment crosses stops the transaction and takes the deployment back whole.
A signer is refused once its nonce is above 1.
A deployment spends the signer's nonce from 0 to 1 and takes no signer past 1, as the legacy engine did.
A signer at nonce 1 stays there however often its deployment fails, so nobody can use up its attempts, and once it deploys, so a resubmission finds the address taken (`ContractAlreadyExists()`).
A `keylessDeploy` call a contract makes is not a deployment: it runs the method body, which reverts with `NotIntercepted()`.

The system address (`MEGA_SYSTEM_ADDRESS`) sends the protocol's own transactions: a legacy transaction from it to a whitelisted contract is validated — the whitelist, the chain id, the nonce and EIP-3607 — and promoted to a deposit, which pays no fee and rewards none.
The account such a transaction creates for its caller is charged the account-creation state gas exactly once.

History gas is in place: every byte a transaction appends to the chain is priced at MegaETH's cost per history byte, and the byte counts are the ones the data-size limit meters, so a record's history bytes are its own data size.
A transaction body is 310 bytes — 110 for the envelope and one 40-byte record for each of the five writes every transaction makes, its sender's account and the four accounts its fees are credited to — plus one byte per calldata byte, 20 per access-list address, 32 per key and 101 per EIP-7702 authorization.
A log costs 32 bytes for its address, 32 per topic and its data; every account or storage write a transaction keeps costs one 40-byte record; every byte of deployed code costs a byte.
The body is charged at validation, in the EIP-8037 intrinsic state-gas slot so the reservoir pays it first: a gas limit that cannot cover it is rejected before inclusion, and the receipt's state figure does not include it.
Everything else is charged where the write is made and given back, at the same price, by whoever's failure takes it back.
The records a `CALL`, `CALLCODE`, `CREATE` or `CREATE2` starts a frame for are charged to the caller out of what it kept after forwarding gas: a caller that cannot pay halts, and the frame does not start.
A deposit, a transaction the protocol itself sent and a system call pay no history at all.
The last two are held to no per-transaction limit either, for the same reason — the protocol's maintenance must not fail on a resource limit: no data-size, KV or state-gas limit and no frame budget stops them, and what they use is counted and reported all the same.
A user's deposit is held to every limit.

The pairing between the two counts is per record, not per transaction.
An Oracle hint's payload is data size that is never history, because the bytes go to the node's oracle service rather than into a block; an EIP-7708 transfer log is data size that is never history either, because Ethereum prices it at nothing and it is not a byte the transaction chose to write; and the five records a transaction's body carries are an upper bound on the accounts its inclusion writes, so a transfer to the block beneficiary or a fee vault pays a record the body already bound.

A value-transferring `CALL` or `CALLCODE` grants the frame it starts a history allowance of 160 bytes — one three-topic event carrying a word — so a `receive()` hook reached through Solidity's `transfer()` can still emit an event.
The allowance is not gas: it never enters the frame's `Gas`, only a log's charge may draw on it, and what it pays for is on no ledger, because no pool of the transaction's gas paid it.
The history bytes a transaction reports count those bytes all the same, so a block's byte column and its history gas column part by exactly what allowances paid.
The transfer log of the value that granted an allowance draws nothing from it.
The byte column is the history the schedule prices, not the chain's physical growth: a transaction exempt from history gas reports none, a body counts its five fixed write records even when fewer fee accounts are written, and the transfer logs are not in it.

The KV and state-gas limits are in place too.
The KV count is the write-record count the layer keeps — one record per account or storage write the transaction keeps, the sender and the fee accounts being the body's — and the KV limit holds it by the data-size limit's rules: a transaction limit that stops the transaction, and a record budget per frame, 98% of what the parent has left.
Every record is forty bytes of data size, so at the production data-size caps a transaction or a block keeps at most 327,680 records whatever its KV limit.
The transaction's KV count is in its outcome's usage and the block's in its result; a block can be held to a KV limit, a packing budget like its data size.
A transaction's state growth is the EIP-8037 state gas it spends, so the state-gas limit is what holds it: `EvmTxRuntimeLimits::tx_state_gas_limit` holds the state gas a transaction holds, net of what it refilled and of what its failed frames rolled back, at every site state gas is charged, and a crossing anywhere on the call stack stops the transaction with `MegaLimitExceeded(3, limit)`.
It is a limit on gas, so a slot or an account in a crowded SALT bucket reaches it sooner.
Every one of these limits is unlimited unless a node sets it.

Gas detention is in place: a transaction that reads volatile data — the block environment, the block beneficiary's account, the Oracle's storage — may compute at most 20,000,000 more gas after the read than it had spent at it.
Compute is the regular gas spent: the state and history gas that spilled onto regular gas are not compute, and neither is what a halting frame burns.
The one exception is what a frame has left when an opcode's static gas fails, which counts as compute: under that charge's price per halting frame, so the stop only comes earlier.
The Host marks the read where it loads the value, and the most restrictive read binds.
Every frame's spendable gas is held to what the limit leaves the transaction, and the rest is withheld, with the revm fork's withheld part of a frame's regular gas: a regular charge cannot draw it, and every other reader of the frame's gas — `GAS`, the gas a call forwards, the `SSTORE` sentry, what a callee returns — counts it.
So a transaction that reads runs as it would without the read until a regular charge needs the withheld gas.
That charge crossed the cap: the transaction is stopped with a revert carrying `MegaLimitExceeded` of kind compute, billed its compute up to the limit, and the sender gets back everything withheld.
A precompile is run on what the limit leaves its frame, not on all its caller forwarded: one priced past that computes nothing and is the same crossing, as is an interceptor's answer that spent more than it.
A precompile forwarded more than what the limit leaves its frame, and priced past its whole forward, is the crossing too, where without the read it would be a failed call its caller survives.
So is one priced between what the limit leaves its frame and its forward whose input fails a check made after its gas check: without the read, that check fails the call, which its caller survives.
A keyless deployment's call is the transaction's own frame, held to the limit as any frame is: the overhead and the `CREATE` opcode's regular gas it charges are compute whether a rule then refuses the call or its creation runs, and the creation runs under what the limit leaves it.
Every other out-of-gas halts and burns as it would without the read.
While `MegaAccessControl`'s switch is off for a frame, its volatile reads are refused: the frame reverts with `VolatileDataAccessDisabled`, having paid the opcode's static gas and nothing more.
The error's argument is a `uint8`: a refused `SLOTNUM` names access type 12, which the contract's `VolatileDataAccessType` does not declare, so a Solidity handler must decode it as `uint8`, and `decode_volatile_data_access_disabled` decodes it on the Rust side.
The two caps are runtime limits, 20,000,000 each by default; `EvmTxRuntimeLimits::no_limits()` leaves them unlimited, and a transaction whose caps are both unlimited is not detained.
The protocol's own transactions and the system calls are not detained either.

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
