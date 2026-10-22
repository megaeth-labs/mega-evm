---
name: satin-review
description: Guide for reviewing the Satin engine pull request (#386) in megaeth-labs/mega-evm, which holds the whole engine now that the stacked pull requests have merged into it. Use when reviewing #386 or the Satin engine, or when the user says "review satin". Gives the scope and the review range, which modules are stable and which #407 still changes, the module map with the invariants to check, the reading order, how to rate a finding, the revert-or-halt rule with its deliberate exceptions, and the decisions already made. Temporary: removed before the Satin pull request merges.
---

# Reviewing the Satin engine (pull request #386)

This guide is temporary.
It is deleted before the Satin pull request merges, so nothing else may link to it.
It is a map: where each part of the engine lives, what to check there, what is still changing, and what is already decided.
Implementation detail lives in [`AGENTS.md`](../../../AGENTS.md) and the specification in [`docs/spec/upgrades/satin.md`](../../../docs/spec/upgrades/satin.md); this guide links them and does not restate them.
[`REVIEW.md`](../../../REVIEW.md) is the repository's own review rubric; its consensus-critical list overlaps the checks below.
The guide describes the code at `cz/feat/satin-w2`; #407 updates it when it merges.

## What is under review

- The Satin engine, in pull request #386 (`cz/feat/satin-w2`), judged at its final state.
  The stacked pull requests #387, #388, #390, #399 and #400 (with #404, merged into #400's branch) have merged into it; each description explains its part as it was merged, and a later part may have changed an earlier one.
- The review range is the pull request's own diff.
  Its base, `cz/feat/satin-review-base`, is `satin` before the Satin skeleton: `a8f8c7c9`, the CI-only commit before it.
  So GitHub's "Files changed" holds the skeleton (#385) and every later series: the whole engine.
  The local equivalent is `git diff a8f8c7c9...<head>`, `<head>` being the head of #386.
  Most of its deletions are the legacy engine the skeleton removed (183 files): skim them, or hide them with `--diff-filter=d`.
- #386 targets the review base only while it is reviewed.
  It is retargeted to `satin` before it merges, after #407 has merged into it, and it must never be merged into the review base.
- One pull request still changes part of the engine: #407 (`cz/feat/satin-w9` into this branch), which merges before #386 merges into `satin`; see "What is still moving".
- Judge the final code.
  A defect an earlier commit introduced and a later one fixed is not a defect; history only explains why something is as it is.
  A test deleted or parked in an intermediate commit is not a finding either: test completeness is judged on the whole.
- Satin is the first spec of a line, so keep the extension points a line needs (spec-keyed constants, a spec argument); a later fork that first needs configuration parameters adds a mechanism for them then.
  Satin is a new spec: simpler semantics win over parity with the legacy engine.
- Out of scope: the legacy engine (`main`, frozen), which runs every block before Satin activates.
  Satin never executes or decodes legacy-era data, so "Satin cannot handle legacy data" is not a defect, and code kept only for that purpose is dead code.
- Fork code: the `megaeth-labs/revm` and `megaeth-labs/op-revm` forks are pinned dependencies with their own review; their diff is not part of this pull request.
  On this branch they are pinned at the tags `v40.0.3-mega.4` and `v20.0.0-mega.3`.
  #407 pins them by commit (`rev`) instead; the fork pull requests megaeth-labs/revm#76 (stacked on megaeth-labs/revm#75) and megaeth-labs/op-revm#8 are the review of those commits.
  A finding about a fork function is about how mega-evm uses it, unless it shows the fork breaking its own documented default behaviour.
  The fork functions the engine relies on are listed in [`AGENTS.md`, "Dependencies on the forks"](../../../AGENTS.md#dependencies-on-the-forks).
- Where code belongs: upstream revm first, then the fork, then mega-evm.
  The fork is only for data from the middle of revm's own flow that mega-evm cannot reach from outside; what can be done at a boundary mega-evm owns (frame start or end, the Host, the handler, the inspect path) belongs in mega-evm.
  For each mechanism, ask whether it sits in the cheapest layer.
- The OP base may move (Karst to a later OP fork) before launch; a finding that depends on Karst specifics should say so.

## What is still moving

#407 changes part of the engine before #386 merges.
It meters writes and frame starts in the revm fork's instruction hooks on a per-frame byte pool, changes the gas detention and write-record rules with it, and deletes the inspector admission gate.
The work still to land in it runs inspectors on revm's own inspected lifecycle, removing the engine's inspected copies of the frame lifecycle, its step guard and the inspector's rewrite settlement; gives the pre-block calls one fixed gas budget; and restores the KV and state-growth limits, the 98/100 per-frame shares and the refunds for an account created and destroyed in the same transaction.
That work also makes a write that crosses a frame's budget and the transaction's limit at once name the budget in its stop.
Review the stable modules now and a changing one after #407 merges into this branch; if you read a changing module earlier, report a defect only if #407 would not remove it.
This table is updated when #407 merges.
Paths are relative to `crates/mega-evm/` unless they start with another top-level directory.

| Module                                  | Files                                                                                                                        | Status                                                                                                             |
| --------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| Block executor and block rules          | `src/block/` except `limit.rs`, `eips.rs` and `genesis.rs`                                                                   | Stable, except the inspector admission gate in `executor.rs` and `factory.rs`                                      |
| System contracts and the system address | `src/system/` except `keyless/`; `crates/system-contracts/`                                                                  | Stable, except the pre-block call's gas budget in `sequencer_registry.rs`                                          |
| Pricing                                 | `src/evm/prices.rs`, `src/evm/history.rs`, `src/evm/precompiles.rs`, `src/external/`                                         | Stable                                                                                                             |
| Node surface and the witness            | `src/lib.rs`, `src/evm/factory.rs`, `src/evm/state.rs`, `src/test_utils/witness.rs`, `crates/mega-state-test/src/witness.rs` | Stable; `src/lib.rs` drops the exports of what #407 deletes                                                        |
| Tools, benches and CI                   | `bin/mega-evme/`, `benches/`, `.github/workflows/`, `scripts/`, `mutants/`                                                   | Stable; #407 changes how `mega-evme` attaches its tracer and how CI reads a `rev` pin                              |
| Limits                                  | `src/limit/`, `src/block/limit.rs`, `src/constants.rs`                                                                       | Changing: the byte pool replaces the lanes; KV and state-growth limits per transaction, frame and block            |
| Execution and frames                    | `src/evm/`: `execution.rs`, `context.rs`, `frame.rs`, `host.rs`, `instructions.rs`, `result.rs`, `mod.rs`                    | Changing: writes and frame starts in the fork's hooks; the per-frame shares; the destruction refunds               |
| Gas detention                           | `src/access/`                                                                                                                | Changing: stipend-funded work is compute; a halting frame's burn is read off the fork                              |
| Inspector contract                      | `src/evm/inspector.rs` and the admission gate                                                                                | Changing: deleted; inspectors run on revm's inspected lifecycle, and block execution admits any inspector          |
| Keyless deployment                      | `src/system/keyless/`                                                                                                        | Changing                                                                                                           |
| Gas schedule                            | `src/evm/schedule.rs`                                                                                                        | Changing: the history allowance and the frame byte share enter the schedule                                        |
| Pre-block calls                         | `src/block/eips.rs`                                                                                                          | Changing: one fixed gas budget                                                                                     |
| Genesis and stateless validation        | `src/block/genesis.rs`, `src/evm/validation.rs`                                                                              | Changing: how a repeated `satinTime`, or one in object form, reads; how intrinsic gas separates history from state |
| Outcome tooling                         | `crates/state-test/`, `crates/mega-state-test/` except `src/witness.rs`                                                      | Changing: an outcome digest, and runs under tighter limits                                                         |
| Tests                                   | `tests/` of the changing modules, `src/test_utils/neutral.rs`                                                                | Changing with them                                                                                                 |
| Specification and guides                | `docs/spec/upgrades/satin.md`, `AGENTS.md`, `REVIEW.md`                                                                      | Changing with the rules above                                                                                      |

## Running the checks

Commands are in [`AGENTS.md`, "Build & Development Commands"](../../../AGENTS.md#build--development-commands) and the gates in [`AGENTS.md`, "Test Gates"](../../../AGENTS.md#test-gates).
Four notes that are easy to miss:

- the `mega-evme` tests run with `--manifest-path bin/mega-evme/Cargo.toml`, not `-p`;
- run the suite with `MEGA_SATIN_CPSB` and `MEGA_SATIN_CPHB` unset, or the tests that pin the spec's byte prices skip;
- after any test change, run `scripts/price_grid.sh --pr`;
- the execution-spec gate needs Ethereum's fixture releases, at the versions `.github/workflows/exec-spec-satin.yml` downloads.

Evidence that a change alters no result: `state-test --mode satin --json-outcome` at two commits, detached, sorted and compared byte for byte, plus the gates and the byte-price grid.
A golden or snapshot changes only for a stated reason: regenerate it and read the diff.

## Reading order

1. The consensus core: limits (the vocabulary of lane, stop and crossing), then execution and frames with gas detention, then the block executor, then the system contracts and system address.
2. The gas schedule and pricing.
3. The inspector contract.
4. The node surface, validation and the witness.
5. Tools, tests, goldens and CI. Skim the goldens and snapshots; read what changes them.

Until #407 merges, take the stable modules of the table above first, in this order, and the changing ones after it.
Skim without reading: the SALT bucket-hasher internals, the generated pricing table, the tracer snapshots, the receipt goldens and the keyless differential digest.
The module documentation at the top of the main source files states the same invariants as the lists below, often more exactly; each entry names which to read first.
A test name or a line count is not a reason to trust the code: read the code against the spec.

## Glossary of the terms the checks use

- **Live system address**: the address the `SequencerRegistry` names (`_currentSystemAddress`).
  `MEGA_SYSTEM_ADDRESS` in code is only the default a test's registry names (`test_utils::MemoryDatabase::sequencer_registry`).
- **Pre-block caller**: `0xff…fe`, the EIP-4788 system address, the caller of the pre-block system calls.
  It is a different address from the live system address.
- **MST** (Mega System Transaction): a signed legacy transaction from the live system address to a whitelisted system contract (today the Oracle), promoted to a deposit by the engine.
- **System-originated** (spec §19, `originates_from_the_protocol` in `src/system/tx.rs`), decided from the transaction's own fields:
  an MST before or after promotion; any transaction whose caller is the pre-block caller; any deposit carrying the source hash the promotion stamps, `keccak256("MEGA_SYSTEM_TRANSACTION")`, whoever sends it.
  Such work is exempt from history gas, per-transaction limits and detention, and priced at the minimum bucket.
- **Lane**: the per-frame record of data size and write records; a success merges into the caller's lane, a failure discards it.
- **Stop**: what a limit does. The frame that crosses it reverts with `MegaLimitExceeded(kind, limit)`, its caller resumes and may catch it.
- **Answered frame**: a frame that gets a result without running (an interceptor, the engine, an inspector's hook).
- **Byte-price grid**: the CI job that runs the suite at other costs per state and history byte, because the prices are provisional.

## Module map

Paths are relative to `crates/mega-evm/` unless they start with another top-level directory.
For each module: where it is, what to check, the spec sections, and the `AGENTS.md` subsection with the detail.

### Limits: data size, write records, the stop (changing in #407)

- **Where:** `src/limit/` (byte table, `LimitKind`, `AdditionalLimit`, `FrameLimitTracker`, `Lane`, `StagedRecord`), `src/block/limit.rs` (`ProtocolLimits::for_spec`), `src/constants.rs`.
- **Read first:** the module docs of `src/limit/mod.rs` and `src/limit/limit.rs`.
- **Check:**
  - A record is counted where it is made and taken back with its frame; the one exception is a failed creation's creator-nonce record, which lands on the creator and is held to the creator's limits.
  - The transaction limit is checked before the frame budget, at one site (`AdditionalLimit::check`); every stop comes from `AdditionalLimit::crossed`, which also applies the exemption of system-originated work.
  - A caller resumed after its child's crossing is held to what the transaction has left, and stops in turn if a charge of its own crosses again.
  - `LimitKind` keeps its encoding: data size 0, compute gas 2, and `from_u8` answers `None` for 1 and 3.
  - Limits are per-spec constants, never chain configuration; a bare `MegaContext` defaults to them; `BlockLimits` is builder policy and can only tighten.
  - Nothing limits the KV count, state growth or the block's execution gas; the ledgers and `BlockGasCounters` report them.
  - `KV × 40 ≤ data size` holds after every instruction.
  - A hint that would cross the limit is neither counted nor forwarded; an admitted hint stays counted through any stop.
- **Spec:** §7, §10, §11, §19, §20. **Detail:** [`AGENTS.md`, "Contracts of the common execution layer"](../../../AGENTS.md#contracts-of-the-common-execution-layer).

### Execution and frames (changing in #407)

- **Where:** `src/evm/execution.rs` (`MegaHandler`, the frame lifecycle), `src/evm/context.rs` (`MegaContext`, `spec_cfg`, `admit_code_deposit`), `src/evm/mod.rs`, `src/evm/host.rs`, `src/evm/state.rs`, `src/evm/instructions.rs`, `src/evm/frame.rs`, `src/evm/result.rs`.
- **Read first:** the module docs of `src/evm/execution.rs` and `src/evm/host.rs`.
- **Check:**
  - The Host only observes; a staged record is committed after the opcode completes and dropped when it fails.
  - Pre-frame charges (a deposit's caller account, EIP-7702 authorities, pre-frame records) either succeed or leave an included out-of-gas halt without a frame, with the journal unwound as revm unwinds it (a creation keeps its nonce bump).
  - A frame whose start would cross a limit is answered with the stop before it is built, so no value moves; `take_back_a_stopped_answer` takes back what an answer rewritten into a crossing had moved.
  - A frame start is counted before revm decides the frame, by predicting its refusals (`caller_refuses_start`, `assert_start_as_counted`); #407 replaces the prediction.
  - A deposit that fails validation or cannot pay a pre-frame charge is included as a failed deposit: whole gas limit used, mint credited, nonce bumped.
  - `spec_cfg` fixes the configuration whatever the caller passes (Satin schedule, EIP-8037, EIP-2780, execution cap 200,000,000, EIP-7708, chain-id check on).
  - Per-transaction state resets with the transaction; a system call has its own entry point and reservoir split.
- **Spec:** §3, §4, §5, §9, §11, §14, §21. **Detail:** [`AGENTS.md`, "How Satin executes today"](../../../AGENTS.md#how-satin-executes-today).

### Gas detention (changing in #407)

- **Where:** `src/access/` (`Detention`, `VolatileDataAccess`), the volatile-read wrappers in `src/evm/instructions.rs`, `hold_precompile` and `settle_answer` in `src/evm/execution.rs`, the switch in `src/system/control.rs`.
- **Read first:** the module doc of `src/access/detention.rs`.
- **Check:**
  - The split into spendable and withheld gas is applied when a read completes, when a frame starts or resumes, and after an `SSTORE`; only regular charges are held to the spendable part, every other reader sees both.
  - The withheld part goes back to the sender unspent.
  - A crossing leaves the frame's gas as it was before the charge and stops the frame with `MegaLimitExceeded(2, limit)`; the caller gets everything the frame had.
  - A precompile is decided from its price before it runs; one outside the Satin set runs on the allowance.
  - A system-originated transaction and a system call are never detained.
  - Compute today excludes what a callee spends of a value call's 2,300 stipend and counts a halting frame's leftover through the halt-noting machinery; #407 changes both, so report a defect in the current rules only if that change would not remove it.
- **Spec:** §12, §13. **Detail:** [`AGENTS.md`, "Gas detention"](../../../AGENTS.md#gas-detention).

### Block executor and block rules (stable, except `limit.rs`, `eips.rs` and the inspector gate)

- **Where:** `src/block/` (`executor.rs`, `limit.rs`, `result.rs`, `eips.rs`, `l1_block_info.rs`, `factory.rs`, `helpers.rs`).
- **Read first:** the module doc of `src/block/executor.rs`.
- **Check:**
  - The activation-block rule is caller-set (`no_user_tx_activation_block`, default `false`); `MegaHardforks::admits_only_deposits` is what a builder computes it from.
  - A transaction is an MST when it is a legacy transaction of the system shape from the live system address, decided before it runs; a failed read of the address is an internal error.
  - A deposit and an MST are never refused by the packing budget or building policy and count towards them; the header gas limit (`BlockLimiter::pre_execution_check`) and the DA footprint (`check_da_footprint`) still apply.
  - `commit_transaction_outcome` refuses a stale outcome: the sender's nonce or balance moved since the candidate executed (`StaleOutcome`, internal).
  - Pre-block order: EIP-2935, EIP-4788, in Satin's activation block alone the check of the six system contracts and the EIP-7997 factory, the registry's due change, the read of the L1 block info; a pre-block call that does not succeed makes the block invalid, and a fatal database error is an internal error.
  - A rewriting inspector is refused at every block-execution entry, as an internal error (#407 deletes the gate).
  - The block's ledgers: regular as `max(regular, floor)`, state, history, plus history bytes, data size and write records; reported, not limited (#407 adds block limits on KV and state growth).
  - When the OP base moves, diff the file against the new base's `OpBlockExecutor`.
- **Spec:** §1, §18, §19, §20, §21.

### System contracts and the system address (stable, except keyless)

- **Where:** `src/system/` (`intercept.rs`, `oracle.rs`, `control.rs`, `limit_control.rs`, `keyless/`, `tx.rs`, `predeploys.rs`, `sequencer_registry.rs`), `crates/system-contracts/`.
- **Read first:** the module doc of `src/system/tx.rs`.
- **Check:**
  - Every transaction reads the registry's account and its `_currentSystemAddress` slot, warm, by comparing the code hash, without running the registry's code.
  - A transaction from the live system address without the system shape is invalid; a deposit from it, which never has the shape, is a failed deposit, including one carrying the MST source hash.
  - A deposit carrying the MST source hash from any **other** sender is not refused: it is system-originated, so it is held to no limit, pays no history gas, is not detained and is priced at the minimum bucket.
    The spec's basis is that no deposit derived from L1 carries the hash. This is the widest exemption in the engine, so check that claim.
  - An MST is validated (chain id, nonce, EIP-3607) before it is promoted to a deposit.
  - Satin deploys, upgrades and seeds no predeploy. The block in which Satin activates (`MegaBlockExecutionCtx::satin_activation_block`, set by the node from its schedule and the parent's timestamp) reads the seven, each a read-only entry, and each must hold exactly Satin's code whatever its nonce, balance and storage; anything else — no account, no code, an older version (the Oracle's 1.0.0 / 1.1.0, the registry's 1.0.0), other code — makes the block invalid with one error, `UnexpectedSystemContractCode`. No other block reads them.
  - An Oracle read in a system call reads chain storage, never the oracle service.
  - Keyless (changing in #407): the call runs as a native creation of the recovered signer; an empty code deposit at the deploy address is refused with `EmptyCodeDeployed`, a constructor's inner creation returning empty code is not; the answers are the documented error set.
  - The interceptor dispatch order is scheme guard, address, selector, then value policy; an unknown selector is not intercepted.
  - Deployed bytecode and code hashes match the attested artifacts (`build.rs`).
- **Spec:** §13, §15, §16, §17, §18, §19.

### Gas schedule and pricing (stable, except `schedule.rs`)

- **Where:** `src/evm/schedule.rs`, `src/evm/prices.rs`, `src/evm/history.rs`, `src/evm/precompiles.rs`, `src/external/` (SALT pricing), `state_gas_price` in `src/evm/host.rs`, the Amsterdam opcode activation in `src/evm/instructions.rs`.
- **Check:**
  - Every regular entry is Amsterdam's; state creation is paid once, in state gas.
  - State gas = schedule entry × bucket multiplier `m`; regular gas never scales.
    A failed capacity lookup is a node fault: the transaction gets no outcome and is not settled, and the node reports an internal error with the cause.
  - History gas uses the limit layer's byte table, so a record's history bytes are its data size; the three sites where the totals part are in the `src/limit/mod.rs` module doc, "The byte table".
  - A deposit, a protocol-produced transaction and a system call pay no history gas.
  - The history allowance (160 bytes, `storage_call_stipend()` in `src/evm/history.rs`) draws only on a log's charge.
  - The EIP-7623 floor is still computed and validated.
  - The constants are provisional; a build with `satin-price-override` must never reach a node.
- **Spec:** §2, §4, §5, §6, §7, §8, §9.

### Inspector contract (deleted in #407)

- **Where:** `src/evm/inspector.rs`, and the admission checks in `src/evm/mod.rs`, `src/block/factory.rs`, `src/block/executor.rs`.
- **Check:**
  - The module documentation of `src/evm/inspector.rs` is the contract (the spec does not describe inspectors); check that each rewrite it permits is settled as it says.
  - An answered frame and a revived call settle as a frame that kept nothing: state and history gas rolled back, refund dropped, lane discarded, the upfront charge given back at the price it was made at.
  - A success rewritten into a failure has its journal taken back to the frame's checkpoint.
  - A pre-block system call runs on the plain system-call path, with no callback.
  - A declared observer must not answer a frame, edit inputs or rewrite a result (checked in debug builds).

### Node surface, validation and the stateless witness (stable, except genesis and validation)

- **Where:** `src/lib.rs` (explicit exports), `src/block/hardfork.rs`, `src/block/chain.rs`, `src/block/genesis.rs`, `src/evm/validation.rs`, `src/evm/state.rs`, `src/block/l1_block_info.rs`, `src/test_utils/witness.rs`, `crates/mega-state-test/src/witness.rs`.
- **Check:**
  - `validate_schedule` refuses a schedule in which a base fork Satin runs on is not active by `satinTime`.
  - The genesis format has exactly one key, `satinTime`; Satin carries no parameter, and every other `satin*` key (the old registry and limit keys included) is refused.
  - The pool and the engine refuse the same oversize body with the same error (`TX_BODY_OVER_DATA_SIZE_LIMIT`): body + 40 × authorizations over the transaction limit.
  - The witness holds every read: block hashes, SALT bucket ids, the Oracle reads of the block's transactions (none for system calls), the L1 block info, the registry.
  - The witness is the sequence of states the block commits: each pre-block step commits its state on its own, read-only entries included, where a commit hook on `State` receives it whole; nothing else hands the pre-block states out.
- **Spec:** §1, §21, §22, "Node integrators".

### Tools, tests, goldens, benches and CI (stable, except the outcome tooling)

- **Where:** `bin/mega-evme/` (`bin/mega-evme/src/engine.rs` picks the engine from `--spec`: Satin in-tree, legacy specs on a released crate; `run` and `tx` default to the legacy `Rex6`), `crates/mega-state-test/`, `crates/state-test/`, `crates/mega-evm/tests/` (`satin/`, `block/`, `system/`, `fuzz/`, `tracers/`), `crates/mega-evm/benches/`, `.github/workflows/`, `scripts/`, `mutants/suppressions.toml`.
- **Check:**
  - No consensus path changed for a tool's sake; tool overrides (`--override.limits`, `with_tx_runtime_limits`, `with_protocol_limits` under `test-utils`) are explicit and never default.
  - A test pins a spec rule, not the implementation; a new or changed test says which rule it checks (some headers carry a `[S<section>.<n>]` tag: the spec section and item).
  - A figure the schedule determines is asserted against its derivation, not pinned; snapshots and goldens own only outputs with no computable expectation.
  - The suite passes at the byte-price grid's points, not only at the spec's prices.
  - The mutation gate is fail-closed: a suppression without a locator may cover only one mutant.
  - A fuzz run that checks nothing fails (zero cases, zero comparisons).

## Judging a finding

1. **Reachability first.** Does the node's block execution or building path reach it? With the default configuration, or only with a non-default limit? Is it decided below, or does #407 change it ("What is still moving")?
2. **Severity**, in this order:
   1. not reachable from block execution or building: at most low (tools, test APIs);
   2. reachable only with a non-default configuration: one level lower;
   3. already decided below: not a new finding; say what is new about it;
   4. only an error kind or text differs, with no effect on state root, receipts, gas used, logs or block validity: not a consensus defect;
   5. can halt the chain (an activation-block error, a pre-block failure, a valid deposit refused): high, whatever the likelihood.
3. **A claim in the pull request description**: turn it into a checklist item and verify it with `file:line` against the final code.
4. **A mechanism** is open to question unless the Satin design settled it: ask what problem it solves, whether that problem still exists, whether a native replacement exists, and how it could be simpler.
5. A consensus defect is a difference in state root, receipts root, gas used, logs or block validity between two honest nodes, or between the engine and its specification.

## Revert or halt

Read this before rating a finding that says a failure reverts where it should halt, or halts where it should revert.

A refusal the protocol imposes reverts; an execution fault halts.
Every limit stop — the transaction's data-size limit, a frame's data-size budget, gas detention's compute limit — reverts the frame that crosses it with `MegaLimitExceeded(uint8 kind, uint64 limit)` on the frame's own gas, so the unspent gas goes back to the caller, which resumes and may catch the stop.
An execution fault — a real out-of-gas, a precompile given less than its price, an invalid opcode, a charge the frame cannot pay — halts and consumes the frame's gas, as in revm.
One ABI serves every kind of stop, and there is no third ending: the halt reasons are op-revm's (`MegaHaltReason`), and the engine makes no halt of its own.

The KV and state-growth stops, and naming the frame budget rather than the transaction limit when a write crosses both, are still to land in #407; on this branch such a write names the transaction limit (`AdditionalLimit::check`).

The deliberate exceptions:

| What happens                                                                                                                                                                                                                                                                                                                                                                                                        | Why                                                                                                                                                                                                                                                                                                               |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A deposit that fails validation — its body over the data-size limit, or sent from the live system address — or that cannot pay a charge made before its first frame, or whose execution halts, is included as a failed deposit: its whole gas limit billed, its nonce bumped, its mint kept, nothing else applied, no stop reported.                                                                                | The OP stack's deposit rule: a deposit comes from L1, where its gas was paid, and must be included, so it can be neither rejected nor caught by a caller.                                                                                                                                                         |
| An input over one of Optimism's precompile input-size limits (the BN254 pairing, the BLS12-381 MSMs and pairing) is refused before any gas check: the call halts and burns its forward.                                                                                                                                                                                                                             | The Optimism precompile specification, inherited with the Karst set: the size check is the precompile's own input check, which fails the call like any bad input. Gas detention prices such an input at zero, so it is never a crossing.                                                                          |
| Under gas detention two precompile faults end as the compute stop instead of a halt: a priced precompile whose price is past the allowance and within the forward is not run, even when its input would fail a check made after the gas check; and a precompile the engine cannot price (one a node added or replaced) that runs out of the allowance is the stop, even when its price is past the whole forward.   | revm runs a precompile inside the frame's start, so the engine must decide before it runs, and it decides from the price; a check made after the gas check is not visible to it. A precompile without a price can only be run on the allowance, and running out of it is all the engine sees.                     |
| A call whose start (its value transfer's records and transfer log) crosses the data-size limit gets the stop on its untouched forward, even when the callee is a precompile that would have failed and burned the forward.                                                                                                                                                                                          | A limit is enforced at the writes it guards, and a start writes before its callee runs. Here the start is counted before revm runs the callee, which then never runs; #407 counts it after revm answered and replaces a failed answer with the stop, to the same end.                                             |
| A frame answered without running (an interceptor's answer, or a `keylessDeploy` call refused for carrying value) that spent more regular gas than the frame's allowance is the compute stop, and none of its spending is billed, the part within the allowance included, where a frame that ran keeps what it spent before the charge that crossed.                                                                 | An answer is one step with no point inside it where the allowance ran out, so its whole spending is the charge that crossed, and a charge that crosses is never made. Of the engine's own answers only the `keylessDeploy` refusal spends regular gas, its overhead.                                              |
| Refusals that are not limit stops revert with their own error, not `MegaLimitExceeded`: `VolatileDataAccessDisabled(uint8)` (the opcode's static gas paid, nothing else), `DisabledByParent()`, `NonZeroTransfer()`, `NotIntercepted()`, keyless deployment's rule errors, and revm's `CallTooDeep` and `OutOfFunds` with empty data. All are catchable, and the outcome's `limit_exceeded` stays empty.            | They refuse what the caller asked for, not a resource the transaction ran out of: each names its reason in the ABI its contract, keyless deployment or revm already defines, so a caller can tell it from a stop.                                                                                                 |
| One size rule has three endings: initcode over the size limit is rejected at validation in a creation transaction, halts the creating frame at `CREATE` or `CREATE2`, and reverts a `keylessDeploy` call with `InitCodeTooLarge(size, max)`. Deployed code over the code-size limit halts the creation, while deployed code within it that takes the transaction or the frame over the data-size limit is the stop. | The size halts are Ethereum's (EIP-3860 for initcode, EIP-170 for deployed code) at MegaETH's sizes: rules of the EVM, not MegaETH's resource limits, so they stay halts. A keyless call checks the init code among its rules before any creation starts, and every rule refuses with a revert and its own error. |

Not exceptions, but easy to mistake for one:

- A transaction that cannot pay a charge made before its first frame — the account a deposit-like transaction creates for its caller, an EIP-7702 authorization's charges, the history of the records made before the first frame — ends out of gas with no frame: its whole regular budget is consumed and its reservoir returned.
  That is EIP-2780's runtime out-of-gas path, a gas fault (`pre_execution` in `src/evm/execution.rs`).
- A keyless creation that reverts, halts or is stopped makes the `keylessDeploy` call succeed, with `ExecutionReverted`, `ExecutionHalted` or `EmptyCodeDeployed` in its ABI return, a stop's revert data carried inside `ExecutionReverted`.
  The call itself reverts with the stop only when its own charges or the signer's nonce record cross a limit (`answer` in `src/system/keyless/settle.rs`).
- Under gas detention, only a regular charge the withheld part would have paid is a crossing; a charge beyond the frame's whole regular gas, and a failed state or history charge, halt as they would without the read.

## Already decided — do not re-report

Each is a decision with its reason; "in this pull request" means review the code, not the decision.
An item marked (#407) is one #407 revisits; until it merges, the item describes this branch.

- (#407) **KV, state-gas and block execution-gas limits are removed**: data size bounds the records and history gas prices them, state gas prices state, so removal changes no result.
- **Consensus limits are one per-spec table** (`ProtocolLimits::for_spec`), not chain configuration: configured values could stop the L1-attributes deposit and cost a silent factory fallback.
- **No latch**: a crossing is a native revert of the crossing frame, which callers may catch; with data size and detention as the only limits no transaction ends over a limit.
- **An oversize transaction body is refused at validation** (a deposit becomes a failed deposit): the body is known before execution, and at Satin's constants the bound is unreachable (6,732,889 < 13,107,200).
- **`tx_chain_id_check` is pinned on**: it is revm 40's default, and it makes the system-transaction chain-id guard effective.
- **Every transaction pre-warms the registry read**, so gas does not depend on the envelope type; the non-warming journal read is not a generic upstream candidate.
- **A delayed commit re-checks only the sender's nonce and balance**: other staleness is the builder's documented responsibility, as the system-transaction-first flow requires.
- **An MST is never refused by a packing budget or builder policy**: it serves a user transaction already admitted.
- **Satin has no deploy or upgrade path**: it runs on a chain that ran the legacy forks, and its activation block refuses any predeploy without Satin's code; no contract changes.
- **Keyless refuses only an empty code deposit** at the deploy address; the four producer-less errors are deleted, which changes no deployed byte.
- (#407) **Creation revival by an inspector is allowed** (Foundry parity), settled as a frame that kept nothing; the upfront refund is given back at the price it was charged at.
- **The Amsterdam regular gas table is taken wholesale**: the earlier press-back to Osaka priced creation twice.
- **`validate_schedule` checks that base forks are active by `satinTime`**; engine-built schedules activate them from the first block, which no Satin block can tell apart. An unknown chain has no schedule (`hardfork_schedule` answers `None`).
- (#407) **A rewriting inspector in block execution is an internal error**, not a block verdict: the inspector is the node's own choice.
- **Explicit crate-root exports; dead helpers and legacy-era remnants are deleted.**
- **Satin carries no fork parameter**, and the fork-parameter mechanism was removed with nothing using it.
- **An absurd state-gas price makes the operation run out of gas**; no reachable divergence.
- **The pending-changes due check uses the direct-read convention** (any database error is an internal error), as the other direct reads and upstream do.
- **Precompile prices are looked up by id**: a precompile set replaced around the engine, through the mutable accessors, keeps the built-in prices; the spec states the boundary, and nodes change the set through `with_dyn_precompiles` or the factory builder.
- **A keyless signer that is the block beneficiary is detained**, as in the legacy engine.
- **No MST in the activation block**: it admits only deposits and every MST consumer is a non-deposit; documented in the spec.
- (#407) **Write records are deduplicated per frame**, not per transaction: documented as an upper bound.
- **The limit override is an explicit tool opt-out** (`with_tx_runtime_limits`); block execution and validation use the chain's limits.
- **No spec ladder in Satin yet**: it comes with the next spec, with the extension points kept; `AGENTS.md` says how to add it.
- **The fork finalizes the journal on an error** (`bluealloy/revm#3780`, carried early): kept; the fork documents it in megaeth-labs/revm#75.
- (#407) **Answered-frame settlement stays in mega-evm**, and `OpBlockExecutor` is not composed (it requires `PostExecEvm` and returns `OpTxResult`).
- **The 98/100 frame budget and dropping a hint from a frame whose volatile access is off are kept**: without a latch the budget protects the caller's own work.
- (#407) **An inspector answer with effects is mis-accounted** (a mock that transfers value, an isolated run): the settlement assumes none; tools only, kept.
- **Mechanisms kept as in the legacy engine** (not under review): `MegaAccessControl`, gas detention's two categories and 20M caps, `remainingComputeGas()`, the Oracle, keyless deployment, the Mega System Transaction, the registry and rotation, the high-precision timestamp, SALT bucket pricing, the history allowance, the data-size limit's 98/100 share, the 200M execution cap with 512 KiB code size and the KZG price, the deposit-caller account charge, the DA footprint, and the inspector admission gate (#407 deletes the gate).
- **The upstream revm change `new_answer`** for answers to frames that did not run (`bluealloy/revm#3979`) is opt-in: the handler does not normalise answers, because real inspectors' answers can have effects.
