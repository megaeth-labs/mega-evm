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
`apply_pre_execution_changes` makes the EIP-2935 and EIP-4788 calls, deploys the six MegaETH system contracts and the EIP-7997 `CREATE2` factory every block, idempotently, and applies a role change the `SequencerRegistry` has due in the block with its `applyPendingChanges()` call.
It hands each pre-block state — the two EIP calls, the seven deploys, the read of the registry's pending changes and its call — to an optional observer before it commits; that sequence is the witness a stateless client needs.

A system call runs as EIP-8037 has it: at most 30,000,000 of its gas limit is regular gas, which is what `GAS` reads inside it, and the rest is its state-gas reservoir, which the state it writes draws first.
revm's default system-call gas limit, 31,566,720, is 30,000,000 and a reservoir of sixteen fresh slots.
The block's pre-block calls run on the block's gas limit and never less than 30,000,000, the legacy engine's budget kept as it was; what it adds above 30,000,000 is reservoir, not regular gas.
The legacy engine widened it for the storage gas a crowded SALT bucket multiplied, which a system call no longer pays: it prices its state at the minimum bucket.
A pre-block call that does not succeed refuses the block with the call's own validation error; a database error the database calls fatal is an internal error instead, since it says nothing about the block.
That is the rule alloy-evm's block executor holds a transaction's database error to.
A system call pays no history gas, is held to no per-transaction limit and is not detained.
A transaction is not a system call: every transaction's gas, the system-address transaction's included, is split by the execution cap.

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
A `keylessDeploy` call a transaction makes is taken before any interceptor sees it, and runs as a frame of its own in which no code runs; the deployment is a native creation, the call's child, and a tracer sees the call, and the creation as its child.
The call's frame is built on the contract as any call's is: the contract's account, which a transaction to it loads with its code in any case, is touched by a call that returns, and the contract's bytecode never runs for it.
A call carrying value is refused before its frame is built, so no value moves.
The call pays a fixed 100,000 gas for decoding the transaction and recovering its signer, and is held to the legacy engine's nine rules and error ABI, in the legacy engine's order.
It pays for the signer's account when the creation's nonce bump is what creates it, and what a `CREATE` opcode charges its frame: its regular gas, the created account, and the two write records of the creation's start.
It then starts the creation as the signer, below it, with `gasLimitOverride` capped to what it has left.
From there the creation is an ordinary frame, priced, limited, journaled and traced as one, and its `ORIGIN` and `GASPRICE` are the transaction's.
Once the creation returns the call answers in the contract's ABI — the deployed address, or the error the creation failed with; a transaction limit the deployment crosses stops the transaction and takes the deployment back whole.
A signer is refused once its nonce is above 1.
A deployment spends the signer's nonce from 0 to 1.
A signer at nonce 1 stays there, as in the legacy engine, however often its deployment fails, so nobody can use up its attempts, and once it deploys, so a resubmission finds the address taken (`ContractAlreadyExists()`).
The creation's bump is taken back with its write record, which stays only when a creation that succeeded moved value out of the signer.
The exception is a signer whose own code spends a nonce in the constructor that survives it — on a default configuration, a delegated signer's `CREATE` or `CREATE2`, successful or not: every bump stays, the creation's included, because a later bump may stand for an account, so the signer ends above 1 and every later deployment of it is refused.
A `keylessDeploy` call a contract makes is not a deployment: it runs the method body, which reverts with `NotIntercepted()`.

The system address sends the protocol's own transactions: a legacy transaction from it to a whitelisted contract is validated — the chain id, the nonce and EIP-3607 — and promoted to a deposit, which pays no fee and rewards none.
It is the address the `SequencerRegistry` holds, and the transaction reads it itself: a transaction of that shape reads the registry's `_currentSystemAddress` from the journal when it is validated, without warming it, and compares it with its caller.
Every other transaction reads nothing, and one of another shape from the system address is an ordinary transaction.
The registry cannot change the address inside a block after its pre-block call, so every transaction of a block reads the address a rotation due in the block left, and an EVM a node builds outside block execution — an RPC call, the replay of a block's transactions for a trace — reads the one in the state it runs on, with nothing to set.
A registry that is absent, holds other code or names a zero address promotes nothing.
The read is in the system-address transaction's own state and witness; the pre-block steps no longer carry it.
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
That charge crossed the cap, and it is not made: the transaction is stopped with a revert carrying `MegaLimitExceeded` of kind compute, billed its compute before that charge, less than one charge short of the limit, and the sender gets back everything the frame had, the part withheld included.
The stop reports that compute as what the transaction used.
A precompile forwarded more than what the limit leaves its frame is decided from its price before it runs: priced within what the limit leaves, it runs as without the read; priced past it and within its forward, it computes nothing and is the same crossing, as is an interceptor's answer that spent more than the limit leaves; priced past its whole forward, it is a failed call its caller survives, as without the read.
So an input priced between what the limit leaves and the forward that would fail a check made after the gas check is the crossing too, where without the read that check fails the call.
A precompile the engine cannot price — a node's own, an address a node replaced, op-revm's size-limited wrappers — is run on what the limit leaves its frame, and one it does not pay is the crossing, whatever its price.
A keyless deployment's call is the transaction's own frame, held to the limit as any frame is: the overhead and the `CREATE` opcode's regular gas it charges are compute whether a rule then refuses the call or its creation runs, and the creation runs under what the limit leaves it.
Every other out-of-gas halts and burns as it would without the read.
While `MegaAccessControl`'s switch is off for a frame, its volatile reads are refused: the frame reverts with `VolatileDataAccessDisabled`, having paid the opcode's static gas and nothing more.
The error's argument is a `uint8`: a refused `SLOTNUM` names access type 12, which the contract's `VolatileDataAccessType` does not declare, so a Solidity handler must decode it as `uint8`, and `decode_volatile_data_access_disabled` decodes it on the Rust side.
The two caps are runtime limits, 20,000,000 each by default; `EvmTxRuntimeLimits::no_limits()` leaves them unlimited, and a transaction whose caps are both unlimited is not detained.
The protocol's own transactions and the system calls are not detained either.

Every transaction-level limit — data size, KV updates, state gas, gas detention's compute limit — stops a transaction the same way.
The frame that crosses it reverts with `MegaLimitExceeded(kind, limit)`, and every frame above returns the same revert without running another instruction, whatever produced its result: revm, an interceptor, or an inspector that rewrites it into a success or a halt.
The transaction settles like any EIP-8037 revert: it keeps nothing it wrote or logged, its sender pays the intrinsic gas and what ran, and gets back the rest of its regular gas and the reservoir, less the body's history.
Nothing is rescued, because nothing was taken: detention withholds gas inside a frame's own tracker and never spends it.
A frame budget reverts its frame alone, and its caller runs on; a real out-of-gas and a precompile given less than its price still halt and burn what their frame was given.
`tests/satin/stops.rs` pins every limit at the transaction's own frame and three calls down, below and above the execution cap, with and without an inspector that rewrites every result.

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
