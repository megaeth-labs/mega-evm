# AGENTS.md

This file provides guidance to AI agents (e.g., claude code, codex, cursor, etc.) when working with code in this repository.

## Project Overview

MegaEVM (mega-evm) — a specialized EVM implementation for MegaETH, built on **revm** and **op-revm** by customizing the hooks revm exposes through its traits.

This branch (`satin`) builds **Satin**, the new engine: a single spec on the MegaETH fork of revm 40.0.3 and the MegaETH fork of op-revm 20.0.0.
Today's `main` is the legacy engine; Satin is the only active spec.
The legacy engine (specs `Equivalence` through `Rex7`, the 1.x crate line on crates.io revm 27) is frozen and is not edited here.

Satin is being built mechanism by mechanism from an empty skeleton.
The module table below says what exists and which mechanisms fill the rest.

## Build & Development Commands

```bash
# Build
cargo build

# Test
cargo test                                # all tests
cargo test -p mega-evm                    # core crate only
cargo test -p mega-evm -- test_name       # single test

# The gas schedule's byte prices are an input; this feature lets a measurement build change them
cargo test -p mega-evm --features satin-price-override,test-utils
# Run the suite with MEGA_SATIN_CPSB and MEGA_SATIN_CPHB unset: with either of them set the tests
# that assert the spec's own byte prices skip, and the suite checks less than it looks like it does

# Regenerate the checked-in pricing table after an intentional schedule or engine change
UPDATE_SATIN_PRICING_TABLE=1 cargo test -p mega-evm --test satin

# The execution-spec gate (see Test Gates): Ethereum's state-test fixtures through MegaEvm, per fork
cargo run --release -p state-test -- --fork Osaka <main-fixtures>/state_tests
cargo run --release -p state-test -- --fork Amsterdam <devnet-fixtures>/state_tests
cargo run --release -p state-test -- --mode satin --fork Osaka <main-fixtures>/state_tests  # the report
# Regenerate the rendered deviation registry after changing crates/mega-state-test/src/deviations.rs
UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations

# Check compiler errors (preferred over clippy for quick checks)
cargo check
cargo check -p mega-evm

# Lint (CI runs all of these)
cargo fmt --all --check
cargo clippy --workspace --lib --examples --tests --benches --all-features --locked
cargo sort --check --workspace --grouped --order package,workspace,lints,profile,bin,benches,dependencies,dev-dependencies,features

# Benchmarks: `transact` (MegaEvm next to op-revm), `corpus` (JSON scenarios, a bench input), `factory` (EVM construction), `block` (a block through MegaBlockExecutor)
cargo bench -p mega-evm --bench <target>                                  # wall-clock + HTML report
cargo codspeed build -p mega-evm --bench <target> && cargo codspeed run   # instruction counts (Linux only)

# no_std check (run against riscv target)
cargo check -p mega-evm --target riscv64imac-unknown-none-elf --no-default-features

# System contracts (requires Foundry)
cd crates/system-contracts && forge build
```

Git submodules are required — clone with `--recursive` or run `git submodule update --init --recursive`.

## Workspace Structure

| Crate                   | Path                      | Member | Purpose                                                                     |
| ----------------------- | ------------------------- | ------ | --------------------------------------------------------------------------- |
| `mega-evm`              | `crates/mega-evm`         | yes    | The Satin engine                                                            |
| `mega-system-contracts` | `crates/system-contracts` | yes    | Solidity system contracts with Rust bindings (Foundry-based)                |
| `mega-state-test`       | `crates/mega-state-test`  | yes    | Execution-spec state-test runner on Satin: the equivalence gate, the report |
| `state-test`            | `crates/state-test`       | yes    | The runner's CLI                                                            |
| `mega-evme`             | `bin/mega-evme`           | no     | EVM execution CLI; rejoins when ported to Satin                             |
| `mega-t8n`              | `bin/mega-t8n`            | no     | State transition (t8n) tool; rejoins when ported to Satin                   |

The two tool binaries `mega-evme` and `mega-t8n` still target the legacy engine.
They are outside `[workspace] members`, so no workspace command builds them; do not edit their sources until they are ported to Satin.

### Dependencies on the forks

The root `Cargo.toml` pins `revm = "=40.0.3"` and redirects all twelve revm crates to the MegaETH fork with `[patch.crates-io]`.
`op-revm` is declared from the OP monorepo revision the node locks and redirected to the MegaETH fork of op-revm with `[patch."https://github.com/ethereum-optimism/optimism"]`; the OP alloy crates (`alloy-op-evm`, `op-alloy-*`) come from the same monorepo revision.

- Patch the twelve revm crates together, or the build resolves a second copy of revm.
- `cargo tree -i revm` must show exactly one revm, from the fork.
- The fork pins move by editing the patch blocks; commit the regenerated `Cargo.lock` with them.

## Architecture

### Spec System (`MegaSpecId`)

`MegaSpecId` has a single rung, `SATIN`, running on `OpSpecId::KARST` (Ethereum `OSAKA`).
`MegaHardfork` has the single fork `Satin`, which activates `SATIN`.

- `SATIN` is the unstable spec under active development: it has no activation timestamp yet (`block/chain.rs` lists the two chains with the Satin timestamp unset), and its behavior may change until it is sealed.
- Display and `FromStr` use `"Satin"`; serde uses the variant name `"SATIN"`.
- The legacy spec names (`Equivalence` … `Rex7`) parse to `ParseMegaSpecError::Legacy`; they are never mapped to Satin.
- There are no spec gates: a single-spec engine has nothing to gate.
  The spec that follows Satin introduces the first `is_enabled`-style gate.

### Core Source Layout (`crates/mega-evm/src/`)

| Module         | Holds now                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | Filled by                                                                                                                                                                                                                         |
| -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `evm/`         | `MegaSpecId`; `MegaContext`; `MegaEvm` over revm's `Evm` with its own frame lifecycle, the interceptor scheme guard, the keyless rewrite point and the keyless settlement, and the system-transaction promotion in `MegaHandler` over op-revm's handler (`execution.rs`); the staging Host, the SALT pricing hook, the block-hash record and the non-warming journal reads (`host.rs`, `state.rs`); the `SSTORE`/`LOG`/`SELFDESTRUCT` commit wrappers, the `CALL`/`CALLCODE`/`CREATE`/`CREATE2` write-record charges and the Amsterdam opcode activation (`instructions.rs`); the Satin gas schedule (`schedule.rs`) and the byte prices it is built from (`prices.rs`); what a history byte costs and how many each site appends (`history.rs`); the Satin precompile set with the repriced KZG (`precompiles.rs`); synthetic frame results (`frame.rs`); `TrustedObserver`, `DeclaredObserver` and the creation-revival refusal (`inspector.rs`); `MegaTransactionOutcome`, `MegaGasUsage` (`result.rs`); `MegaEvmFactory` and its dynamic precompile builder (alloy-evm) | revert-class policy aborts, the system-call reservoir split; EIP-7708 and SLOTNUM; inspector support                                                                                                                              |
| `block/`       | `MegaHardfork`, `MegaHardforks`, `MegaHardforkConfig` and the load-time schedule validation (`hardfork.rs`); the chain table and the canonical schedules (`chain.rs`); `MegaBlockExecutor` with the Karst block rules, `MegaBlockExecutorFactory` and the admission gate; `BlockLimits` with the production data-size caps as its default, `BlockLimiter` with the execution-gas, state-gas, data-size and KV block limits (`limit.rs`); `BlockGasCounters` (the three ledgers and the history bytes), `MegaBlockTxResult` (`result.rs`); the pre-block call helpers (`eips.rs`); the system-contract deploy and the pre-block state observer in `apply_pre_execution_changes`; `MegaTransactionExt` (`helpers.rs`); the block-hash record (`evm/state.rs`)                                                                                                                                                                                                                                                                                                                 | the pre-block system calls (the remaining hook point of `apply_pre_execution_changes`)                                                                                                                                            |
| `external/`    | `ExternalEnvFactory`, `ExternalEnvs`, `SaltEnv`, `OracleEnv`, `EmptyExternalEnv`, the bucket hasher, `TestExternalEnvs`; `BucketMultipliers`, the per-transaction cache the SALT pricing hook reads through (`gas.rs`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | the oracle                                                                                                                                                                                                                        |
| `limit/`       | the byte table every mechanism sizes a thing by; `LimitKind`, `LimitCheck`, `MegaLimitExceeded`, `LimitUsage`, `StagedRecord`; `AdditionalLimit`: the per-frame lanes of data-size bytes, write records, history allowances and the log and code bytes of the history byte count (`frame_limit.rs`), staging, the latch, the transaction body and deployed code on the lanes; what the state-gas limit counts, a frame's state gas held outside it (`state_gas.rs`); `EvmTxRuntimeLimits` (the data-size and KV limits: a transaction cap, the 98% share a child takes of what its parent has left, and an optional frame cap, each in its own unit; the per-transaction state-gas limit)                                                                                                                                                                                                                                                                                                                                                                                   | detention                                                                                                                                                                                                                         |
| `access/`      | nothing                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | volatile-data access tracking (detention)                                                                                                                                                                                         |
| `system/`      | the six contracts' addresses, code and ABIs, one module each; the interceptor dispatch and the value policy (`intercept.rs`); the `MegaAccessControl` and `MegaLimitControl` interceptors (`control.rs`, `limit_control.rs`); the Oracle's hint path (`oracle.rs`); keyless deployment as a native creation: the dispatch, the overhead, the nine rules, the charges of the creation's start and the rewrite (`keyless/dispatch.rs`), the settlement and the ABI answer (`keyless/settle.rs`), the Nick's Method transaction format and the error ABI (`keyless/tx.rs`, `keyless/error.rs`); the system-address transaction and its whitelist (`tx.rs`); the deploy helper, the EIP-7997 factory and `SequencerRegistryConfig` (`deploy.rs`, `sequencer_registry.rs`)                                                                                                                                                                                                                                                                                                       | the oracle and control contracts (the detention and compute-ledger semantics the two control contracts answer with, and the Host's oracle reads)                                                                                  |
| `constants.rs` | the provisional numbers (CPSB, slot and account state gas, CPHB, execution cap, contract and initcode size, data-size limits)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | the Satin gas schedule reads the state-gas and history numbers and the spec configuration reads the caps and the size limits; the rest are read by the limit mechanisms. The numbers are provisional until the economics sign-off |
| `types.rs`     | transaction, halt reason, error and envelope aliases, kept because Satin adds nothing to them (its halt set is op-revm's)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | —                                                                                                                                                                                                                                 |
| `test_utils/`  | `MemoryDatabase`, `ErrorInjectingDatabase`, `BytecodeBuilder`, `GasInspector`, `transact`, the JSON `Scenario` format (one EVM per scenario, as a block runs); the execution-spec gate's neutral configuration (`neutral_cfg`, `neutralize_evm`, over `MegaContext::with_neutral_cfg`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | `BytecodeBuilder` is extended with EIP-7708 and SLOTNUM                                                                                                                                                                           |

### How Satin executes today

- `MegaContext` wraps op-revm's context shape (`MegaTransaction` = alloy-op-evm's `OpTx`, `L1BlockInfo`) and adds the spec and the external environments.
  It keeps the configuration twice — the `MegaSpecId` view callers see and the `OpSpecId` view op-revm executes on — and writes both together.
- The spec fixes part of the configuration, whatever the caller passes (`MegaContext::with_cfg`): the Satin gas schedule, EIP-8037 on, the EIP-2780 intrinsic cost on, `tx_gas_limit_cap` = 200,000,000, EIP-7708 off, the system-call reservoir margin off, the contract size limit at 512 KiB and the initcode size limit at 1 MiB.
  Gas above the 200M execution cap goes to the EIP-8037 reservoir.
- The Satin gas schedule (`evm/schedule.rs`) is Amsterdam's with two changes: the seventeen entries EIP-8038 repriced go back to their Osaka values, and the EIP-8037 state-gas entries are rebuilt from `MegaETH`'s own cost per state byte.
  It is built once per process and installed on every configuration; the byte prices are an input (`evm/prices.rs`), so a measurement build can run other ones through the std-only `satin-price-override` feature or the `MEGA_SATIN_CPSB` / `MEGA_SATIN_CPHB` environment variables.
  `crates/mega-evm/tests/satin/pricing-table.md` lists every entry next to Osaka's and Amsterdam's, what nine probe transactions spent, and what two of those probes spend with the bucket they write into crowded; a test renders it and compares it to the file.
- The instruction table adds the Amsterdam opcodes the base spec gates off (`DUPN`, `SWAPN`, `EXCHANGE`, `SLOTNUM`), before the metering wrappers go in; `CLZ` is gated on Osaka and needs no activation.
- The precompile set is op-revm's Karst set with KZG point evaluation repriced to 100,000, carried as an alloy-evm `PrecompilesMap` so a node can add or replace an address (`MegaEvmFactory::with_dyn_precompiles_builder`).
- `MegaEvm` runs transactions through `MegaHandler`, which wraps op-revm's handler, and implements revm's frame lifecycle itself; every `Host` and context method delegates to op-revm's context except the three that stage what a state-writing opcode did, the block-hash read the Host records and the state-gas price the Host scales.
  Satin's one divergence from op-revm on an ordinary transaction is the history gas it charges: `tests/satin/equivalence.rs` holds every baseline case to op-revm's total plus the history ledger, on op-revm's own state gas, logs and state, and pins what that ledger is made of; a later change that alters behavior on purpose updates that baseline.
- SALT pricing scales every EIP-8037 state gas charge by the capacity of the SALT bucket it lands in: `state gas = schedule entry x m`, where `m` is that capacity in minimum buckets (`evm/host.rs`).
  A charge on a slot reads the slot's bucket, a charge on an account leaf reads the account's, and `m` touches the state dimension only — regular gas never scales.
  A slot is charged by the `SSTORE` that fills a fresh one; an account leaf by a `CREATE`, `CREATE2` or value `CALL` that adds it, by the transaction's own recipient or the address a creation transaction deploys at, by an applied EIP-7702 authority, and by the beneficiary a `SELFDESTRUCT` moves a balance to; the bytes a creation deposits are charged in the deployed address's bucket.
  The capacities come from the transaction's `SaltEnv` through `BucketMultipliers` (`external/gas.rs`), which reads a bucket once per transaction; the cache is cleared with the rest of the per-transaction state.
  A system-originated transaction prices at the minimum bucket and reads no capacity at all, so a state change the protocol mandates cannot be priced out by the growth of a region it does not control; that is a transaction whose caller is the EIP-4788 system address, the sequencer's own system transaction — before or after the engine promotes it to a deposit — and every transaction run through a system-call entry point.
  A failed capacity lookup reports nothing and records its cause the way a failed `sload` does; the transaction fails with that cause rather than settling at a price nobody chose.
  At the minimum bucket every number is the schedule's own, which is why the op-revm baseline and the pricing table did not move when SALT pricing landed.
- History gas prices the bytes a transaction appends to the chain, at `MegaETH`'s cost per history byte and never scaled by SALT — history bytes live in no bucket (`evm/history.rs`).
  The byte table is the `limit` module's, the same counts the data-size limit meters, so a record's history bytes are its own data size and the two cannot drift apart: a transaction body of 310 bytes (110 for the envelope and one 40-byte record for each of the five writes every transaction makes — the sender's account and the four accounts its fees are credited to), one byte per calldata byte, 20 per access-list address and 32 per key, 101 per EIP-7702 authorization, a log's 32 for its address and 32 per topic plus its data, one 40-byte record per kept account or storage write, and one byte per byte of deployed code.
  The body, the applied authorities' records and the record the transaction's own frame makes are charged before execution: the body in the EIP-8037 intrinsic state-gas slot, where the reservoir pays it first and it is not held to the execution cap, and the two record charges in pre-execution, where the records are made.
  A gas limit that cannot cover the body is rejected at validation rather than included as an out-of-gas, and the settled result takes the body back out of the state gas it reports.
  Every other charge is made where the record is: the opcode wrappers charge what the Host staged for them, and `CALL`, `CALLCODE`, `CREATE` and `CREATE2` charge their caller for the records the frame they start makes, out of what the caller kept rather than out of what it forwards.
  What a frame does not keep comes back, at the same price, to whoever paid it.
  The pairing is per record: per transaction the two totals part at two sites, both by decision — an Oracle hint's payload is data size that is never history, and the body's five records are an upper bound, so a transfer to the block beneficiary or a fee vault pays a record the body already bound.
  A deposit, a transaction the protocol itself produced and a system call pay no history at all (`MegaContext::prices_history`), and an exempt transaction runs a schedule that prices a deposited byte at zero, so the one history charge revm makes itself is off it too.
  The EIP-7623 calldata floor is still computed and validated and binds nothing: history charges the same bytes at a higher rate.
- A value-transferring `CALL` or `CALLCODE` below the transaction's own frame grants the frame it starts a history allowance of 160 bytes — one three-topic event carrying a word — at the cost per history byte, so a `receive()` hook reached through Solidity's `transfer()` can still emit an event.
  It is not gas: it never enters the frame's `Gas`, only a log's charge may draw on it, and what it pays for is on no ledger, because no pool of the transaction's gas paid it.
  The history bytes a transaction reports count those bytes all the same, so the byte column and the history gas column part by exactly what allowances paid.
- The data-size limit holds a transaction to `EvmTxRuntimeLimits::tx_data_size_limit` and every frame to a budget: the transaction's own frame gets what its body leaves, and a child 98% of what its parent has left, under an optional frame cap.
  The bytes are the byte table's, counted where they are made: the body before any frame, the authorities' records before the first frame, a frame start's records when it starts, a storage write, a log or a `SELFDESTRUCT`'s beneficiary once the opcode completed, deployed code revm would deposit before the creation is committed (code starting with `0xEF` or over the code-size limit, and a creation that cannot pay for its deposit, fail the creation alone and are not counted), an Oracle hint before it is forwarded.
  A frame that crosses its budget reverts alone; a transaction that crosses its limit is stopped through the latch (the contracts below).
  A bare `MegaContext` sets no limit; a block executor installs its `BlockLimits`, whose default holds each transaction to `TX_DATA_LIMIT` and the block to `BLOCK_DATA_LIMIT`, 12.5 MiB each.
  The block's data size is a packing budget like the block's execution gas (see the block executor below).
  `tests/satin/data_size.rs` derives where the stop gives way to out-of-gas: a top-level fresh `SSTORE` is counted at 162,306 gas, and keeping its record costs its 3,520 history on top.
- A limit stop names one of four runtime dimensions, the `kind` of `MegaLimitExceeded` (`LimitKind`), with the discriminants the legacy engine encoded: data size (0) and KV updates (1), counted on the lanes with a transaction limit and frame budgets; compute gas (2), which is the execution cap revm enforces on the regular pool until detention adds its stop; and state growth (3), held as EIP-8037 state gas by a per-transaction limit.
- The KV limit holds the write records a transaction keeps — its KV count, one record per kept account or storage write, deduplicated per frame and taken back with a write-back or a failed frame — by the data-size limit's rules in its own unit: `EvmTxRuntimeLimits::tx_kv_update_limit` stops the transaction, and every frame gets a record budget, what the transaction has left for its own frame and 98% of what its parent has left for a child, under `frame_kv_update_limit`.
  The sender's account and the fee accounts are the body's, not records, so a transaction that writes nothing counts none.
  A record is forty bytes of data size, counted and taken back with them, so `KV × 40 ≤ data size` holds after every instruction (`tests/satin/write_records.rs`), and data size is checked before the records wherever both cross.
  At the production data-size caps a transaction or a block keeps at most 327,680 records, so the KV limits bind only below that; both are unlimited unless a node sets them, and `BlockLimits::block_kv_update_limit` is the block's, a packing budget like its data size.
- The state-gas limit holds a transaction's state growth: EIP-8037 charges state gas for exactly the state a transaction adds, so nothing counts new accounts and slots beside it.
  `EvmTxRuntimeLimits::tx_state_gas_limit` holds what the transaction holds, net of refills and of failed frames, so a write taken back gives its room back: the state gas charged before its first frame plus what every frame on the call stack holds, read off revm's own per-frame counters (`limit/state_gas.rs`).
  It is per transaction, with no frame budget: a child's crossing stops the whole transaction, with `MegaLimitExceeded(3, limit)` where the limit is in gas.
  It is held where state gas is charged, after the charge — a charge the frame cannot pay is an out-of-gas whatever the limit: the authorities before the first frame (taken back on a crossing), EIP-2780's charge for the first frame's recipient or created account (the frame is answered with the stop), the `SSTORE` and `SELFDESTRUCT` wrappers, the new account a `CALL`, `CREATE` or `CREATE2` is charged for upfront, once revm has decided the frame it starts (`MegaEvm::frame_init`: a frame revm refuses — a value call its caller cannot fund, one past the call-stack limit — gives the charge back and is never held for it; a built frame returns the stop before its first instruction and an answered one is rewritten to it, which gives back the gas they forwarded) — and, for deployed code, just before `return_create` makes its charge, priced through the same hook, as the data-size limit counts the same bytes there, so a crossing leaves no code behind; both hold the deposit only once `return_create` is sure to make that charge, so a creation that cannot pay the regular costs it charges first, or the state gas itself, runs out of gas whatever the limit.
  The state gas is checked before the data size and the records wherever both cross at one site; at a frame start the records are held before revm builds the frame, and a frame they stop adds no account, so its upfront state gas is given back rather than held.
  It is a limit on gas, so it counts state at the price SALT sets for it: a slot or an account in a bucket `m` times the minimum costs `m` times the entry, and a crowded bucket reaches the limit sooner.
  It is unlimited unless a node sets it; the per-block one is `BlockLimits::block_state_gas_limit`.
- The protocol's own work is held to no per-transaction limit: a system-originated transaction (`system::is_system_originated`) and a system call — the EIP-2935 and EIP-4788 pre-block calls among them — run under the sticky `LimitCheck::Exempt` (`AdditionalLimit::exempt`), stamped before the body is counted, which every stop the layer hands out consults (`AdditionalLimit::crossed`): no data-size, KV or state-gas limit and no frame budget stops them, and they are never latched.
  It is the set history gas exempts, for the same reason: the protocol's maintenance must not fail on a resource limit.
  What they use is counted all the same and reported, in the outcome's usage and in the block's counters, as a deposit's is; a block cap never refuses them, a system transaction being promoted to a deposit.
  A user's deposit is not system-originated and is held to every per-transaction limit.
- `MegaBlockExecutor` runs a block: alloy-evm's `BlockExecutor` over a `MegaEvm`, mirroring what alloy-op-evm's `OpBlockExecutor` does for an OP chain.
  It applies the three block rules of the Karst base — an activation block admits only deposits, the data-availability footprint is a block limit reported as the block's blob gas, and the L1 block info is read by the first transaction that prices against it — holds every transaction to `BlockLimits` and fills `BlockGasCounters`.
  A block limit known only after execution packs the transaction that reaches it: the execution-gas and data-size limits then refuse every later transaction before it runs, and the state-gas limit refuses a later transaction after it runs, only if it adds state gas — one that adds none still fits.
  The KV limit refuses like the data-size one.
  None of the four refuses a deposit: they are packing budgets for the transactions the builder chooses, and a deposit is not chosen; it still counts towards all four, so the transactions after it find the room it used.
  alloy-evm's `commit_transaction` cannot fail, and its contract is that an outcome commits before the next transaction executes; a builder that executes candidates and chooses among them commits through `commit_transaction_outcome`, which checks the counters again and refuses what no longer fits, and a debug build asserts the same check in `commit_transaction`.
  Neither commit checks the state a candidate executed against: a candidate executed before another commit changed that state is the builder's to execute again.
  It refuses a rewriting inspector at every entry point, and `apply_pre_execution_changes` deploys the six MegaETH system contracts and the EIP-7997 factory every block (idempotent), handing each pre-block state to an optional observer before it is committed — that sequence is the witness a stateless client needs; the pre-block system calls that apply a due `SequencerRegistry` change remain an empty hook after that deploy.
- The six MegaETH system contracts and the EIP-7997 factory are deployed at Satin activation, idempotently, at the start of every block.
  Three of the six system contracts answer calls through their interceptors (`system/`), and `KeylessDeploy`'s `keylessDeploy` is taken by the keyless rewrite before interception.
  A `CALL` or `STATICCALL` is dispatched on its target address, then on the four selector bytes of its input; an unknown selector is not intercepted and runs the contract's own bytecode.
  `MegaAccessControl` and `MegaLimitControl` answer with what the common execution layer knows until detention and the control contracts' compute-ledger semantics fill them in, and the Oracle forwards a `sendHint` payload to the node's oracle service.
- A `keylessDeploy(bytes,uint256)` call a transaction makes is a native keyless deployment (`system/keyless/`): the pre-EIP-155 creation it carries is deployed as a `CREATE` of its signer, at the signer's Nick's-Method address.
  The rewrite (`keyless::rewrite`) runs in `frame_init` before interception and in `inspect_frame_init` before the inspector is told the frame starts, so the creation is an ordinary creation from there on and an inspector's root is a `create`.
  It charges the fixed overhead, holds the call to the legacy engine's nine rules and error ABI, charges what the `CREATE` opcode charges its frame — the signer's account when the creation's nonce bump creates it and the created account, both SALT-priced state gas, and the creation's two write records — and starts the creation at depth 1 with `CreateScheme::Custom` and `gasLimitOverride` capped to what the call has left.
  The call is a frame no code runs in: its lane (the one any depth-0 call pushes, running as the signer), its journal checkpoint and its gas (`KeylessCall`, on the context) stay until `MegaHandler::last_frame_result` settles the creation into it (`keyless::settle`) and answers in the ABI, before the call's lane is popped.
  A stop the call is left with — the latch, or its own budget crossed by the creator's nonce record — reverts it with the stop and takes the deployment back, the signer's nonce included; otherwise the call succeeds with the deployed address or the error the creation failed with, and the signer's nonce stays spent.
  `ORIGIN` and `GASPRICE` in the init code are the transaction's, and `gasUsed` is what the creation spent from either pool, the same below and above the execution cap.
  Rule 4 (signer nonce at most 1) counts real nonces, so a signer gets two failing attempts and not a third.
  `ParentBudgetExceeded`, `InvalidTransaction`, `InsufficientComputeGas` and `InternalError` stay in the ABI with no producer: the sandbox's preflight and validation are gone, the overhead is regular gas a call that cannot pay runs out of, and a failed read or SALT lookup fails the transaction with its cause.
- The system address may send transactions the protocol pays nothing for (`system/tx.rs`): a legacy transaction from it to a whitelisted contract is validated — the whitelist, the chain id, the nonce, EIP-3607 — and promoted to a deposit, and the account a deposit-like transaction creates for its caller is charged the account-creation state gas once.
- The legacy engine's gas leakage pitfalls and limit-check protocol describe mechanisms that do not exist here; the contracts below replace them.
  Its storage-gas stipend does have a counterpart, the history allowance above, and it is a different thing: it never enters a frame's gas limit, so there is nothing to burn on return.

### Contracts of the common execution layer

Every later mechanism plugs into these; a change to one comes back to this layer.

- **The Host only observes.**
  `sstore`, `log` and `selfdestruct` stage the facts revm hands them (`StagedRecord`) and record nothing.
  Recording in the Host would count a write the opcode's own failure takes back (an out-of-gas after the Host call), and a limit crossed there would stop the transaction for a write that never happened.
- **Commit after the opcode.**
  The wrappers of `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT` discard a stale record on entry, run revm's instruction, then commit the staged record if the opcode completed and discard it if it failed, and charge the frame the history the record costs — which is the record's own data size, so the charge and the count are the same number.
  The wrappers of `CALL`, `CALLCODE`, `CREATE` and `CREATE2` charge their own frame for the records the frame they start makes, after revm computed the gas they forward, so the frame's budget carries none of them.
  Two things follow from charging after revm's instruction has built the frame's input: the frame carries a copy of the caller's reservoir, taken before the charge, so the wrapper writes the post-charge reservoir back into it — a frame that inherited the earlier one would hand the charge back on return, and a frame answered without running would hand it back twice.
  And the frame is already pending as the interpreter's action, which halts on an instruction's error only when no action is pending, so a caller that cannot pay drops the pending frame before it fails; otherwise the frame starts and its records are never paid for.
  Every other opcode runs revm's instruction unwrapped, and the static gas table is revm's.
- **Write records.**
  One 40-byte record per account or storage write: a slot's first change in the transaction (taken back on write-back to the original value), a value transfer's sender and recipient, a creation's creator nonce and created account, a `SELFDESTRUCT` moving value to another account, an applied EIP-7702 authority, the transaction's value recipient or created account.
  The sender's own account is part of the transaction body: it is never a record, and a frame running as the sender (through an EIP-7702 delegation) counts it as recorded.
  Records are deduplicated per frame (a frame's account is recorded once) and live on the frame's lane: a success merges the lane into the caller's, a failure discards it; a creator's nonce record survives the creation's failure once the nonce was bumped.
  That record lands on the creator after the creation's own check, so the creator is held to its budget with it before it runs on, and reverts alone if it crossed (`AdditionalLimit::on_frame_return`).
  The KV count a node reports is the write-record count, and the KV limit holds it.
- **Lanes stay aligned with frames.**
  `frame_init` pushes one lane per frame result revm will return, an empty one for a frame answered without running (the latch, the depth guard, an interceptor, an inspector); `frame_return_result` pops it, and `last_frame_result` pops the outermost one when it never ran.
  A `keylessDeploy` call rewritten into its creation pushes a lane of its own, under the creation's: the creation is the outermost frame revm runs, so the history its lane gives back is kept for the call (`keyless::give_back_history`), and the keyless settlement pops the creation's lane when the creation never ran.
  The state-gas limit's entry of what is held outside each frame is pushed and popped with the lane.
- **The latch.**
  A transaction-level limit stops the transaction with a revert, never a halt: the frame that crosses it reverts with `MegaLimitExceeded(uint8 kind, uint64 limit)`, the transaction is latched (`AdditionalLimit::latch`), no caller resumes (`before_frame_run`), no frame starts, every result returned above is rewritten to the stop, and the outermost frame settles like an EIP-8037 revert, its unspent regular gas and the reservoir back to the sender.
  A frame budget reverts its frame alone, without a latch.
  A limit is enforced before the writes it guards: a frame whose start would cross it is answered with the stop before revm builds it (a creation still bumps its creator's nonce, so a stopped creation transaction cannot be replayed), and EIP-7702 authorities whose state gas or records would cross it are taken back before the first frame.
  The one exception is the state gas an opcode charges upfront for the account the frame it starts adds: revm gives it back for a frame it refuses, so it is held once revm has decided the frame, and what the frame's start wrote goes with the frames the stop reverts.
  A record is checked before its history is charged, so a record the limit rejects is not charged and the stop is what its frame reports; at a frame start the caller pays at its opcode, before the records are counted, so there an out-of-gas comes first.
  A body over the limit latches the transaction before it runs: pre-execution applies no authorization and charges no record made outside a frame, the first frame's input is built with nothing charged for its start, and the first frame is answered with the stop.
  A real out-of-gas, a precompile out-of-gas and an invalid opcode still halt and burn; an out-of-gas before the first frame takes back what pre-execution counted and keeps the body — only the account a deposit-like transaction creates for its caller can still run a latched transaction out of gas there, and then the halt clears the latch.
  The layer's state belongs to one transaction: every entry point of `MegaEvm` resets it before it runs the handler.
  The outcome's `limit_exceeded`, not the output, tells a stop from a contract reverting with the same bytes.
- **Synthetic frame results carry the caller's pools.**
  A result built without running a frame is a `synthetic_frame_result`: gas untouched with the inherited reservoir (`untouched_call_gas`, `with_pools_of`), never `Gas::new(limit)`, and the calling opcode's upfront state-gas flags, so it settles exactly like revm's own (`settle_frame_result`).
- **The admission gate.**
  A rewriting inspector is a tool feature; `with_trusted_inspector` requires a `TrustedObserver` declaration, `has_rewriting_inspector` is what block execution refuses, and `DeclaredObserver` carries the declaration for a foreign tracer and proves it in debug builds.
  alloy-evm's `BlockExecutorFactory` asks for an executor for every `I: Inspector`, so the refusal cannot be a bound on the type: block execution checks it at every entry point — the pre-block changes, a transaction, a commit and the end of the block — because an inspector can be enabled after the block was set up and a caller can run transactions without setting it up.
  `create_executor_with_trusted_inspector` is the compile-time proof; `create_executor` is the checked route.
  The one rewrite refused is a failed creation turned into a success (`FORBIDDEN_CREATE_REVIVAL`, an `EVMError::Custom`).
  The test utilities' inspectors are not declared.
- **The interceptor dispatch.**
  Its order is the scheme guard, the address, the selector, then the method's value policy, each step cheaper than the next: `CALLCODE` and `DELEGATECALL` never reach an interceptor, because they run the callee's code in the caller's context; the address test is one comparison against the shared `0x6342…` prefix and runs on every call a transaction makes; the selector is peeked without materialising the calldata behind it, and is admitted on its four bytes alone, trailing bytes and all.
  A selector a contract does not intercept is not intercepted: the call falls through, which is to say the deployed bytecode runs, and what it answers is that contract's own.
  The two control contracts have a fallback that reverts with `NotIntercepted()`; `KeylessDeploy` has none, so a selector it does not declare reverts with empty data while a `keylessDeploy` call the keyless rewrite did not take — one a contract makes — reaches the method body's `NotIntercepted()`; the Oracle's other selectors are methods it runs, and one it does not declare reverts with empty data.
  A method that takes no value answers a value-bearing call with `NonZeroTransfer()`, or with the error its own ABI names, after the selector matched — so a value-bearing call to an unknown selector still falls through.
  An answer is a synthetic frame result and carries the reservoir; an interceptor that lets the frame run charges nothing.
- **One hook prices every state charge.**
  `Host::state_gas_price(id, site)` is the single place an EIP-8037 state gas charge is priced.
  A charge is given back two ways, and neither reads a second table: `SSTORE`'s own restore, a caller's upfront charge for a call or creation that did not add the leaf, the transaction-level refund and the settlement of a synthetic frame result re-price the same `(id, site)` pair through the hook; a frame that fails rolls its own charges back by the amounts it recorded while running, without asking for a price at all.
  Both cancel the charge exactly whatever the price was — the re-priced sites because a bucket's multiplier is read once per transaction, so the two cannot disagree even if the environment's answer would, and the rolled-back amounts because nothing but the hook priced them.
  A mechanism that adds a state charge adds a `StateGasCharge` at the site the state lands on; it must not price the charge itself.
- **Three ledgers, and the bytes beside the third.**
  `MegaGasUsage` splits a transaction's raw spend into regular (compute), state and history gas, all paid from the EIP-8037 pools, reservoir first; state or history gas that spilled onto regular gas stays on its own ledger.
  The regular ledger is `total − state − history`, read off the result revm's `Gas` settled into; nothing counts compute beside it, so gas an inspector edits moves it with the receipt.
  The figure a block counts is `max(regular, floor)`, history taken out before the floor applies: the fork's `block_regular_gas_used` keeps history in, and subtracting it afterwards can land below the floor.
  The three add up to the raw spend, which is why what a history allowance paid for is on none of them: no pool of the transaction's gas paid it.
  `history_bytes` counts it all the same — the body, one 40-byte record per kept write, the kept logs and the deposited code — so the bytes at the cost per history byte exceed the history gas by exactly what allowances paid; an exempt transaction reports neither.
  It is the history the schedule prices, not the chain's physical growth: an exempt transaction's bytes are not in it, and a body counts its five fixed write records even when fewer fee accounts are written.
  For a transaction that pays history it is the data size the transaction kept less its Oracle hints' payloads: both columns read one count at every site, deployed code included, which the data-size limit makes before the creation commits and only for code revm deposits.
  `gas_used` is the receipt's figure; `BlockGasCounters` sums the three ledgers and the history bytes per block.

## Test Organization (`crates/mega-evm/tests/`)

- `satin/` — tests of the Satin engine (integration tests; add new ones here or in a new directory with a `main.rs`).
- `block/` — tests of block execution: the Karst block rules, the block-level limits, the counters, the factory and the admission gate.
- `system/` — tests of the system contracts: the interceptor dispatch, what each contract answers, keyless deployment (`system/keyless/`) and the system-address transaction.
- `_pending/` — the legacy tests the test inventory keeps, parked until the mechanism they test lands.
  It has no `main.rs`, so Cargo does not build it; its `README.md` names the mechanism that owns every file.
  The change that ports a pending test deletes it from `_pending/` in the same commit.
- Unit tests live next to the code in `#[cfg(test)] mod tests`.

## Test Gates

`REVIEW.md` lists every check a pull request meets; two of them are this repository's own execution gates.

- **The op-revm baseline** (`tests/satin/equivalence.rs`): an ordinary transaction through `MegaEvm` against op-revm on the same configuration, which Satin differs from by its history ledger alone.
- **The execution-spec gate** (`crates/mega-state-test`, `.github/workflows/exec-spec-satin.yml`): Ethereum's state-test fixtures through `MegaEvm`, on the fixture releases the fork's own runner (`.github/workflows/exec-spec.yml`) uses.
  - Equivalence mode is the gate: Satin's machinery — handler, frame lifecycle, Host, instruction table — priced as the fixture's fork prices it, through the neutral configuration (`MegaContext::with_neutral_cfg`, `test_utils::{neutral_cfg, neutralize_evm}`), which exists only behind `test-utils`, and held to no runtime limit (`EvmTxRuntimeLimits::no_limits()`, installed by the runner itself).
    Every failure must be explained by a deviation in `crates/mega-state-test/src/deviations.rs`, with its rule, its reason and the exact entries it explains, each with the hashes Satin produces, and every listed entry must fail exactly as listed; the executed and skipped counts are pinned in the workflow and equal the fork runner's.
  - Satin mode is a report: the same fixtures under Satin's own configuration, counted by outcome in the step summary; it never fails the job.

## Version Control

`satin` is the integration branch of the new engine; pull requests for Satin target `satin`.
`main` carries the legacy engine and is not touched from Satin work.
Both are protected: all changes go through PRs on GitHub.

### Branch naming convention

The naming convension for git branches is `[DEVELOPER NAME]/[CHANGE CATEGORY]/[SHORT DESCRIPTION]`, where:

- `[DEVELOPER NAME]` is the (nick)name of the developer.
- `[CHANGE CATEGORY]` should indicate what type of modifications this PR is making, e.g., feat, fix, doc, ci, refactor, etc.
- `[SHORT DESCRIPTION]` is a short (a few words) description of the detailed changes in this branch.

## Workflows

### Committing changes

When requested to commit changes, the agent should first review the current all changes in the working tree, regardless of whether they are staged or not.
There may be other changes in the worktree in addition to those made by the agent, which may also need to be included.
If the agent is not sure whether some changes should be included in the commit, ask the user.
The commit message should reflect the overall changes of the commit, which may beyond the existing context of the agent.

The commit message should be short and exclude any information of the agent itself.

### Creating PR

When a PR creation is requested, the agent should:

1. Check if the repo is current on a different branch other than `satin` or `main`.
   If not, create and checkout to a new branch.
   Make sure to inform the user about this branch creation.
2. Commit the changes in the worktree before fix linting issues.
3. Run lint check, and fix any lint warnings, and then commit if there are any changes.
4. Format the code and commit if there are any changes.
5. Push to the remote.
6. Use `gh` CLI tool to create a PR against `satin`.
   When generating the PR title and description, consider the overall changes in this branch across commits.
   In the PR description, make sure a `Summary` section is put on the top.
   The PR will be merged with `Squash and Merge` operation, whose commit description should include the summary.

### Implementing features or bug fixes

When the agent is requested to implement a new feature or bug fix, it should consider the following additional aspects in addition to the feature/fix itself and the other requirements by the user.

1. Should the documentation need to be updated (or added)?
2. Is there sufficient tests for this feature?

## Caveats for Agents

- **Satin is the only active spec; the legacy engine is frozen.**
  Do not add legacy specs, alias rungs or spec gates to this crate, and do not port a legacy mechanism the legacy test inventory retired.
  Behavior of the legacy specs is changed only on the legacy line, never here.
- **Build from the skeleton, one mechanism at a time.**
  Each module is filled by the mechanisms named in the module table; do not implement another mechanism to make yours work.
  If a change needs an interface that belongs to another mechanism, stub the smallest interface and name the mechanism that owns it.
- **Always test logic changes.**
  Any logic change or modification to mega-evm should be equipped with tests if there is no specific reason of not adding tests.
  The agent should always consider accompanying tests or suggest to add additional tests.
- **Keep the op-revm baseline honest.**
  A change that makes Satin differ from op-revm on purpose must update `tests/satin/equivalence.rs` (or add a case) so the difference is pinned, not silently absorbed.
- **Keep the execution-spec gate honest.**
  A change that makes Satin's machinery differ from Ethereum's fixtures on purpose registers a deviation (its rule, its reason, the entries it fails with the hashes Satin produces for them) and regenerates `crates/mega-state-test/DEVIATIONS.md`; a failure that is a bug is fixed, never registered.
  A price or a limit `MegaETH` sets is not a deviation: equivalence mode takes it out, and a new pricing or limit dimension extends the neutral configuration, or the limits the runner installs, rather than the registry.
- **Add benchmarks for performance-sensitive changes.**
  Changes on the EVM execution hot path must be accompanied by benchmarks.
  This includes new or modified opcode behavior, gas mechanics, system contract interception, resource limit tracking, and block executor pipeline changes.
  Per-PR instruction-count reports for these benchmarks are produced automatically by the CodSpeed CI workflow.
- **Always run benchmarks locally before committing.**
  New or modified benchmarks must be executed locally (`cargo bench -p mega-evm --bench <name>`) to verify they pass before committing.
  Benchmarks may compile but panic at runtime due to missing setup (e.g., required block fields), so compilation alone is not sufficient.
  For instruction-count deltas across a PR, use the CodSpeed report posted on the PR rather than local wall-clock numbers.
- **Use `test_` prefix for Rust test function names.**
  New `#[test]` functions should be named with a `test_` prefix for consistency with this repository and upstream revm style.
  If editing nearby tests in the same module, align names to the same `test_` style when reasonable.
- **System contracts carry over as they are.**
  Satin reuses the Solidity sources and bindings in `crates/system-contracts`; changing a contract needs a decision of its own, not a side effect of an engine change.
- **Rules for system contract interceptors (once the interceptors land).**
  For read-only or control methods, reject calls with non-zero `transfer_value` in the interceptor; if a method intentionally accepts value, document the reason in spec and code comments and add dedicated tests.
  Do not intercept unknown selectors: they fall through to on-chain bytecode and revert with a stable custom error such as `NotIntercepted()`.
  Only `CALL` and `STATICCALL` reach interceptor dispatch; `CALLCODE` and `DELEGATECALL` are rejected by the call-scheme guard before any interceptor is consulted.
  Interceptor tests cover the intercepted path, non-zero value, unknown selector fallback, and CALL vs DELEGATECALL/CALLCODE boundaries.
- **Validate per-fork parameters at load time.**
  A hardfork parameters type overrides `HardforkParams::validate()` with field-level invariant checks, so a bad chain config fails when it is loaded rather than at the fork's first block.
  The fork-requires-params rule is registered in `MegaHardforks::validate_schedule` too — one `require_params::<P>()` per params type — so a schedule that activates the fork without its params is rejected at load time.
- **Pre-block helpers must return state, not commit directly.**
  Any helper participating in pre-block execution (system contract deploys, pre-block system calls, etc.) returns `Option<EvmState>` and never calls `db.commit(...)` directly, so the witness generator sees the complete read and write set.
  `block/eips.rs` holds the ones that exist — the EIP-2935 and EIP-4788 calls and the post-block balance increments — and the block executor is what commits them.
- **Respect `no_std` in `mega-evm` crate.**
  Do not use `std::` directly.
  Follow the existing pattern: `#[cfg(not(feature = "std"))] use alloc as std;` then `use std::{vec::Vec, ...};`.
  Use `core::` for items like `fmt`, `cell`, `convert`.
- **All execution logic must be deterministic and architecture-independent.**
  Code that affects EVM execution results, gas computation, state transitions, or consensus-critical hashing must produce identical output regardless of target architecture, endianness, or pointer width.
  Never use `mem::transmute`, native-endian byte conversions, or platform-dependent operations in consensus paths.
  Use explicit little-endian (`from_le_bytes`/`to_le_bytes`) or big-endian conversions instead.
  When vendoring external code, audit for hidden platform dependencies (e.g., `zerocopy::transmute!` is native-endian).
- **`cargo sort` is enforced in CI.**
  Dependencies in `Cargo.toml` must follow the grouped-by-family convention with comment headers (`# alloy`, `# revm`, `# megaeth`, `# misc`) and be sorted alphabetically within each group.
- **Use `default-features = false` for new workspace dependencies.**
  This is the standard convention — features are opted-in explicitly.
- **Use `cargo check` (not `cargo clippy`) for compiler error checking.**
  Use `cargo clippy` only when specifically checking lint warnings.
- **Before finishing a change, always run full lint and format checks.**
  Run `cargo clippy --workspace --lib --examples --tests --benches --all-features --locked` before completion.
  Run `cargo fmt --all --check` before completion.
- **Keep documentation up to date.**
  When making changes, always check whether related documentation needs updating.
  The primary documentation is in `docs/`; it still describes the legacy engine until the Satin specification pages are written.
  The spec documentation is under `docs/spec/`, and the mega-evme documentation is under `docs/mega-evme/`.
  Also update this `AGENTS.md` when relevant (e.g., the module table, the unstable-spec marker).
- **One sentence, one line.**
  When writing markdown or similar format files, put each sentence in a separate line.
- **Run Prettier on docs before committing.**
  `docs/` markdown files are checked by Prettier in CI (`prettier --check 'docs/**/*.md'`).
  After editing any `docs/` file, run `npx prettier --write 'docs/**/*.md'` to fix formatting.

## Documentation Conventions (`docs/`)

The `docs/` directory is organized into two GitBook sections:

- **`docs/spec/`** — The public-facing specification for the MegaETH blockchain's execution layer — covering MegaEVM, system contracts, oracle services, resource metering, and the upgrade history.
  It is framed as a protocol specification, not as documentation for a specific crate.
- **`docs/mega-evme/`** — Documentation for the `mega-evme` CLI tool.

All conventions for writing and editing the spec documentation (audience, content rules, upgrade page format, writing style) are defined in [`docs/spec/AGENTS.md`](docs/spec/AGENTS.md).
