# AGENTS.md

This file provides guidance to AI agents (e.g., claude code, codex, cursor, etc.) when working with code in this repository.

## Project Overview

MegaEVM (mega-evm) — a specialized EVM implementation for MegaETH, built on **revm** and **op-revm** by customizing several hooks exposed by trait of revm.

## Build & Development Commands

```bash
# Build
cargo build
cargo build --release -p mega-evme       # CLI tool

# Test
cargo test                                # all tests
cargo test -p mega-evm                    # core crate only
cargo test -p mega-evm -- test_name       # single test

# Check compiler errors (preferred over clippy for quick checks)
cargo check
cargo check -p mega-evm

# Lint (CI runs all of these)
cargo fmt --all --check
cargo clippy --workspace --lib --examples --tests --benches --all-features --locked
cargo sort --check --workspace --grouped --order package,workspace,lints,profile,bin,benches,dependencies,dev-dependencies,features

# Benchmarks
cargo bench -p mega-evm --bench transact                                  # wall-clock + HTML report
cargo codspeed build -p mega-evm --bench <target> && cargo codspeed run   # instruction counts (Linux only)

# no_std check (run against riscv target)
cargo check -p mega-evm --target riscv64imac-unknown-none-elf --no-default-features

# System contracts (requires Foundry)
cd crates/system-contracts && forge build
```

Git submodules are required — clone with `--recursive` or run `git submodule update --init --recursive`.

## Workspace Structure

| Crate                   | Path                      | Purpose                                                                                     |
| ----------------------- | ------------------------- | ------------------------------------------------------------------------------------------- |
| `mega-evm`              | `crates/mega-evm`         | Core EVM implementation                                                                     |
| `mega-system-contracts` | `crates/system-contracts` | Solidity system contracts with Rust bindings (Foundry-based)                                |
| `mega-state-test`       | `crates/mega-state-test`  | State-test fixtures + runner library (EEST-compatible, published; imported as `state_test`) |
| `state-test`            | `crates/state-test`       | Thin CLI front-end over `mega-state-test` (not published)                                   |
| `mega-evme`             | `bin/mega-evme`           | CLI tool for EVM execution (`run`, `tx`, `replay`)                                          |
| `mega-t8n`              | `bin/mega-t8n`            | Standalone state transition (t8n) tool                                                      |

The EEST corpus sweep that exercises the unstable spec against the whole Ethereum state-test suite lives in `tools/eest-sweep/` and runs nightly (`.github/workflows/eest-nightly.yml`).

## Architecture

### Spec System (`MegaSpecId`)

Progression: `EQUIVALENCE` → `MINI_REX` → `REX` → `REX1` → `REX2` → `REX3` → `REX4` → `REX5` → `REX6` → `REX7`

- **Spec** defines EVM behavior (what the EVM does).
  Defined in `crates/mega-evm/src/evm/spec.rs`.
  The code base **MUST** maintain **backward-compatibility**, which means the semantics (i.e., EVM behaviors) must remain the same for existing specs.
  The only exception for this is the **unstable** spec that is under active development (if exists, must be the latest one).
  - _`REX7` is the current unstable spec under active development._
    When a new spec is introduced, this line should be updated to indicate the unstable spec.
  - Frozen and activated are separate properties.
    `REX6` is frozen but has no activation timestamp on mainnet or testnet, so both chains still execute `REX5`.
    Freezing forbids further semantic change; scheduling is a later, separate decision.
  - Specifications of each spec can be found in the upgrade pages under `docs/spec/upgrades/`.
- **Hardfork** (`MegaHardfork`) defines network upgrade events (when specs activate).
  Multiple hardforks can map to one spec.
  `MiniRex1` and `MiniRex2` are hardforks that reuse `EQUIVALENCE` and `MINI_REX` respectively.
  Defined in `crates/mega-evm/src/block/hardfork.rs`.
- All specs use `OpSpecId::ISTHMUS` as the Optimism base layer.
  But this is subject to change in the future.

### Core Source Layout (`crates/mega-evm/src/`)

- **`evm/`** — Core mega-evm logic: spec definitions, context, factory, execution pipeline, modified opcodes (LOG, SELFDESTRUCT), host hooks, precompiles.
  This module collects all our modifications and customizations of EVM's behavior for mega-evm based on the revm.
- **`block/`** — Block execution: executor, factory, hardfork-to-spec mapping, limit enforcement, and the canonical per-chain hardfork schedules.
  This module defines how a block in MegaETH block should be executed.
  `block/chain.rs` is the single source of truth for the mainnet/testnet chain IDs and activation-timestamp schedules (`hardfork_schedule(chain_id)`, `MAINNET_CHAIN_ID`, `TESTNET_CHAIN_ID`, `mainnet_hardforks()`, `testnet_hardforks()`); look there to find or change when a fork activates on a given chain.
- **`limit/`** — Resource limit tracking: compute gas, data size, KV updates, state growth (each in its own module).
  MegaETH introduces additional resource metering mechanism and this module implements their logic as utility structs to be used by mega-evm.
- **`access/`** — Block env access tracking and volatile data detection for parallel execution.
  MegaETH incorporates parallel EVM, so it is essential to reduce the conflicts between transactions by restricting the access to some "hot" resources.
  This module collects the logic of tracking the access to such hot resources during transaction execution.
- **`external/`** — External environmental context depended by mega-evm.
  The execution of mega-evm relies on some external environment information, in addition to revm's normal BlockEnv and CfgEnv.
- **`system/`** — System contract integration.
  MegaETH provides several system contracts that are predeployed on the chain.
  Any modification of such system contract must induce a new Spec to ensure backward compatibility.
- **`sandbox/`** — Isolated EVM execution
- **`constants.rs`** — All numeric constants organized by spec
- **`types.rs`** — Shared type definitions

### Key Concepts

#### Backward Compatibility of Specs

The spec system (`MegaSpecId`) forms a linear progression where each newer spec includes all previous behaviors.
The codebase **MUST** maintain backward-compatibility: EVM semantics must never change for existing (stable) specs.
The only exception is the latest spec if explicitly marked as **unstable**.
Consequently:

- Adding/modifying a system contract requires introducing a new spec.
- Changing gas costs, opcode behavior, or resource limits requires a new spec.
- Code should use `spec.is_enabled(MegaSpecId::X)` to gate spec-specific behavior.
- Modified opcodes (e.g., SELFDESTRUCT behavior varies by spec) are wired per-spec in the instruction table (`evm/instructions.rs`).

#### Dual Gas Model (Compute Gas vs Storage Gas)

MegaETH separates EVM gas into two independent dimensions tracked during execution:

- **Compute gas**: Measures pure computational cost.
  Through REX6 every opcode's gas consumption is recorded via wrapped instructions in `evm/instructions.rs` — `compute_gas_ext::*` for plain opcodes and `storage_gas_ext::*` for storage-affecting opcodes (SSTORE, LOG, CALL-family, CREATE/CREATE2, SELFDESTRUCT) — both invoking the shared `record_storage_compute_gas!` primitive after the opcode body completes.
  REX7 settles compute gas at checkpoints (storage-gas opcodes, CALL/CREATE family, volatile opcodes, `GAS`, frame entry/resume/exit) rather than after every plain opcode, and enforces limits inside plain segments with a gas clamp.
  A REX7 frame that ends in an exceptional halt splits its remaining budget: the work it performed before failing settles through the ordinary enforcing path, while the remainder it destroyed goes into a lane of `ComputeGasTracker` that the reported total and block accounting include but no limit comparison sees — destroyed gas is not work performed, and enforcing it would turn an EVM halt into a resource-limit failure with the gas rescued.
  The destroyed half is read from the frame's final result at `AdditionalLimit::finalize_frame`, so revm's create-return rejects and any rewrite an inspector's last callback made are both covered; storage gas a checkpoint body charged before aborting belongs to neither half.
  A precompile that fails never becomes a child EVM frame, so the same split is taken at `AdditionalLimit::finalize_frame` from the classification the call returns to its caller: executed work (the KZG fixed fee when verification ran; zero when the input was rejected before any work) enforces, and the unused caller-supplied envelope — including any REX5 forwarded-gas cap gap — is destroyed.
  The recording site stages the two numbers only it knows (the uncapped forwarded envelope, and `MegaETH`'s price for the work performed, which a halting precompile's `Gas` does not carry) and the settlement point takes the difference, so a classification an inspector rewrote after the dispatch is the one the split follows.
  A frame init that refuses to build a frame at all is settled at the same point, driven by the same classification: a halting refusal (a CREATE onto an occupied address) has its whole child budget destroyed, and a returning or reverting one books nothing because the caller gets the budget back.
  The split crosses the transaction boundary: `MegaTransactionOutcome` carries the destroyed part and the enforced part alongside the reported total, and `BlockLimiter` keeps `block_compute_gas_used` (reported) separate from `block_compute_gas_enforced` (the counter block admission compares).
  The destroyed part a transaction _reports_ is not the sum of those per-site bookings: `MegaHandler::last_frame_result` derives it once, from a conservation law over the envelope — `spent = C + S + D − K − I`, stated once as `ConservationTerms` (`limit/conservation.rs`) and read from `AdditionalLimit::conservation_terms()` by every site that derives, re-settles or checks it — and the per-site bookings stay as the enforcement split and as the `debug_assert` cross-check that the two agree.
  The derived number is reported and nothing else: the block's enforced counter accumulates `MegaTransactionOutcome::compute_gas_enforced`, read from `AdditionalLimit::enforced_compute_gas` (the per-site lane), rather than subtracting the reported destroyed total, so a missing term in the law misreports a statistic instead of repacking blocks.
  `minted_call_stipend` is the correction the law needs because revm mints `CALL_STIPEND` into a value-transferring call's child frame without debiting the caller, so recorded work exceeds the envelope by one stipend per such call; it is booked per mint event — at the CALL-family settlement, before frame init — so a value call turned away at frame entry (insufficient balance, call depth) books one too, because its refund returns the mint into the caller's envelope.
  `inspector_conjured_gas` is the same kind of correction for a producer outside the EVM: `MegaEvm` wraps every inspector it is handed in `MeasuredInspector`, which snapshots the interpreter's gas counter and a frame input's `gas_limit` across each callback and books the difference into `AdditionalLimit::inspector_ledger` — the EVM does not execute inside a callback, so anything that moves across one is the inspector's.
  Gas an inspector writes in was never debited from the transaction's envelope, so without the term the derivation reads such a transaction as having spent less than it did and can go negative; the term is zero for every uninspected transaction and every observation-only inspector.
  The same booking site shifts the checkpoint baseline and re-derives the gas clamp, so an inspector's edit never enters the compute measurement and never buys compute headroom.
  An edit to a frame *result*'s gas is booked instead from `finalize_frame`, because whether it moves the envelope at all depends on how the frame ends: a returning or reverting frame's remainder goes back to its caller and the edit is booked, a halting one's does not and the destroyed remainder is taken on the EVM's own number.
  The shim also counts, on the same ledger, the rewrites it sees that move no gas at all — a frame result's classification or output coming back changed, a frame's inputs edited anywhere but their gas limit, a frame the inspector answered itself with a synthetic outcome — because a rewrite that costs nothing still produces different state and a different receipt.
  The interpreter's pending action is measured on the same ledger: a frame holds its gas counter, plus a pending `NewFrame` action's `gas_limit`, or — once a terminating instruction has run — only the `Return` action's own copy, so the shim reads both objects at every live-interpreter callback and books the difference to the lane the action it was left holding names (the result lane for a `Return` action, settled at the frame's settlement point on the final classification; the envelope lane for a `NewFrame` one; the counter lane when the callback removed the action).
  A frame the inspector answers itself is the one place a difference across the callback is not the measurement, because no frame is built and the whole result is the inspector's: the shim stages the envelope the answering callback was handed, and `inspect_frame_init` settles the gas the result finally carries against it on the result lane — which also covers whatever of an edit to the inputs survives into a guard's replacement result, and which is zero for the echo convention every tool that intercepts follows.
  What no callback boundary can see stays invisible (the interpreter's stack and memory, direct journal writes), so an all-zero ledger says the shim saw no gas move and nothing it was handed come back changed, not that the transaction is the one the EVM would have produced alone.
  Two numbers inside the gas objects the shim *does* hold are known not to be covered — a `Gas`'s `refunded` and its EIP-8037 `reservoir` — and `crates/mega-evm/src/evm/AGENTS.md` carries the closed per-field enumeration that names them, pinned by `tests/rex7/gas_surface.rs`.
  The whole ledger travels on `MegaTransactionOutcome::inspector_ledger`, and the canonical block path — `run_transaction_with_sizes`, `run_tx_env_with_sizes`, and the `commit_tx_result` funnel every commit entry routes through — refuses a transaction whose ledger is non-zero with `MegaBlockExecutionError::InspectorAdjustedAccounting`, in release builds as well as debug.
  Observation is untouched (a tracer's ledger is empty, which is what every inspector on that path is today); an embedder that wants a rewriting inspector drives `MegaEvm::execute_transaction` directly, which supports it in full and is not covered by the guard — that is what leaves an off-band simulation EVM free to rewrite.
  Pre- and post-block system calls and the keyless-deploy sandbox are not entries the guard has to cover: neither produces a `MegaTransactionOutcome`, the ledger is reset at the start of every transaction, and both run uninspected anyway (`Handler::run_system_call` takes the plain frame loop; the sandbox builds its own EVM with no inspector).
  Subject to a per-spec compute gas limit and further restricted by gas detention (see below).
- **Storage gas**: Charges for persistent state modifications (SSTORE, account creation, contract deployment).
  These costs scale dynamically with SALT bucket capacity (see External Environment Dependencies below).
  LOG opcodes are charged in both dimensions: standard compute gas rates plus a storage gas multiplier on topic and data costs.

Both dimensions are enforced independently.
A transaction can be halted by exceeding either limit.

#### Frame Lifecycle and the Single Settlement Point

revm assembles a frame's result, decides its journal checkpoint and — for a contract creation — runs the deposit predicates and writes the code all inside `EthFrame::process_next_action`, and runs the inspector's last mutating callback after that function returns.
`evm/frame.rs` splits it: `classify_frame_action` decides what the frame's result is and records the journal decision it reached as a `FrameJournalVerdict`; `commit_frame_journal` carries that decision out.
Between the two run the frozen post-action charge, the inspector's `frame_end`, and `AdditionalLimit::finalize_frame` — the single point a frame's outcome is settled (final classification, executed/destroyed split, frame-init refusal booking, gas rescue, and the REX7 frame-local absorb).
Under REX7 the journal decision is taken later still — the frame loops park it on `MegaEvm::deferred_journal` and `frame_return_result` carries it out, after `AdditionalLimit::before_frame_return_result` (the last thing that can rewrite a frame's result) and before the caller resumes — so a frame's state agrees with the result its caller is handed and a creation's `set_code` still lands with no observation window.
Frozen specs take it where revm does, right after the classification, because what they replay includes the state a frame leaves behind when a later rewrite fails it.
The rewrite that made the extra station necessary is the late frame-local exceed: a per-frame budget is the frame's usage weighed against its *caller's* budget after the merge, so a frame can overrun one with nothing having latched it.
REX7 asks that question before the pop, through `AdditionalLimit::peek_check_limit_after_pop` over `FrameLimitTracker::view_after_pop`, and rewrites the frame to a revert first; the pop then discards the frame's usage the way it discards any reverting frame's, and the caller carries on.
The pre-pop reading and the post-pop `check_limit()` are cross-checked against each other on every frame return in debug builds, on every spec — that assertion is what stands between the early decision and a drift in what counts as a frame-local exceed.
Both frame loops (`frame_run` / `inspect_frame_run`) and both frame-init paths (`frame_init` / `inspect_frame_init`) run the same bodies; the inspected copies add exactly one thing, the callback that can rewrite a frame's classification.
`classify_frame_action` and `commit_frame_journal` together are a re-ordering of upstream code with no type-level tie to it, so a revm bump has to re-audit them; the debug assertion in `classify_create_return` catches only the one drift class it names.

#### Multidimensional Resource Limits

Beyond the dual gas model, mega-evm enforces **four independent per-transaction resource limits** via `AdditionalLimit` (`limit/limit.rs`):

- **Compute gas** — Computational opcode cost
- **Data size** — Calldata + logs + storage writes + code deploy + account updates
- **KV updates** — Storage writes + account modifications (net, with refunds)
- **State growth** — Net new accounts + net new storage slots (not all specs enable this)

Each dimension has its own per-spec limit defined in `constants.rs`.
All trackers are **frame-aware**: reverted inner calls discard their tracked usage, successful calls merge into the parent frame.
When any limit is exceeded, execution halts with `OutOfGas` and remaining gas is preserved for refund.

#### Gas Detention (Volatile Data Access Restriction)

MegaETH's parallel EVM needs to minimize conflicts between concurrent transactions.
"Volatile" data — block environment fields (NUMBER, TIMESTAMP, COINBASE, etc.), the beneficiary's account state, and the oracle contract — is frequently read by many transactions and thus a major source of conflicts.

**Gas detention** restricts computation after volatile data is accessed by capping the remaining compute gas:

- Different volatile data categories (block env/beneficiary, oracle) have different cap levels defined in `constants.rs`.
- The **most restrictive cap wins** when multiple volatile sources are accessed.
- Caps are applied via host hooks (`evm/host.rs`) that mark access in a `VolatileDataAccessTracker` (`access/tracker.rs`), then enforced after each volatile opcode via `wrap_op_detain_gas!` in `evm/instructions.rs`.
- REX7 charges the opcode's static fee even when `disableVolatileDataAccess` rejects (charge-on-reject); frozen specs still reject for free.
- REX7 specifies that a detention mark is produced when the target account is loaded, so a frame that cannot afford the pre-load CALL / EXTCODECOPY fees produces no mark (the frozen-window tripwire is `!REX7`-gated).

This forces transactions that touch volatile data to terminate quickly, reducing parallel execution conflicts without banning the access outright.
Detained gas is effectively refunded — users only pay for actual computation performed.

#### Storage Gas Stipend (Rex4+)

MegaETH's 10× storage gas multiplier on LOG opcodes causes even `LOG1` to cost 4,500 gas, exceeding the EVM's `CALL_STIPEND` of 2,300.
Rex4 introduced `STORAGE_CALL_STIPEND` (23,000 gas) for internal (`depth > 0`) value-transferring `CALL`/`CALLCODE`; Rex5 reworked it into a separated allowance, so the tracker is dual-mode.
Under Rex5 the stipend is a per-frame allowance that does NOT inflate the child's `gas_limit`: it is drawn only at MegaETH's storage-gas surcharge sites (LOG topic/data, new-account materialization, first-time-write SSTORE, contract-creation storage, SELFDESTRUCT beneficiary creation), is structurally unspendable on compute, and is neither returned to the caller nor rescued for the sender — nothing is burned because nothing ever enters the frame's gas limit.
Under Rex4 (legacy mode) the stipend instead inflates the child's `gas_limit`, a per-frame compute gas cap keeps the extra gas usable only for storage-gas-heavy operations, and unused stipend is burned on return.

The stipend lifecycle is managed by `StorageCallStipendTracker` (`limit/storage_call_stipend.rs`), which maintains a per-frame stack aligned with the EVM call stack.
The tracker's `before_frame_init` method is called inside `AdditionalLimit::before_frame_init`, after all four sub-trackers push their frames (so the compute gas frame exists for the Rex4 legacy-mode per-frame cap).

The storage gas stipend is subject to the general gas leakage pitfalls described below.

#### System Contracts

MegaETH pre-deploys system contracts at well-known addresses (`0x634200...0001`, `0002`, `0003`, etc.).
They are deployed idempotently during `pre_execution_changes()` in `block/executor.rs`, gated by hardfork activation:

| Contract                 | Address suffix | Purpose                                             |
| ------------------------ | -------------- | --------------------------------------------------- |
| Oracle                   | `...0001`      | External key-value storage with hint support        |
| High-Precision Timestamp | `...0002`      | Sub-second block timestamp                          |
| Keyless Deploy           | `...0003`      | Deterministic contract deployment via Nick's Method |
| MegaAccessControl        | `...0004`      | Access control (disableVolatileDataAccess)          |
| MegaLimitControl         | `...0005`      | Limit query/control (currently remainingComputeGas) |
| SequencerRegistry        | `...0006`      | System address and sequencer role registry          |

Key design aspects:

- Solidity sources in `crates/system-contracts/contracts/`, compiled by Foundry, with Rust ABI bindings generated via `alloy-sol-types`.
- Bytecode is versioned and hash-verified at build time (`crates/system-contracts/build.rs`).
- The **MEGA_SYSTEM_ADDRESS** can call whitelisted system contracts as deposit-like transactions — no signature or fee required.
  This is how the sequencer updates oracle storage.
- **Any system contract modification requires a new spec** to preserve backward compatibility.

#### External Environment Dependencies

mega-evm requires external context beyond revm's standard `BlockEnv`/`CfgEnv`, provided via the `ExternalEnvFactory` trait (`external/factory.rs`):

- **SALT environment** (`external/salt.rs`): Provides bucket capacity data for dynamic gas pricing.
  Each account and storage slot maps to a SALT bucket; gas cost = base cost × (bucket_capacity / MIN_BUCKET_SIZE).
  This makes storage operations more expensive in crowded state regions, preventing state bloat.
  Implementation: `DynamicGasCost` struct (`external/gas.rs`) lazily caches bucket multipliers.
- **Oracle environment** (`external/oracle.rs`): Supplies storage values for the oracle contract via `get_oracle_storage(slot)`.
  Oracle reads in `sload` are **always forced cold** for deterministic replay.
  The `on_hint(from, topic, data)` callback enables synchronous oracle hints during execution.
- An `EmptyExternalEnv` implementation disables both features (returns minimum bucket size, no oracle data) for testing or standalone use.

### Gas Leakage Pitfalls

Any mechanism that grants, inflates, or adjusts gas on a per-frame basis (e.g., `STORAGE_CALL_STIPEND`, and any future per-frame gas adjustment) must account for all frame termination paths.
Failing to do so can cause system-granted gas to leak back to the parent call frame or the transaction sender.
The following paths are common sources of leakage:

1. **System contract interception** (`system/intercept.rs`): Interceptors short-circuit `frame_init` before `AdditionalLimit::before_frame_init` runs, producing a synthetic `FrameResult` without a real child EVM frame.
   Any per-frame gas adjustment applied in `before_frame_init` is skipped on this path.
   The `push_empty_frame()` call maintains stack alignment but does not apply adjustments.
   Synthetic results must not assume any per-frame gas mechanism was applied.
   Such a result does reach `finalize_frame`, as `FrameExit::RefusedSynthetically`: under REX7 its envelope is settled like any other refusal's, while frozen specs leave it alone.
2. **Gas rescue on TX-level limit exceed** (`limit/limit.rs`): When a transaction-level resource limit is exceeded, `rescue_gas` captures remaining gas for sender refund.
   If a frame's gas was inflated by a per-frame mechanism, the rescued amount must exclude the inflated portion — otherwise the sender recovers system-granted gas that should have been burned.
3. **Frame return** (`limit/limit.rs`): `before_frame_return_result` is the final hook before gas is returned to the parent.
   Any per-frame gas adjustment must be unwound here (e.g., burning unused granted gas).
   The unwinding must apply identically on success and revert — conditional unwinding can leak gas on one path.

When adding a new per-frame gas mechanism, verify that all three paths handle it correctly and add tests for each.

### Resource-Limit Check Protocol

The per-opcode hot path records and checks only the compute-gas dimension (`AdditionalLimit::record_compute_gas`).
Correctness of the other three dimensions (data size, KV updates, state growth) rests on a protocol instead of a per-opcode fan-out across all four trackers:

1. **Every non-compute mutation site must latch.**
   Any code that records data-size/KV/state-growth usage during execution (`on_sstore`, `on_log`, `record_oracle_hint_bytes`, the frame-lifecycle hooks) must run `check_limit()` itself, latching any exceed into `has_exceeded_limit`.
   The latch is surfaced by the leading short-circuit of the next `record_compute_gas` call (through REX6, that is the next metered opcode; under REX7 checkpoint accounting it is the next checkpoint), so the halt lands on the same site as the pre-protocol fan-out did.
2. **Pre-inner recorders must NOT latch.**
   A site that records usage _before_ its inner instruction executes (currently SELFDESTRUCT's two beneficiary recorders: empty-beneficiary creation and the REX6+ existing-beneficiary credit) must record without latching: the inner instruction can still fail, the frame then discards the usage, and an early latch would stick and rewrite the frame's real result.
   Such opcodes use a trailing all-dimension check (`record_compute_gas_all_dims`) that runs only after the inner instruction succeeds.
3. **Compute gas is always recorded.**
   `record_compute_gas` must record before surfacing any latched exceed — the recorded total feeds the transaction outcome and block-level compute accounting even for transactions halted on another dimension.

The protocol governs mutation sites that run during execution; the REX6+ post-execution fee-reward accounting is deliberately outside it.
That accounting merges usage into the transaction's reported totals and the block-level cumulative counters after the execution result is final, without latching, and never retroactively fails the transaction.

Rule 1 is backed by a `debug_assert!` inside `record_compute_gas_impl`, reached through the guarded entry `record_compute_gas` (`GUARD_LATCH_PROTOCOL = true`): if a non-compute dimension is over its limit but not yet latched, the assert trips at the exact opcode whose mutation site forgot to call `check_limit()`.
The sub-tracker checks are non-mutating, so the guard compiles out of release builds.
The same impl is also reached through `record_compute_gas_unguarded` (`GUARD_LATCH_PROTOCOL = false`), which skips the assert.
REX7 frame-exit tail settlement (`after_frame_run_instructions`) uses the unguarded entry: that settlement can observe SELFDESTRUCT pre-inner recorder usage that rule 2 deliberately left unlatched, because the frame is about to pop and discard it, and the guarded entry would trip the assert on that path.

When adding an opcode or mutation site that touches a non-compute dimension, decide whether it records after or before its inner instruction, follow the matching case above, and add a test asserting the exceed halts at that opcode.

## Test Organization (`crates/mega-evm/tests/`)

Tests are organized by spec: `equivalence/`, `mini_rex/` (12 modules), `rex/`, `rex2/`, `rex3/`, `rex4/`, `rex5/`, `rex6/`, and `block_executor/`.
Each module tests specific features of that spec.

## Version Control

The main branch is `main`, but it's protected.
All change should be made via PRs on GitHub.

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

1. Check if the repo is current on a different branch other than `main`.
   If not, create and checkout to a new branch.
   Make sure to inform the user about this branch creation.
2. Commit the changes in the worktree before fix linting issues.
3. Run lint check, and fix any lint warnings, and then commit if there are any changes.
4. Format the code and commit if there are any changes.
5. Push to the remote.
6. Use `gh` CLI tool to create a PR.
   When generating the PR title and description, consider the overall changes in this branch across commits.
   In the PR description, make sure a `Summary` section is put on the top.
   The PR will be merged with `Squash and Merge` operation, whose commit description should include the summary.

### Implementing features or bug fixes

When the agent is requested to implement a new feature or bug fix, it should consider the following additional aspects in addition to the feature/fix itself and the other requirements by the user.

1. Should the documentation need to be updated (or added)?
2. Is there sufficient tests for this feature?

## Caveats for Agents

- **Always test logic changes.**
  Any logic change or modification to mega-evm should be equipped with tests if there is no specific reason of not adding tests.
  The agent should always consider accompanying tests or suggest to add additional tests.
- **Add benchmarks for performance-sensitive changes.**
  Changes on the EVM execution hot path must be accompanied by benchmarks.
  This includes new or modified opcode behavior, gas mechanics, system contract interception, resource limit tracking, and block executor pipeline changes.
  Per-PR instruction-count reports for these benchmarks are produced automatically by the CodSpeed CI workflow.
- **Always run benchmarks locally before committing.**
  New or modified benchmarks must be executed locally (`cargo bench -p mega-evm --bench <name>`) to verify they pass before committing.
  Benchmarks may compile but panic at runtime due to missing setup (e.g., required block fields), so compilation alone is not sufficient.
  For instruction-count deltas across a PR, use the CodSpeed report posted on the PR rather than local wall-clock numbers.
- **Re-run the destroyed-gas conservation scan after a revm / alloy-evm upgrade.**
  The REX7 destroyed total is derived from the envelope, so any upstream change that moves gas without a MegaETH site recording it — a new minted subsidy like `CALL_STIPEND`, a changed refund or floor ordering, a new component of `total_gas_spent` — becomes a missing term in the law rather than a compile error.
  The frame-lifecycle mirror in `evm/frame.rs` is the same kind of exposure in the other direction: it is a re-ordering of `EthFrame::process_next_action` and `return_create`, so an upstream change to either becomes a silent divergence rather than a compile error.
  After bumping revm or alloy-evm, diff those two upstream functions against `evm/frame.rs`, then run `cargo test -p mega-evm` and `cargo test -p mega-state-test -p state-test` (the `debug_assert` cross-check is live in debug builds) plus the replay fixtures under the latest spec (`cargo run -p state-test -- --bench --bench-spec <LatestSpec> bench/replay/fixtures`), whose own `post` expectations pin an older spec and would otherwise give the derivation no coverage.
- **Use `test_` prefix for Rust test function names.**
  New `#[test]` functions should be named with a `test_` prefix for consistency with this repository and upstream revm style.
  If editing nearby tests in the same module, align names to the same `test_` style when reasonable.
- **Do NOT modify behavior for existing stable specs.**
  All specs through `REX6` are frozen; `REX7` is the unstable spec under active development.
  A spec being frozen is independent of whether any network has scheduled it — `REX6` is frozen and unscheduled, and is still off-limits to behavior changes.
  New EVM behavior, gas cost changes, or opcode modifications for stable specs **must** introduce a new spec and be gated with `spec.is_enabled(MegaSpecId::NEW_SPEC)`.
  Never change what an existing stable spec does.
- **System contract changes require a new spec.**
  Do not modify system contract Solidity sources or their Rust integration without also introducing a new spec for backward compatibility.
- **The gas schedule belongs to the spec, not to `CfgEnv`.**
  revm 40 made every operation's price a `CfgEnv.gas_params` table an embedder can rewrite, but several `MegaETH` accounting sites carry the schedule's values as constants (the `CALL_STIPEND` a value-transferring call mints, the pre-`REX7` per-byte code-deposit rate, the mainnet table the keyless-deploy preflight estimates intrinsic gas from).
  A configuration whose `gas_params` is not exactly `GasParams::new_spec(SpecId::from(cfg.spec))` is therefore rejected with a panic rather than executed, at both `with_cfg` entry points, at the deprecated `new_with_context`, and again at the point of use before every transaction — so a configuration mutated in place after the context was built is caught too.
  The check is unconditional across specs: it governs the configuration domain, which no historical block covers.
  Build configurations with `CfgEnv::new_with_spec(spec)` or `cfg.set_spec_and_mainnet_gas_params(spec)`; do not add a way to opt out, and do not add a tool-only bypass.
  New code that needs one of the schedule's values may read it from `cfg().gas_params()` or restate the constant — under the pin the two are equal, and reading the table is the preferred form for unfrozen specs.
- **Override `HardforkParams::validate()` for every new params type.**
  The default implementation accepts any value silently.
  Override it with field-level invariant checks (e.g., non-zero addresses) so that `with_params()` panics loudly at chain-config load time rather than allowing the error to surface at the first block where the fork activates.
- **Pre-block helpers must return state, not commit directly.**
  Any helper participating in `pre_execution_changes` (system contract deploys, pre-block system calls, etc.) MUST return `Option<EvmState>` and never call `db.commit(...)` directly.
  Full convention: `crates/mega-evm/src/system/AGENTS.md` → `PRE-BLOCK STATE CHANGE CONTRACT`.
- **Define value-transfer policy explicitly for system contract interceptors.**
  For read-only or control methods, reject calls with non-zero `transfer_value` in the interceptor.
  If a method intentionally accepts value, document the reason in spec and code comments and add dedicated tests.
- **Do not intercept unknown selectors for system contracts.**
  Unknown selectors should fall through to on-chain bytecode and revert with a stable custom error such as `NotIntercepted()`.
- **Only `CALL` and `STATICCALL` reach interceptor dispatch.**
  `CALLCODE` and `DELEGATECALL` are rejected by the call-scheme guard in `frame_init` before any interceptor is consulted.
  Do not expect these schemes to trigger system contract interception.
- **System contract interceptor tests must cover boundary behaviors.**
  Include tests for normal intercepted path, non-zero value behavior, unknown selector fallback, and CALL vs DELEGATECALL/CALLCODE interception boundaries.
- **A new precompile that can halt after doing work must record that work explicitly.**
  Under REX7+ the generic precompile-halt arm books zero executed compute gas and destroys the whole forwarded envelope, because every wired precompile halts only on a pre-work input rejection.
  A precompile with a do-work-then-halt path (a future verification precompile that halts after running its check) would leave that work unenforced, so a caller could repeat the failure without the transaction- or block-level compute limits ever accounting for it.
  Express failure-after-work as a `revert` instead of a halt (the revert arm records actual spend), or give the precompile its own recording arm the way KZG does.
  Only code that compiles into the node can register a precompile, so this is a rule for future authors, not an on-chain attack surface.
  Do not override the KZG address — or any precompile that has its own recording arm — without updating that arm to match the replacement; a substitute registered under `PrecompileId::KzgPointEvaluation` is a new precompile and must ship its own arm.
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
  The primary documentation is in `docs/`.
  The spec documentation is under `docs/spec/`, and the mega-evme documentation is under `docs/mega-evme/`.
  Also update this `AGENTS.md` when relevant (e.g., unstable spec marker, spec progression list, system contract table).
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
