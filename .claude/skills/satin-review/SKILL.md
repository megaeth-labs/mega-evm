---
name: satin-review
description: Guide for reviewing the Satin engine pull request (#386) in megaeth-labs/mega-evm, which holds the whole engine now that the stacked pull requests and #407 have merged into it. Use when reviewing #386 or the Satin engine, or when the user says "review satin". Gives the scope and the review range, the design changes #407 and the commits after it made to what the stacked pull requests describe, the module map with the invariants to check, the reading order, how to rate a finding, the revert-or-halt rule with its deliberate exceptions, and the decisions already made. Temporary: removed before the Satin pull request merges.
---

# Reviewing the Satin engine (pull request #386)

This guide is temporary.
It is deleted before the Satin pull request merges, so nothing else may link to it.
It is a map: where each part of the engine lives, what to check there, what changed after the stacked pull requests were described, and what is already decided.
Implementation detail lives in [`AGENTS.md`](../../../AGENTS.md) and the specification in [`docs/spec/upgrades/satin.md`](../../../docs/spec/upgrades/satin.md); this guide links them, and restates a rule only to say what changed.
[`REVIEW.md`](../../../REVIEW.md) is the repository's own review rubric; its consensus-critical list overlaps the checks below.
The guide describes the code at `cz/feat/satin-w2` with #407 merged into it and the commits after it.

## What is under review

- The Satin engine, in pull request #386 (`cz/feat/satin-w2`), judged at its final state.
  The stacked pull requests #387, #388, #390, #399 and #400 (with #404, merged into #400's branch) merged into it, and then #407 (`cz/feat/satin-w9`); each description explains its part as it was merged, and a later part may have changed an earlier one.
  The commits after #407 on `cz/feat/satin-w2` are part of this pull request and have no description of their own.
  "Design changes a reviewer must know" below lists what #407 and those commits changed, a change those commits made marked as such.
- The review range is the pull request's own diff.
  Its base, `cz/feat/satin-review-base`, is `satin` before the Satin skeleton: `a8f8c7c9`, the CI-only commit before it.
  So GitHub's "Files changed" holds the skeleton (#385) and every later series: the whole engine.
  The local equivalent is `git diff a8f8c7c9...<head>`, `<head>` being the head of #386.
  Most of its deletions are the legacy engine, its tests and its tools (184 files): skim them, or hide them with `--diff-filter=d`.
- #386 targets the review base only while it is reviewed.
  It is retargeted to `satin` before it merges, and it must never be merged into the review base.
- Judge the final code.
  A defect an earlier commit introduced and a later one fixed is not a defect; history only explains why something is as it is.
  A test deleted or parked in an intermediate commit is not a finding either: test completeness is judged on the whole.
- Satin is the first spec of a line, so keep the extension points a line needs (spec-keyed constants, a spec argument); a later fork that first needs configuration parameters adds a mechanism for them then.
  Satin is a new spec: simpler semantics win over parity with the legacy engine.
- Out of scope: the legacy engine (`main`, frozen), which runs every block before Satin activates.
  Satin never executes or decodes legacy-era data, so "Satin cannot handle legacy data" is not a defect, and code kept only for that purpose is dead code.
- Fork code: the `megaeth-labs/revm` fork at `4c8f0d43b70f2175f9ce0132cc343283f9a29548` and the `megaeth-labs/op-revm` fork at `8c0db939291e1c4cbf2375782d1483bc371d6b94`, which the patch blocks pin by `rev`, are pinned dependencies with their own review: megaeth-labs/revm#76, stacked on megaeth-labs/revm#75, and megaeth-labs/op-revm#8.
  Their diff is not part of this pull request.
  A finding about a fork function is about how mega-evm uses it, unless it shows the fork breaking its own documented default behaviour.
  The fork functions the engine relies on are listed in [`AGENTS.md`, "Dependencies on the forks"](../../../AGENTS.md#dependencies-on-the-forks).
- Where code belongs: upstream revm first, then the fork, then mega-evm.
  The fork is only for data from the middle of revm's own flow that mega-evm cannot reach from outside; what can be done at a boundary mega-evm owns (frame start or end, the Host, the handler, the inspect path) belongs in mega-evm.
  For each mechanism, ask whether it sits in the cheapest layer.
- The OP base may move (Karst to a later OP fork) before launch; a finding that depends on Karst specifics should say so.

## What is still moving

Nothing.
#407 has merged into this branch, and with it everything its description listed as still to be added: inspectors on revm's own inspected lifecycle, a rewritten result taken as given, one fixed gas budget for the pre-block calls, the cleanups, and the KV-update and state-growth limits with their 98/100 frame shares and the destruction table's give-backs.
It also counts a creator's nonce record when the creation returns, which its description did not list.
The commits after it on this branch count a frame start's records from what revm did; they are part of this pull request, and the design changes below mark what they changed.
Every module is at its final state for this review: there is no longer a table of stable and changing modules, nor a rule to hold back findings in a changing one.
What follows this pull request changes no rule.
The fork pins move from the reviewed commits to the merged commits of megaeth-labs/revm#76 and megaeth-labs/op-revm#8, and to release tags once the line is final.
Performance work is left for later: the optimisations #407 recorded are made together once the structural work is done, measured against CodSpeed's report.

## Design changes a reviewer must know

The stacked pull requests' descriptions predate #407 and the commits after it on this branch, which changed parts of what they describe.
A change made after #407 merged is marked as such: it is not in #407's diff.
Each change is stated here in plain words with its reason; the specification has the rule and `AGENTS.md` the mechanism.

### Where the engine meets revm

- **Inspectors run on revm's own inspected lifecycle, and the engine has no separate inspected lifecycle.**
  The engine's inspected copies of the frame lifecycle, its step guard and `src/evm/inspector.rs` are gone: every per-frame mechanism sits in the frame start, run result and return (`frame_init`, `frame_run_result`, `frame_return_result`) that revm's plain and inspected loops share.
  A result an inspector rewrites is settled by the caller as revm settles it, with no correction of the engine's, and an answer it gives in place of a frame never reaches the engine's frame start, so the depth guard, the interceptors and gas detention see none of it.
  What the engine decides for an inspector is the start ledger's settlement of an inspector's answer, a replaced tracker and a rebuilt outcome, read off revm's flags.
  Block execution admits any inspector.
  Why: a second copy of the lifecycle had to be kept in step with revm's and made an inspected run differ from a plain one, and which inspectors run on block production is the node's choice.
- **The fork's loop hooks.**
  revm's frame run returns an action set on a frame before it runs, without running an instruction or calling the inspector's `step`, and its inspected frame start hands a frame built with one to no `initialize_interp`: a `keylessDeploy` call's creation and answer, and the stop of a caller a child left over a limit, come back through revm's own run.
  The run-result hook (`EvmTr::frame_run_result`) is called once a frame's run returns, before the inspector's `call_end`, `create_end` and `frame_end` and before `frame_return_result`; gas detention turns a crossing into the stop there, so inspectors and tracers see the stop the caller gets.
  Why: both are points in the middle of revm's own loop that mega-evm cannot reach from outside, which is what the fork is for, and with them the engine has no frame run of its own.
- **The engine decides the four figures of a frame start, and the fork carries none.**
  The four figures of a start are the three budgets with their nets, the part of the reservoir the caller keeps, the start charge and the history allowance.
  The engine decides all four itself, in every build: the budgets, the start charge and the allowance on its start ledger (`src/limit/ledger.rs`), an entry per start driven only by the fork's start lifecycle hooks (`frame_split`, `frame_settle`, `frame_lifecycle_end`), and the reservoir's share where the start is made (`MegaContext::forward_start`, named to revm on `FrameStart::reservoir`).
  The fork forwards and settles the reservoir as upstream does: the settlement puts the reservoir the child hands back in place of the caller's, and `frame_settle` adds back the part the caller kept (`FrameSettle::held_reservoir`) at a child's settlement, not at the transaction's, whose gas held a copy of its frame's reservoir.
  A limit check reads the running frame's budget off the ledger's top entry; `frame_settle` gives a failed start's charge back to the caller's gas, as history, and after a halt of the transaction's own frame burns the transaction's regular gas, as revm's settlement did after its own refund, so regular gas an inspector's extra reservoir unwound is not kept; a log's history draws the entry's allowance first; the frame cap is the ledger's (`cap_child`, which replaced `cap_the_shares` and is held to it by a unit test).
  The inspector shapes whose results changed when the engine took the figures over, each accepted: an answer built on gas whose pools the inspector set, or with the input's start charge copied, which an inspector can no longer build, now that the fork carries neither; a tracker an inspector restores from a snapshot leaves the ledger's draws and the allowance where they were; and an inspector that replaces a value call's interpreter leaves the frame its allowance, where the fresh gas of the replaced interpreter held none while the allowance was on the frame's gas.
  A test reads what the ledger settled (`test_utils::record_start_ledger_settlements`).
  Why: the figures are the engine's decisions, and the fork carries none of them.
- **Writes and frame starts are metered in the fork's instruction hooks.**
  `SSTORE`, `LOG0`–`LOG4` and `SELFDESTRUCT` call `write_completed` once the write is in the journal and every charge the opcode makes is made.
  The four calls and the two creations call `frame_starting` once the forward is taken: the caller is charged the history of the start's records out of what it kept, the frame's own part is parked on the start ledger as the start's charge, which its settlement gives back when the frame fails, and the frame's share of the caller's reservoir is taken and named on the start (`FrameStart::reservoir`).
  Why: a write the opcode's own failure takes back is never counted, and the engine no longer rebuilds revm's state from outside with instruction wrappers.
- **An error leaves nothing the next call depends on.**
  Made after #407 merged: the handler's fatal cleanup (`MegaHandler::catch_error`), which every entry point reaches, empties the error slot before op-revm's cleanup runs, discards the failed call's journal transaction on every error, and clears the cached L1 cost, the local context and the frame stack after op-revm; `replay`, `SystemCallEvm::system_call_with_caller` (with the deprecated aliases and alloy-evm's `transact_system_call` that route through it) and the pre-block calls finalize the journal on an error as they do on a success.
  Before, an error a Host load recorded in the transaction's own frame, which post-execution preempted by failing first on a read of its own, stayed in the slot and failed the next transaction on the same EVM; and a `_one` entry point, `replay`, a system call returning state and a pre-block call left the failed call's writes, nonce bump, warm set and transaction id in the journal for the next call.
  A failed deposit is a result: its nonce bump and mint stay; the calls a `_one` journal holds before the failed one stay too.
  An embedder reuses an EVM after an error as after a success; a caller of `run_without_catch_error` that drops an error calls `catch_error` first, and an EVM a panic unwound out of is not reused.
  Why: op-revm discards the journal only for a failed deposit, revm takes the recorded error only once post-execution succeeded, and op-revm's own clearing is skipped when its failed-deposit recovery errors; the boundary every entry point reaches is the engine's, so the repair is made there and the forks are unchanged.

### What is counted

- **Data size, KV updates and state growth are pools on each frame start, settled with revm's start lifecycle.**
  Each is a pool on the frame's entry of the engine's start ledger, which a child's start takes its share of where it is made and which merges into the caller's when revm settles the start, as revm settles the EIP-8037 reservoir; the transaction's limits are held beside the pools, on what the transaction keeps.
  The engine's per-frame record lanes and its journal wrapper are gone, and the context's journal is revm's own.
  Why: a frame's budget then fails and succeeds with the frame's start, paired by revm, instead of a second account kept beside the journal.
- **Write records follow the net change.**
  An account's record exists while its nonce, balance or code differs from what the transaction loaded, and a slot's while its value differs from its original.
  The per-frame deduplication is gone, a `SELFDESTRUCT` records the account it clears as well as its beneficiary, and a creation is charged exactly the records it makes.
  Why: the record measures the state the transaction keeps, by revm's own notion of a change; the per-frame count over-counted, and charged history gas for writes that leave nothing.
- **A frame start is held to the limits once revm decided it, on what it kept.**
  Made after #407 merged: #407 already held a start once revm decided it — a built frame stopped before its first instruction, an answer stopped on its untouched forward, a collision not stopped for its start — but to records it predicted from the journal before revm decided, which it held the two answers below to as well; the commits after it count what revm did, and hold a failed answer to no records limit and a detention-priced answer to its price alone.
  Its accounts on the frame are read before revm decides the start and again once it did, and what changed between — each account's record, an account that was empty and is not, the transfer log revm journaled — is counted on the started frame's pools; nothing is predicted.
  A frame revm built returns the stop before its first instruction, and a success revm gave without building a frame gives way to the stop on the untouched forward, its value move taken back.
  An answer that failed kept nothing, revm having taken back what its start moved, and is held to nothing: a value call to a precompile that fails at a budget boundary ends as the precompile's failure, which burns its forward, not as a limit's stop.
  A precompile gas detention answers from its price before revm runs it moved nothing and is held to the price alone: its stop is kind 2, whatever records its value transfer would have made.
  A creation onto an occupied address fails on the collision, and is not stopped for what its start would count.
  Why: the limit judges what the journal kept, by the same rule at every start, instead of a prediction of revm's start held to the journal only in debug builds and checked hypothetically for the two answers that leave nothing to read; the opcode's history charge is still priced from the journal's state before revm decides, which on the plain path is exact because the opcode makes revm's refusal checks first, and the start's credit settles a price that differs from what revm made.
- **A call that moves no value is not read at its start.**
  Made after #407 merged: a call of any scheme that moves no value and whose code address is not a precompile of the EVM's set skips the reads before and after revm decides its start (`start_writes_nothing` in `src/evm/execution.rs`), and the opcode charges a call that transfers no value nothing without reading the journal (`start_makes_no_records` in `src/limit/record.rs`); such a start is still opened and held to the transaction's limits.
  A debug build makes the skipped reads and asserts they find nothing, and each decision has a truth-table test.
  A `CALLCODE` moving value and any call to a precompile stay read: a node's own precompile runs inside the start and may write the start's accounts there (`tests/satin/tool_writes.rs`).
  Why: revm's start of such a call only loads and touches its target, which changes no nonce, balance or code and journals no log, and the reads cost about 900 instructions per start, measured, on every contract call and every internal call that moves nothing.
- **A creator's nonce record is counted on the creator when the creation returns with the nonce bumped.**
  The creation's budget is the share taken at the opcode, before that record, which is Rex6's order.
  The record is held to the creator's budget, not the creation's, and survives the creation's failure; a keyless signer's is counted on the call at its settlement, and the keyless creation's share is of the call's whole pools, which the call draws nothing from before it.
  The frame-start hook only prices: it charges the start's history and counts nothing (`hold_creator` in `src/evm/execution.rs` counts the record).
  A start an inspector rewrites after its opcode is not charged again: what the opcode charged stands, its declared part coming back only as for any start whose frame keeps nothing, and its records are those revm then made; the specification says nothing of inspectors, and `AGENTS.md` states this.
  Why: the bump is a fact only once revm made it, and the creator's pools are in reach again only at the return, so nothing is counted before revm decides and nothing has to be given back.
- **A frame a failure below it left over the transaction's limit is stopped as it resumes.**
  A failed frame's counts go back to what the frames kept when it started, a record of theirs it took back included, while an Oracle hint it admitted stays counted; a hint sent into the room such a take-back freed leaves the transaction over its data-size limit, and the frame it returns into returns the transaction's stop before its next instruction (`AdditionalLimit::hold_caller`), whether the frame below reverted, halted or was itself stopped.
  Why: without it a caller that catches the failure could end the transaction above its limit.
- **KV updates and state growth are limited again**, per transaction (`TX_KV_UPDATE_LIMIT` = 500,000 write records, `TX_STATE_GROWTH_LIMIT` = 1,000 new accounts and slots), per frame (98/100 of what the parent has left) and per block (`BLOCK_KV_UPDATE_LIMIT` = 500,000, `BLOCK_STATE_GROWTH_LIMIT` = 1,000, packing budgets as the block's data size is).
  The KV count is the write-record count.
  State growth rises by one where EIP-8037 charges the state gas of one new account or slot, and for the caller a failed deposit creates, which pays no state gas; it falls when a slot whose original is zero is written back to zero; a frame's net is signed, and a negative net never enlarges a child's budget.
  An EIP-7702 list that would cross either limit is skipped whole, and the transaction's own frame is stopped before it runs.
  Why: these are Rex6's limits, and gas does not stand in for the count: 1,000 fresh slots cost 97,920,000 state gas, which a gas limit can cover; at Satin's constants data size binds before the KV count, and the KV check is still made.
- **The destruction table.**
  The first effective `SELFDESTRUCT` of an account created in the same transaction gives back what the transaction made of it: each part absent at the transaction's start and present before the destruction — the account leaf, its deployed code, each slot written non-zero — is refilled at the state gas it was charged, to the destroying frame; the leaf and each such slot lower state growth by one and have their write records and those records' history taken back, and the code's data size and history gas are taken back.
  A slot's value at the start is its original in the transaction, so a slot of the created account first accessed after the creation is zero there whatever the state held; a leaf is absent when no account stood at the address: the database answered none (`None`, not an empty account), or an earlier transaction on the same journal destroyed it (`Accounts::was_absent_at_start` in `src/limit/record.rs`).
  A part present at the start is an ordinary deletion; a failure of the destroying frame, or of a frame that contains it, discards all of it.
  From the mark the account adds no further state growth or record, and every state charge on it is priced at nothing (`Host::state_gas_price`), so a later `SSTORE` or a deposit of its constructor's code is never charged state gas rather than charged and refilled; the table's own give-back prices outside that rule (`MegaContext::priced_state_gas`).
  The neutral configuration counts the table and prices state gas as the fixture's fork does, which gives none back, so equivalence mode's transactions that create and destroy an account report other usage than before the table, and nothing else differs.
  The op-revm baseline pins the divergence (`test_a_creation_that_destroys_itself_diverges_from_op_revm`).
  Why: a part the transaction made does not remain for a node to store, so the count and the charge stay aligned; EIP-8037 refills nothing for this destruction and Rex6 refunded no storage gas, so this departs from both on purpose.
- **A fact about a transaction's start is read off that transaction's journal.**
  revm's account flags of a database load (`LoadedAsNotExisting`), of a touch and of a destruction are set once per journal and outlive the transaction that set them when a journal runs several transactions without finalizing; the engine reads such a fact with what the journal records of the running transaction — the original info, the transaction id of what it loaded, its entries (`JournalEntry::AccountDestroyed`'s status, `AccountTouched`) and the local created and destroyed flags — and never off such a flag alone.
  So what the engine decides on such a journal follows what revm charges there, as it does on a fresh one: the destruction table's leaf, the keyless empty-deposit rule and its answer (`Accounts::is_destroyed`), and the authorities an EIP-7702 list created (`applied_authorities` in `src/evm/execution.rs`, which counts one where revm charged it a new account).
  Why: a node's block executor finalizes each transaction, but `transact_one` on the public API does not, and a count must not part from its charge on either path.

### Gas

- **The 98/100 forwards.**
  A call or a creation forwards at most `remaining − remaining × 2 / 100` of the caller's regular gas, in place of EIP-150's 63/64 (the schedule's `call_stipend_reduction`, 50), and a value call's 2,300 stipend is added after the cap.
  Every child receives `remaining − remaining × 2 / 100` of the caller's reservoir, whatever regular gas the call asks for; the transaction's own frame and a system call's outermost frame take the whole reservoir, and a keyless deployment's creation forwards `min(gasLimitOverride, remaining)` of regular gas with the reservoir share.
  The three pools give a child `remaining × 98 / 100`, rounded down, so the two formulas differ at some values.
  Why: the regular forward is Rex6's, and the reservoir share keeps a caller's own work within reach of what it calls, since state and history gas draw the reservoir first; EIP-8037 passes the reservoir whole, which would let a callee spend all of it.
- **The history allowance replaces the storage-gas stipend.**
  A value `CALL` or `CALLCODE` below the transaction's own frame gives the frame it starts 160 bytes of history (`HISTORY_ALLOWANCE_BYTES`, 14,080 gas at the cost per history byte, kept on the start's entry of the start ledger), on which only a log's history charge draws.
  It never enters the frame's gas and is on no ledger.
  Why: a `receive()` reached through Solidity's `transfer()` can still emit one event, and an allowance that is not gas leaves nothing to burn on return.
- **One fixed gas budget for the pre-block calls.**
  Each runs on `PRE_BLOCK_CALL_GAS_LIMIT` = 31,566,720, whatever the block's gas limit: 30,000,000 of regular gas and a reservoir of sixteen fresh slots at the minimum bucket.
  Why: a pre-block call does not draw on the block's gas; Rex6's `max(block gas limit, 30,000,000)` was widened for storage gas a crowded bucket multiplied, which a system call no longer pays; a pre-block call on MegaETH's chains writes at most six fresh slots, and none of the three contracts reads `GAS`.

- **The first frame's record is priced once.**
  Made after #407 merged: the history gas pre-execution charges for the write record the transaction's own frame makes is kept on the layer (`AdditionalLimit::keep_first_frame_charge`) and taken by the frame's input as its start charge, where it used to be priced a second time on the same journal.
  A debug build prices it again at the input and asserts the two agree; a system call, which skips pre-execution, is priced at its input as before.
  Why: nothing between the charge and the input changes the journal it is priced on, and the second pricing cost about 550 instructions per value-transfer transaction, measured.

### Gas detention

- **Compute counts the work run on a value call's stipend, and never what a halting frame burns.**
  The stipend is not forwarded gas, so what a callee runs on it is compute, and what it leaves of it does not lower compute when it comes back.
  What a halting frame burned is read off the fork's failed-charge record and is not compute, so no halt takes a detained transaction past its limit.
  Why: the old figure let a callee spend the stipend uncounted after a read, and counting the gas a halting frame had left overshot the limit.
- **A value call's own charges come first.**
  Its static 100, its memory, the 11,300 value transfer and a cold access are decided as any opcode's charges are, and one that crosses the limit stops the calling frame before anything moves, whether or not the caller can fund the value; only then is a precompile decided from its price.
  Why: an opcode's own charges are held where they are made, and the outcome must not depend on whether the value could be funded; the specification now says so where it states the precompile exception.
- **The volatile-data rules.**
  `BLOBHASH` is not volatile: it reads the transaction's own blob hashes.
  `SLOTNUM` is a read of the block environment, and its refusal carries access type 12, which the contract's enum does not declare, so a handler decodes the argument as a `uint8`.
  A call to the EIP-2935 history contract is a read of the block environment, through the `NUMBER` its `get` path executes before it loads the slot; an `SLOAD` of the storage the EIP-2935 and EIP-4788 pre-block calls wrote is state the block starts with, and is no read.
  A `CREATE` or `CREATE2` is neither marked nor refused for loading its creator or created address, the block beneficiary included.
  Why: volatile data is data the block, the sequencer or the oracle changes under the transaction; the blob hashes are the transaction's own, the history contract's answer depends on the block number it reads, the pre-block storage is fixed before the first transaction, and a creation's loads are the creation opcode's own checks under EIP-8037, as a value call's check of its caller's balance is.

### The stop and the configuration

- **Limit stops are revert-class in four dimensions, in a fixed order.**
  A crossing of data size (kind 0), KV updates (kind 1), state growth (kind 3) or gas detention's compute limit (kind 2) reverts the crossing frame with `MegaLimitExceeded(kind, limit)`, and its caller may catch it.
  An operation that crosses several counted dimensions at one site stops on the first of data size, KV updates, state growth; compute is decided at the regular charge, before the write is counted.
  Inside a dimension the frame budget is tested first, and the stop names the budget whenever the budget is crossed, the transaction limit only when it is not.
  Why: the order across dimensions and the discriminants are Rex6's, so a Rex6 decoder reads the stop, and a frame that crosses its budget is told the budget it crossed.
- **Limits files of the first shape still run.**
  A limits file written before the KV-update and state-growth limits returned, such as one given to `mega-evme --override.limits`, reads with the default spec's values for the four newer fields; every field that shape had stays required, and an unknown field is refused.
  Why: a tool's existing files keep working; consensus never reads such a file.
- **Smaller changes of the node surface.**
  A Satin key given twice in a genesis `config` object is read as its last copy by every reader, because a node and the reference CLI parse the file into a JSON value that keeps only the last copy.
  `satinTime` is also read from `{"$serde_json::private::Number": "<digits>"}`, decimal digits alone, the form `serde_json` hands a number over in with `arbitrary_precision` on.
  A witness replay's exports must be a subset of the building node's and may be a proper one, because the building node's also hold what candidates it executed and dropped asked for.
  Satin's configuration fixes off the fork's switch back to revm 40.0.3's journal semantics, which exists for the legacy engine.
  The handler splits a system call's gas itself, at most `SYSTEM_CALL_REGULAR_GAS_LIMIT` = 30,000,000 of it regular, and reads the reservoir a result settled with (`AdditionalLimit::reservoir_remaining`), which the fork no longer does.

## Running the checks

Commands are in [`AGENTS.md`, "Build & Development Commands"](../../../AGENTS.md#build--development-commands) and the gates in [`AGENTS.md`, "Test Gates"](../../../AGENTS.md#test-gates).
Four notes that are easy to miss:

- the `mega-evme` tests run with `--manifest-path bin/mega-evme/Cargo.toml`, not `-p`;
- run the suite with `MEGA_SATIN_CPSB` and `MEGA_SATIN_CPHB` unset, or the tests that pin the spec's byte prices skip;
- after any test change, run `scripts/price_grid.sh --pr`;
- the execution-spec gate needs Ethereum's fixture releases, at the versions `.github/workflows/exec-spec-satin.yml` downloads.

Evidence that a change alters no result: `state-test --json-digest` at two commits, detached, over both fixture releases, in equivalence mode, in Satin mode and in Satin mode under tighter limits (the `--limit-*` options), each sorted and compared byte for byte, plus the gates and the byte-price grid.
The digest hashes each transaction's whole outcome, its gas by ledger and its usage counts included, so it catches a shift between ledgers that `--json-outcome` does not.
`--json-witness` (and `state-test btest --json-witness`, per block) adds the hash of what each transaction read below the journal, so a change that keeps every outcome and reads other state — a code, a block hash, a SALT bucket, an account a rejected transaction loads — is caught too; it prints in the same pass as the digest, whose lines it leaves unchanged.
A golden or snapshot changes only for a stated reason: regenerate it and read the diff.

## Reading order

1. The consensus core: limits (the vocabulary of pool, record, stop and crossing), then execution and frames with gas detention, then the block executor, then the system contracts and system address.
2. The gas schedule and pricing.
3. The inspector contract.
4. The node surface, validation and the witness.
5. Tools, tests, goldens and CI.
   Skim the goldens and snapshots; read what changes them.

Skim without reading: the SALT bucket-hasher internals, the generated pricing table, the tracer snapshots, the receipt goldens and the keyless differential digest.
The module documentation at the top of the main source files states the same invariants as the lists below, often more exactly; where an entry names what to read first, start there.
A test name or a line count is not a reason to trust the code: read the code against the spec.

## Glossary of the terms the checks use

- **Live system address**: the address the `SequencerRegistry` names (`_currentSystemAddress`).
  `MEGA_SYSTEM_ADDRESS` in code is only the address the registry named when Rex5 created it, which a chain's registry holds until a rotation.
- **Pre-block caller**: `0xff…fe`, the EIP-4788 system address, the caller of the pre-block system calls.
  It is a different address from the live system address.
- **MST** (Mega System Transaction): a signed legacy transaction from the live system address to a whitelisted system contract (today the Oracle), promoted to a deposit by the engine.
- **System-originated** (spec §19, `originates_from_the_protocol` in `src/system/tx.rs`), decided from the transaction's own fields:
  an MST before or after promotion; any transaction whose caller is the pre-block caller; any deposit carrying the source hash the promotion stamps, `keccak256("MEGA_SYSTEM_TRANSACTION")`, whoever sends it.
  Such work is exempt from history gas, every per-transaction limit and frame budget, and detention, and priced at the minimum bucket; its usage is still counted.
- **Pool**: a frame's budget in data size, KV updates or state growth, held on its start's entry of the engine's start ledger (`src/limit/ledger.rs`); the frame-start hook parks a child's share, revm's split of the start opens the child's entry with it, and revm's settlement of the start merges the child's pools into its caller's, as revm settles the reservoir.
- **Write record**: one per account or storage slot the transaction keeps changed, by the net change; 40 bytes of data size and of history; the KV count.
- **State growth**: the net count of new accounts and new slots the transaction keeps.
- **Stop**: what a limit does.
  The frame that crosses it reverts with `MegaLimitExceeded(kind, limit)`, and its caller resumes and may catch it.
- **Answered frame**: a frame that gets a result without running (a precompile, a call to an account with no code, an interceptor, the engine, an inspector's hook).
- **Start charge**: the history gas a frame start charges its caller for the records the started frame makes, kept on the start's entry of the start ledger and given back to the caller when the frame fails.
- **Failed-charge record**: the regular gas a frame had before a charge that failed, which the fork keeps, so the engine knows exactly what a halting frame burned.
- **Byte-price grid**: the CI job that runs the suite at other costs per state and history byte, because the prices are provisional.

## Module map

Paths are relative to `crates/mega-evm/` unless they start with another top-level directory.
For each module: where it is, what to check, the spec sections, and the `AGENTS.md` subsection with the detail.

### Limits: data size, KV updates, state growth, the stop

- **Where:** `src/limit/` (the byte table, `LimitKind`, `LimitCheck`, `MegaLimitExceeded` and `EvmTxRuntimeLimits` in `mod.rs`; the write records and the state growth a write and a frame start make in `record.rs`; `AdditionalLimit` in `limit.rs`; the start ledger in `ledger.rs`), `src/block/limit.rs` (`ProtocolLimits::for_spec`, `BlockLimits`, `BlockLimiter`), `src/constants.rs`.
- **Read first:** the module doc of `src/limit/mod.rs` and the doc of `AdditionalLimit` in `src/limit/limit.rs`.
- **Check:**
  - A count is drawn where it is made, on the pool of the frame that makes it (`AdditionalLimit::draw`), and a failed frame's draws go back when its start settles.
  - The transaction's limits are held beside the pools, on what the transaction keeps (`AdditionalLimit::usage`): the body, the hints, the authorities' records and the accounts they created, a deposit's created caller, and what the frames keep, restored when a start's frame keeps nothing (`AdditionalLimit::close_start`).
  - A completed write and a decided start are held in one function (`AdditionalLimit::check`, through `draw` and `count_start`): in the order data size, KV updates, state growth (`limit::COUNTED`), the frame budget, then the transaction limit; a hint, the authorization list and a caller's hold at a child's return check the same limits directly.
  - The stop names the budget when the budget is crossed, and the transaction limit only when it is not, which a framed write reaches only in data size, after Oracle hints used what the transaction had left; a start names it too when a deposit-like transaction's created caller stands on a state-growth limit of nothing, which only a tool's limit sets.
  - Every stop comes from `AdditionalLimit::crossed`, which also applies the exemption of system-originated work and system calls.
  - A write record follows the net change: an account's while its nonce, balance or code differs from what the transaction loaded, a slot's while its value differs from its original; the sender's account and the fee credits the settlement makes are the body's, not records.
  - A creator's nonce record is counted on the creator when the creation returns with the nonce bumped, held to the creator's budget, and survives the creation's failure; the creation's budget is the share taken at the opcode, before it.
  - State growth rises by one at each EIP-8037 account-leaf or fresh-slot charge site and for the caller a failed deposit creates, and a slot written from an original zero back to zero falls by one; a frame's net is signed, a negative net never enlarges a child's budget (`StartLedger::cap_child` in `src/limit/ledger.rs`), and a transaction reports `max(net, 0)`.
  - The first effective `SELFDESTRUCT` of an account created in the transaction applies the destruction table: each part absent at the transaction's start (a slot by its original in the transaction, a leaf by no account standing there) and present before the destruction is refilled at the price it was charged, to the destroying frame, the leaf and each such slot lower state growth by one and lose their write records, and the code's data size and history gas are taken back; from the mark the account adds no further state growth, state gas or write record, and a failure of the destroying frame, or of a frame that contains it, discards it all.
  - A caller resumed after its child's crossing is held to what the transaction has left, and stops in turn if a count or a charge of its own crosses again; a caller a failure below it left over the transaction's data-size limit, a hint having filled room a take-back freed, returns the transaction's stop before its next instruction (`AdditionalLimit::hold_caller`).
  - `LimitKind` keeps Rex6's encoding: data size 0, KV updates 1, compute gas 2, state growth 3.
  - Limits are per-spec constants, never chain configuration; a bare `MegaContext` runs under them; `BlockLimits` is builder policy and can only tighten.
  - `KV × 40 ≤ data size` holds after every instruction.
  - An EIP-7702 list long enough to cross the KV-update or state-growth limit is simulated whole before the first frame, writing and warming nothing; one that would cross is skipped whole, and the transaction's own frame is stopped before it runs (`hold_authorization_list` in `src/evm/execution.rs`); a shorter list cannot cross and is applied as EIP-7702 applies it.
  - A hint that would cross the limit is neither counted nor forwarded; an admitted hint stays counted through any stop.
- **Spec:** §7, §10, §11, §19, §20.
- **Detail:** [`AGENTS.md`, "Contracts of the common execution layer"](../../../AGENTS.md#contracts-of-the-common-execution-layer).

### Execution and frames

- **Where:** `src/evm/execution.rs` (`MegaHandler`; `MegaEvm`'s frame start and return, `frame_init` and `frame_return_result`, and the run-result hook `frame_run_result`), `src/evm/host.rs` (the completed-write and frame-start hooks), `src/evm/context.rs` (`MegaContext`, `spec_cfg`, `admit_code_deposit`), `src/evm/mod.rs`, `src/evm/state.rs`, `src/evm/instructions.rs`, `src/evm/frame.rs`, `src/evm/result.rs`.
- **Read first:** the docs of `frame_init` and `frame_return_result` in `src/evm/execution.rs`, and the module doc of `src/evm/host.rs`.
- **Check:**
  - A write is counted in the Host's completed-write hook (`write_completed`), which revm calls only once the opcode made every charge, so a failed opcode counts nothing; the Host's write methods count nothing, whoever calls them.
  - A completed write's record is held to the limits before its history is charged, so a record a limit refuses is not charged; at a frame start the order is the other way round, the caller paying at its opcode before revm decides the start and its records are held.
  - The frame-start hook (`frame_starting`) charges the caller the history of the start's records after the forward is taken, out of what the caller kept, parks the frame's own part as the start's charge on the start ledger with the frame's budget shares and allowance, and takes the frame's reservoir share out of the caller (`forward_start`); a caller that cannot pay gets the forward back, the halt's record re-written, and starts no frame.
  - Every budget a limit check reads, and every draw, is the start ledger's top entry, the running frame's start (`StartLedger::budget`, `draw`); with no start open there is no frame budget.
  - The start charge comes back in `frame_settle`, by the settlement's status, `created` and revm's inspector flags.
  - A start's records, state growth and transfer log are read off what revm did: its accounts before revm decides the start and again once it did (`FrameStart::of` and `FrameStart::count` in `src/evm/execution.rs`, `Accounts::start_before` and `start_made` in `src/limit/record.rs`), but for a call that moves no value and runs no precompile, which writes nothing on them (`start_writes_nothing`), counted on the started frame's pools (`AdditionalLimit::count_start`) for a frame revm built or a call it answered with a success; an answer that failed counts nothing and is held to no records limit, and a precompile gas detention answers from its price counts nothing and is held to the price alone; a start revm refuses on its caller's account counts nothing and is charged nothing.
  - A frame revm built whose start crossed a limit ends with the stop before its first instruction (`end_before_it_runs`); a success revm gave without building a frame gives way to the stop on its untouched forward, its value move and log taken back to the start's journal mark (`stop_the_answer_at`, `take_back_a_stopped_answer`); an answer that failed is revm's result, held to nothing.
  - Pre-frame charges (a deposit's caller account, EIP-7702 authorities, pre-frame records) either succeed or leave an included out-of-gas halt without a frame, with the journal unwound as revm unwinds it (a creation keeps its nonce bump).
  - A deposit that fails validation or cannot pay a pre-frame charge is included as a failed deposit: whole gas limit used, mint credited, nonce bumped.
  - The forwards: regular gas `remaining − remaining × 2 / 100`, plus a value call's stipend after the cap; the reservoir by the same subtraction, the transaction's own frame and a system call's outermost frame taking the whole; each pool `remaining × 98 / 100`, rounded down.
  - `spec_cfg` fixes the configuration whatever the caller passes (Satin schedule, EIP-8037, EIP-2780, execution cap 200,000,000, EIP-7708, chain-id check on, the contract size limit at 512 KiB and the initcode size limit at 1 MiB, the fork's legacy journal semantics off).
  - Per-transaction state resets with the transaction; a system call has its own entry point, and the handler splits its gas (`MegaHandler::tx_gas`).
  - An execution that fails with an error leaves nothing the next one on the same EVM reads: `MegaHandler::catch_error` empties the error slot before op-revm runs, discards the journal transaction on an error (a failed deposit's commit stays), and clears the L1 cost, the local context and the frame stack after op-revm; every entry point that takes the state off the journal on a success does so on an error, and a `_one` entry point keeps the calls before the failed one (`tests/satin/reuse_after_error.rs`, every public entry point, plain and inspected).
  - A fact about the transaction's start is read with what the journal records of the transaction (its original info, its entries, its transaction id), never off an account flag revm sets once per journal alone, so on a journal that runs several transactions unfinalized the engine's counts follow what revm charges there.
- **Spec:** §3, §4, §5, §9, §10, §11, §14, §21.
- **Detail:** [`AGENTS.md`, "How Satin executes today"](../../../AGENTS.md#how-satin-executes-today).

### Gas detention

- **Where:** `src/access/` (`Detention`, `VolatileDataAccess`), the volatile-read wrappers in `src/evm/instructions.rs`, the loads the Host marks or refuses in `src/evm/host.rs`, `hold_precompile`, `settle_answer`, `settle_system_answer` (which applies the switch-off an interceptor's answer carries) and `end_frame` in `src/evm/execution.rs`, the switch in `src/system/control.rs`.
- **Read first:** the module doc of `src/access/mod.rs`.
- **Check:**
  - The split into spendable and withheld gas is applied when a read completes, when a frame starts or resumes, and after an `SSTORE`; only regular charges are held to the spendable part, every other reader sees both.
  - The withheld part goes back to the sender unspent, except from a failed deposit.
  - A crossing is classified in the run-result hook (`frame_run_result`, `Detention::on_frame_end`), and only on an out-of-gas: the frame's gas is put back to what it was before the charge, the frame stops with `MegaLimitExceeded(2, limit)`, and its caller gets everything the frame had.
  - Compute is every regular charge the frames make, the work run on a value call's stipend included, less the state and history gas that spilled onto regular gas, and less what halting frames burned, which is read off the fork's failed-charge record.
  - A value call's own regular charges are decided first, and stop the calling frame if they cross; a precompile is then decided from its price before it runs, and one the engine cannot price runs on the allowance.
  - What is volatile: the block environment (`SLOTNUM` and `BLOCKHASH` among it, `BLOBHASH` not), the block beneficiary's account, the Oracle's storage; a call to the EIP-2935 history contract reads the block number, and a `CREATE`'s loads of its creator and created address are no reads.
  - A system-originated transaction and a system call are never detained.
- **Spec:** §12, §13.
- **Detail:** [`AGENTS.md`, "Gas detention"](../../../AGENTS.md#gas-detention).

### Block executor and block rules

- **Where:** `src/block/` (`executor.rs`, `limit.rs`, `result.rs`, `eips.rs`, `l1_block_info.rs`, `factory.rs`, `helpers.rs`).
- **Read first:** the module doc of `src/block/executor.rs`.
- **Check:**
  - The activation-block rule is caller-set (`no_user_tx_activation_block`, default `false`); `MegaHardforks::admits_only_deposits` is what a builder computes it from.
  - A transaction is an MST when it is a legacy transaction of the system shape from the live system address, decided before it runs; a failed read of the address is an internal error.
  - The block's packing budgets are data size 13,107,200, KV updates 500,000 and state growth 1,000, checked in that order before a transaction runs (`BlockLimiter::pre_execution_check`): the transaction that reaches one is packed, and every later ordinary transaction is refused.
  - A deposit and an MST are never refused by the packing budgets or building policy and count towards them; both are held to the header gas limit (`BlockLimiter::pre_execution_check`), and an MST, a legacy envelope, to the DA footprint (`check_da_footprint`), which a deposit is exempt from.
  - `commit_transaction_outcome` checks the counters again and refuses a stale outcome: the sender's nonce or balance moved since the candidate executed (`StaleOutcome`, internal).
  - Pre-block order: EIP-2935, EIP-4788, in Satin's activation block alone the check of the six system contracts and the EIP-7997 factory, the registry's due change, the read of the L1 block info.
  - Every pre-block call runs through one helper (`transact_pre_block_call` in `eips.rs`) on `PRE_BLOCK_CALL_GAS_LIMIT`, whatever the block's gas limit; one that does not succeed makes the block invalid, and a database error the database declares fatal is an internal error.
  - The pre-block calls count towards no block budget.
  - Block execution admits any inspector; the node decides which run on block production.
  - The block's counters: regular gas as `max(regular, floor)`, state and history gas, history bytes, data size, write records and state growth; the three gas ledgers are reported, not limited.
  - When the OP base moves, diff the file against the new base's `OpBlockExecutor`.
- **Spec:** §1, §18, §19, §20, §21.

### System contracts and the system address

- **Where:** `src/system/` (`intercept.rs`, `oracle.rs`, `control.rs`, `limit_control.rs`, `keyless/`, `tx.rs`, `predeploys.rs`, `sequencer_registry.rs`), `crates/system-contracts/`.
- **Read first:** the module doc of `src/system/tx.rs`.
- **Check:**
  - Every transaction reads the registry's account and its `_currentSystemAddress` slot, warm, by comparing the code hash, without running the registry's code.
  - A transaction from the live system address without the system shape is invalid; a deposit from it, which never has the shape, is a failed deposit, including one carrying the MST source hash.
  - A deposit carrying the MST source hash from any **other** sender is not refused: it is system-originated, so it is held to no limit, pays no history gas, is not detained and is priced at the minimum bucket.
    The spec's basis is that no deposit derived from L1 carries the hash.
    This is the widest exemption in the engine, so check that claim.
  - An MST is validated (chain id, nonce, EIP-3607) before it is promoted to a deposit.
  - Satin deploys, upgrades and seeds no predeploy.
    The block in which Satin activates (`MegaBlockExecutionCtx::satin_activation_block`, set by the node from its schedule and the parent's timestamp) reads the seven, each a read-only entry, and each must hold exactly Satin's code whatever its nonce, balance and storage.
    Anything else — no account, no code, an older version (the Oracle's 1.0.0 / 1.1.0, the registry's 1.0.0), other code — makes the block invalid with one error, `UnexpectedSystemContractCode`.
    No other block reads them.
  - An Oracle read in a system call reads chain storage, never the oracle service.
  - Keyless: the call runs as a native creation of the recovered signer; an empty code deposit at the deploy address is refused with `EmptyCodeDeployed`, a constructor's inner creation returning empty code is not; the answers are the documented error set.
  - The keyless creation forwards `min(gasLimitOverride, remaining)` of regular gas, the reservoir share of what the call has left and the child share of each of the call's pools; the signer's nonce record is counted on the call at its settlement, once the creation returned.
  - The interceptor dispatch order is scheme guard, address, selector, then value policy; an unknown selector is not intercepted.
  - Deployed bytecode and code hashes match the attested artifacts (`build.rs`).
- **Spec:** §13, §15, §16, §17, §18, §19.

### Gas schedule and pricing

- **Where:** `src/evm/schedule.rs`, `src/evm/prices.rs`, `src/evm/history.rs`, `src/evm/precompiles.rs`, `src/external/` (SALT pricing), `state_gas_price` in `src/evm/host.rs`, the Amsterdam opcode activation in `src/evm/instructions.rs`.
- **Check:**
  - Every regular entry is Amsterdam's; state creation is paid once, in state gas.
  - The regular-gas forward is a schedule entry, `call_stipend_reduction` = 50; the shares of the reservoir and the three budgets are the engine's (`FRAME_KEPT_PERCENT` = 2), in no schedule entry; the neutral configuration keeps EIP-150's 63/64 and forwards the whole reservoir and budgets, whatever schedule it is given.
  - State gas = schedule entry × bucket multiplier `m`; regular gas never scales.
    A failed capacity lookup is a node fault: the transaction gets no outcome and is not settled, and the node reports an internal error with the cause.
  - History gas uses the limit layer's byte table, so a record's history bytes are its data size; the three sites where the totals part are in the `src/limit/mod.rs` module doc, "The byte table".
  - A deposit, a protocol-produced transaction and a system call pay no history gas.
  - The history allowance (`HISTORY_ALLOWANCE_BYTES`, 160 bytes; `history_allowance_gas()` in `src/evm/history.rs`, kept on the start ledger, in no schedule entry) draws only on a log's charge.
  - The EIP-7623 floor is still computed and validated.
  - The constants are provisional; a build with `satin-price-override` must never reach a node.
- **Spec:** §2, §4, §5, §6, §7, §8, §9.

### Inspector contract

- **Where:** the `evm` module documentation (`src/evm/mod.rs`, "Inspectors") and `src/evm/execution.rs` (`frame_init`, `frame_run_result`, `frame_return_result`; `frame_run` and `InspectorEvmTr` are revm's), `test_utils::ReadOnlyObserver`.
- **Check:**
  - The module documentation of `src/evm/mod.rs` is the contract (the spec does not describe inspectors): an inspector runs on revm's inspected lifecycle unchanged, and every per-frame mechanism sits in the frame start, run result or return both loops share.
  - A rewritten result is taken as given: the caller settles it as revm does, with no correction of the engine's.
  - An inspector's answer never reaches the engine's frame start, so the depth guard, the interceptors and gas detention see none of it, and it settles on the gas it is built on.
  - A frame that crosses the compute limit is seen to end with the stop: the stop is made in the fork's run-result hook (`frame_run_result`), before the inspector's `call_end`, `create_end` and `frame_end`.
  - A `keylessDeploy` call's frame is handed to no `initialize_interp` and gets no `step`: the inspector sees the call start, its creation, and the call end.
  - A start an inspector rewrites after its opcode is not charged again: what the opcode charged stands, its declared part coming back only as for any start whose frame keeps nothing (a frame that fails, or a start revm refuses before it bumps a nonce or moves a value), and its records are those revm then made.
  - A pre-block system call runs on the plain system-call path, with no callback.
  - `test_utils::ReadOnlyObserver` checks, in debug builds, that an inspector a test declares an observer writes nothing back: a tripwire for tests, not a proof.
  - The start ledger settles an inspector's answer and a replaced tracker at unlimited budgets, and gives no start charge back for an answer or a rebuilt outcome; an inspector has no budget, start-charge or allowance field to write on an input, an answer or a result, since the engine keeps all three on its ledger, a restored tracker snapshot leaves the ledger's draws standing, and a replaced interpreter leaves the frame its allowance; the tests of such inspectors read what the ledger settled.
- **Detail:** [`AGENTS.md`, "Contracts of the common execution layer"](../../../AGENTS.md#contracts-of-the-common-execution-layer), the entries on inspectors.

### Node surface, validation and the stateless witness

- **Where:** `src/lib.rs` (explicit exports), `src/block/hardfork.rs`, `src/block/chain.rs`, `src/block/genesis.rs`, `src/evm/validation.rs`, `src/evm/state.rs`, `src/block/l1_block_info.rs`, `src/test_utils/witness.rs`, `crates/mega-state-test/src/witness.rs`.
- **Check:**
  - `validate_schedule` refuses a schedule in which a base fork Satin runs on is not active by `satinTime`.
  - The genesis format has exactly one key, `satinTime`; Satin carries no parameter, and every other `satin*` key (the old registry and limit keys included) is refused.
  - A key given twice is read as its last copy by every reader, and `satinTime` is also read from the `arbitrary_precision` object form, decimal digits alone.
  - The pool and the engine refuse the same oversize body with the same error (`TX_BODY_OVER_DATA_SIZE_LIMIT`): body + 40 × authorizations over the transaction limit.
  - The witness holds every read: block hashes, SALT bucket ids, the Oracle reads of the block's transactions (none for system calls), the L1 block info, the registry, and every authority the authorization-list simulation loaded.
  - The witness is the sequence of states the block commits: each pre-block step commits its state on its own, read-only entries included, where a commit hook on `State` receives it whole; nothing else hands the pre-block states out.
  - A witness replay's exports are a subset of the building node's, and may be a proper one.
  - A node's database answers an address it holds no account for as no account (`None`), never as an empty account: the destruction table gives a leaf back only when no account stood there.
- **Spec:** §1, §21, §22, "Node integrators".

### Tools, tests, goldens, benches and CI

- **Where:** `bin/mega-evme/` (`bin/mega-evme/src/engine.rs` picks the engine from `--spec`: Satin in-tree, legacy specs on a released crate; `run` and `tx` default to the legacy `Rex6`), `crates/mega-state-test/`, `crates/state-test/`, `crates/mega-evm/tests/` (`satin/`, `block/`, `system/`, `fuzz/`, `tracers/`), `crates/mega-evm/benches/`, `.github/workflows/`, `scripts/`, `mutants/suppressions.toml`.
- **Check:**
  - No consensus path changed for a tool's sake; tool overrides (`--override.limits`, `with_tx_runtime_limits`, `with_protocol_limits` under `test-utils`) are explicit and never default.
  - A limits file of the first shape, without the KV-update and state-growth fields, reads with the default spec's values for them; every other field stays required, and an unknown one is refused.
  - `state-test --json-digest` hashes each transaction's whole outcome, `--json-witness` what it read below the journal (and `btest --json-witness` what each block read), and the `--limit-*` options run Satin mode under tighter limits in each dimension.
  - A test pins a spec rule, not the implementation; a new or changed test says which rule it checks (some headers carry a `[S<section>.<n>]` tag: the spec section and item).
  - A figure the schedule determines is asserted against its derivation, not pinned; snapshots and goldens own only outputs with no computable expectation.
  - The limit matrix (`tests/satin/limit_matrix/`) runs every row, none of them ignored.
  - `mega-evme`'s recorded mainnet blocks are one deterministic archive, `bin/mega-evme/tests/fixtures/blocks.tar.xz`, extracted once per test binary into cargo's target scratch directory, each file moved into place on its own so racing extractions all succeed; `bin/mega-evme/scripts/pack_blocks_fixture.py --check` holds a repack of it byte-identical.
  - The suite passes at the byte-price grid's points, not only at the spec's prices.
  - The mutation gate is fail-closed: a suppression without a locator may cover only one mutant.
  - A fuzz run that checks nothing fails (zero cases, zero comparisons).

## Judging a finding

1. **Reachability first.**
   Does the node's block execution or building path reach it?
   With the default configuration, or only with a non-default limit?
   Is it decided below?
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
Every limit stop — the transaction's data-size, KV-update and state-growth limits, a frame's budget in those three, gas detention's compute limit — reverts the frame that crosses it with `MegaLimitExceeded(uint8 kind, uint64 limit)` on the frame's own gas, so the unspent gas goes back to the caller, its reservoir included, the frame's budgets go back to the caller's when its start settles, and the caller resumes and may catch the stop.
An execution fault — a real out-of-gas, a precompile given less than its price, an invalid opcode, a charge the frame cannot pay — halts and consumes the frame's gas, as in revm.
One ABI serves every kind of stop, and there is no third ending: the halt reasons are op-revm's (`MegaHaltReason`), and the engine makes no halt of its own.
An operation that crosses several counted dimensions at one site stops on the first of data size (kind 0), KV updates (kind 1) and state growth (kind 3); compute (kind 2) is decided at the regular charge, before the write is counted.
Inside a dimension the frame budget is tested before the transaction limit, and the stop names the budget whenever the budget is crossed.

The deliberate exceptions:

| What happens                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | Why                                                                                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A deposit that fails validation — its body over the data-size limit, or sent from the live system address — or that cannot pay a charge made before its first frame, or whose own frame halts, is included as a failed deposit: its whole gas limit billed, its nonce bumped, its mint kept, nothing else applied, no stop reported.                                                                                                                                                                                     | The OP stack's deposit rule: a deposit comes from L1, where its gas was paid, and must be included, so it can be neither rejected nor caught by a caller.                                                                                                                                                                     |
| An input over one of Optimism's precompile input-size limits (the BN254 pairing, the BLS12-381 MSMs and pairing) is refused before any gas check: the call halts and burns its forward.                                                                                                                                                                                                                                                                                                                                  | The Optimism precompile specification, inherited with the Karst set, makes the size check the precompile's own input check, which fails the call like any bad input, and gas detention prices such an input at zero, so it is never a crossing.                                                                               |
| Under gas detention two precompile faults end as the compute stop instead of a halt: a priced precompile whose price is past the allowance and within the forward is not run, even when its input would fail a check made after the gas check; and a precompile the engine cannot price (one a node added or replaced) that runs out of the allowance is the stop, even when its price is past the whole forward.                                                                                                        | revm runs a precompile inside the frame's start, so the engine must decide before it runs and decides from the price, which says nothing of a check made after the gas check, while a precompile without a price can only be run on the allowance, and running out of it is all the engine sees.                              |
| Changed after #407 merged: a call whose start (its value transfer's records and transfer log) crosses a data-size, KV-update or state-growth limit gets the stop on its untouched forward when revm built its frame or answered it with a success; a call revm answered with a failure — a precompile that fails after the value moved — kept nothing and halts as revm has it, burning its forward, and a precompile gas detention answers from its price is the compute stop, whatever its records would have crossed. | A limit is enforced at the writes it guards, which are what the journal kept: a success kept the start's writes, so it gives way to the stop; a failure's writes revm took back, and a precompile answered from its price moved nothing, so neither has a write to hold.                                                      |
| A frame answered without running (an interceptor's answer, or a `keylessDeploy` call refused for carrying value) that spent more regular gas than the frame's allowance is the compute stop, and none of its spending is billed, the part within the allowance included, where a frame that ran keeps what it spent before the charge that crossed.                                                                                                                                                                      | An answer is one step with no point inside it where the allowance ran out, so its whole spending is the charge that crossed, and a charge that crosses is never made; of the engine's own answers only the `keylessDeploy` refusal spends regular gas, its overhead.                                                          |
| Refusals that are not limit stops revert with their own error, not `MegaLimitExceeded`: `VolatileDataAccessDisabled(uint8)` (the opcode's static gas paid, nothing else; 12 for `SLOTNUM`, outside the contract's enum), `DisabledByParent()`, `NonZeroTransfer()`, `NotIntercepted()`, keyless deployment's rule errors, and revm's `CallTooDeep` and `OutOfFunds` with empty data, all of them catchable, with the outcome's `limit_exceeded` left empty.                                                              | They refuse what the caller asked for, not a resource the transaction ran out of: each names its reason in the ABI its contract, keyless deployment or revm already defines, so a caller can tell it from a stop.                                                                                                             |
| One size rule has three endings: initcode over the size limit is rejected at validation in a non-deposit creation transaction, halts the creating frame at `CREATE` or `CREATE2`, and reverts a `keylessDeploy` call with `InitCodeTooLarge(size, max)`; deployed code over the code-size limit halts the creation, while deployed code within it that takes the transaction or the frame over the data-size limit is the stop.                                                                                          | The size halts are Ethereum's (EIP-3860 for initcode, EIP-170 for deployed code) at MegaETH's sizes, rules of the EVM rather than MegaETH's resource limits, so they stay halts, while a keyless call checks the init code among its rules before any creation starts and every rule refuses with a revert and its own error. |

Not exceptions, but easy to mistake for one:

- A transaction that cannot pay a charge made before its first frame — the account a deposit-like transaction creates for its caller, an EIP-7702 authorization's charges, the history of the records made before the first frame — ends out of gas with no frame: its whole regular budget is consumed and its reservoir returned.
  That is EIP-2780's runtime out-of-gas path, a gas fault (`pre_execution` in `src/evm/execution.rs`).
- An EIP-7702 list that would take the transaction over its KV-update or state-growth limit is skipped whole, and the transaction's own frame is stopped before it runs, naming the transaction limit (kind 1 when both would cross).
  The crossing happens before any frame, yet it is a limit stop, where Rex6 halted; the intrinsic gas, the body's history and its data size stay, and nothing of the list is kept.
- A deposit whose own frame is stopped settles as a revert, like any transaction's, and is not a failed deposit.
- A failed deposit that creates its caller counts that account as one unit of state growth, for the transaction and the block, and no limit stops it: it has already failed.
- A keyless creation that reverts, halts or is stopped makes the `keylessDeploy` call succeed, with `ExecutionReverted`, `ExecutionHalted` or `EmptyCodeDeployed` in its ABI return, a stop's revert data carried inside `ExecutionReverted`.
  The call itself reverts with the stop only when its own charges or the signer's nonce record cross a limit; otherwise `answer` in `src/system/keyless/settle.rs` builds its ABI return when the creation returned (`frame_return_result` in `src/evm/execution.rs`).
- Under gas detention, only a regular charge the withheld part would have paid is a crossing; a charge beyond the frame's whole regular gas, and a failed state or history charge, halt as they would without the read.
- Under gas detention, a value call whose own charges cross stops the calling frame before anything moves, whatever its target and whether or not the caller can fund the value.
- A creator that its creation's nonce record takes over a limit is stopped when the creation returns, before it runs another instruction, whatever the creation's own result.
- A frame a failure below it left over the transaction's data-size limit — the failure put back a record of a frame above, and a hint the failed frame sent stays — is stopped as it resumes, before it runs another instruction, whether the frame below reverted, halted or was itself stopped.
- The destruction table's give-back crosses nothing; the operation's positive effects are held after it, in the order data size, KV updates, state growth, and one that crosses stops the destroying frame, which then keeps nothing of the operation; a regular charge of the `SELFDESTRUCT` that crosses gas detention stops the frame before the opcode completes, and none of the table applies.

## Already decided — do not re-report

Each is a decision with its reason; "in this pull request" means review the code, not the decision.

- **KV updates and state growth are limited, per transaction, frame and block; state gas and the block's execution gas are not**: the count, not the gas, stops the 1,001st slot, and the KV check is made though at Satin's constants data size binds first.
- **Consensus limits are one per-spec table** (`ProtocolLimits::for_spec`), not chain configuration: configured values could stop the L1-attributes deposit and cost a silent factory fallback.
- **No latch**: a crossing is a native revert of the crossing frame, which callers may catch, and a caller that runs on is held to what the transaction has left, so no transaction keeps more than its limits.
- **An oversize transaction body is refused at validation** (a deposit becomes a failed deposit): the body is known before execution, and at Satin's constants the bound is unreachable (6,732,889 < 13,107,200).
- **An EIP-7702 list over the KV-update or state-growth limit is skipped whole** and stops the first frame before it runs: Rex6's all-or-nothing rule, as a revert-class stop, because a stop of the first frame does not revert what was applied before it.
- **The stop names the frame budget when both the budget and the transaction limit are crossed**; when a block is full on several budgets, a node may name them in the order data size, KV updates, state growth, and two nodes need not name the same one.
- **`tx_chain_id_check` is pinned on**: it is revm 40's default, and it makes the system-transaction chain-id guard effective.
- **Every transaction pre-warms the registry read**, so gas does not depend on the envelope type; the non-warming journal read is not a generic upstream candidate.
- **A delayed commit re-checks only the sender's nonce and balance**: other staleness is the builder's documented responsibility, as the system-transaction-first flow requires.
- **An MST is never refused by a packing budget or builder policy**: it serves a user transaction already admitted.
- **Satin has no deploy or upgrade path**: it runs on a chain that ran the legacy forks, and its activation block refuses any predeploy without Satin's code; no contract changes.
- **Keyless refuses only an empty code deposit** at the deploy address; the four producer-less errors are deleted, which changes no deployed byte.
- **A rewritten result is taken as given, as revm takes it**: a revived creation settles as the success the inspector made of it (Foundry parity), and an inspector's answer settles on the gas it is built on.
- **The Amsterdam regular gas table is taken wholesale**: the earlier press-back to Osaka priced creation twice.
- **`validate_schedule` checks that base forks are active by `satinTime`**; engine-built schedules activate them from the first block, which no Satin block can tell apart.
  An unknown chain has no schedule (`hardfork_schedule` answers `None`).
- **Block execution admits any inspector**: which inspectors run on block production is the node's decision.
- **Explicit crate-root exports; dead helpers and legacy-era remnants are deleted.**
- **Satin carries no fork parameter**, and the fork-parameter mechanism was removed with nothing using it.
- **An absurd state-gas price makes the operation run out of gas**; no reachable divergence.
- **The pending-changes due check uses the direct-read convention** (any database error is an internal error), as the other direct reads and upstream do.
- **Precompile prices are looked up by id**: a precompile set replaced around the engine, through the mutable accessors, keeps the built-in prices; the spec states the boundary, and nodes change the set through `with_dyn_precompiles` or the factory builder.
- **A keyless signer that is the block beneficiary is detained**, as in the legacy engine.
- **No MST in the activation block**: it admits only deposits and every MST consumer is a non-deposit; documented in the spec.
- **Write records follow the transaction-wide net change**, not Rex6's per-operation count: the record measures the state the transaction keeps, which is what history gas prices.
- **The limit override is an explicit tool opt-out** (`with_tx_runtime_limits`); block execution and validation use the chain's limits.
- **No spec ladder in Satin yet**: it comes with the next spec, with the extension points kept; `AGENTS.md` says how to add it.
- **The fork finalizes the journal on an error** (`bluealloy/revm#3780`, carried early): kept; the fork documents it in megaeth-labs/revm#75.
- **`OpBlockExecutor` is not composed**: it requires `PostExecEvm` and returns `OpTxResult`.
- **The 98/100 shares and dropping a hint from a frame whose volatile access is off are kept**: without a latch the frame budgets and the reservoir share protect the caller's own work.
- **The neutral configuration forwards as Ethereum does** (EIP-150's 63/64, the whole reservoir): equivalence mode is Ethereum's forward, not Satin's.
- **An inspector's answer built on `Gas::new(limit)` carries no reservoir and unlimited pools**, so above the execution cap its caller keeps only the 2 per cent of its reservoir it kept, and the caller's budgets are lifted for the rest of its frame: tools only, kept.
- **Satin does not apply EIP-7610's storage collision**: the engine reads storage slot by slot and cannot see storage it has not loaded; the execution-spec gate skips those fixtures.
- **The destruction table refills state gas, which EIP-8037 does not**: a part the transaction made does not remain for a node to store.
- **The table's slot column follows the transaction's original**: a slot of a created account first accessed after the creation is zero at the start whatever the state held, as revm loads it; only a slot accessed before the creation keeps its value, and the specification says so.
- **The neutral configuration counts the destruction table**, and prices it as the fixture's fork does: equivalence mode's usage of a transaction that creates and destroys an account differs from a count without the table, no fixture checks usage, and no second counting rule is kept for the neutral configuration.
- **`BLOBHASH` is not volatile**, unlike in Rex6: it reads the transaction's own blob hashes.
- **The pre-block calls run on one constant budget** (`PRE_BLOCK_CALL_GAS_LIMIT`), not on the block's gas limit.
- **Mechanisms whose existence is kept from the legacy engine** (whether to have them is not under review; their Satin rules, which the specification states against Rex6, are): `MegaAccessControl`, gas detention's two categories and 20M caps, `remainingComputeGas()`, the Oracle, keyless deployment, the Mega System Transaction, the registry and rotation, the high-precision timestamp, SALT bucket pricing, the history allowance, the 98/100 frame share and Rex6's numbers for the data-size, KV-update and state-growth limits, the 200M execution cap with 512 KiB code size and the KZG price, the deposit-caller account charge, the DA footprint.
- **The upstream revm change `new_answer`** for answers to frames that did not run (`bluealloy/revm#3979`) is opt-in: the handler does not normalise answers, because real inspectors' answers can have effects.
