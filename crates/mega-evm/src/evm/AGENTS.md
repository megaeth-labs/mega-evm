# AGENTS.md

## OVERVIEW
MegaEVM execution core that wraps revm/op-revm with MegaETH instruction tables, host behavior, and execution interfaces.

## STRUCTURE
- `mod.rs`: `MegaEvm` wrapper, inspector toggling, execution convenience APIs.
- `context.rs`: execution context composition and state wiring.
- `execution.rs`: transaction execution flow, the two frame loops and the two frame-init paths, and result shaping.
- `frame.rs`: revm's frame-action processing, split so the journal decision can be withheld until the frame's result is final — `classify_frame_action` decides the result, `commit_frame_journal` carries the decision out.
- `factory.rs`: `MegaEvmFactory` builder for context and external env wiring.
- `instructions.rs`: spec-layered opcode table and extension wrappers.
- `host.rs`: host overrides for volatile tracking, oracle reads, SALT gas hooks.
- `limit.rs`: EVM-facing limit helpers and runtime-limit adaptation.
- `spec.rs`: `MegaSpecId` parsing/ordering utilities.

## KEY PATTERNS
- Instruction semantics are layered wrappers, not ad-hoc per-opcode mutations.
- Preserve wrapper ordering when changing opcode stacks.
- Spec upgrades extend prior tables by overriding changed opcodes only.
- Host methods mark volatile accesses before delegating to inner host behavior.
- Oracle `sload` handling forces cold semantics for deterministic replay.
- `MegaEvm` methods read aggregate resource usage from `additional_limit` after execution.
- Keep inspector and non-inspector paths behaviorally aligned.
  Observational inspectors (`NoOpInspector`, tracers that only read) are bit-identical to no inspector at all, and must stay so.
- Rewriting inspectors are supported in full, and what they do to gas is measured rather than assumed — see `## INSPECTOR CONTRACT` below for the per-shape table and the two shapes that are refused.
- Under REX7 a frame's journal decision travels: the frame loops park it on `MegaEvm::deferred_journal` and `frame_return_result` carries it out, after `AdditionalLimit::before_frame_return_result` — the last thing that can rewrite a frame's result — and before the caller resumes.
  There is never more than one decision outstanding and it never survives the step it was parked for; `hold_deferred_journal` asserts that.
  Anything new that can rewrite a frame's result has to land inside that window, or it reopens the split the deferral closed.
- Both frame loops and both frame-init paths run the same bodies; the inspected copies add exactly one thing, the callback that can rewrite a frame's classification.
  Add to the shared body, not to one copy: `tests/rex7/frame_loop_parity.rs` compares the two on every frame outcome, state included, and is what a one-sided edit fails.

## INSPECTOR CONTRACT

Every inspector `MegaEvm` is handed is wrapped in `MeasuredInspector` (`inspector.rs`) before it reaches the inner EVM.
The public accessors hand the unwrapped inspector back, so the type a caller names is unchanged and the wrapper is not something a caller opts into or can opt out of.

The shim's soundness rests on one fact: the EVM does not execute inside an inspector callback.
Anything that changes between the moment the shim delegates to the user's inspector and the moment control comes back is therefore the inspector's doing by construction, not by attribution — which is what makes the callback boundary a place a measurement can be taken at all.
The shim snapshots what it cares about on the way in, compares on the way out, and books the difference on `InspectorLedger` (`../limit/inspector_ledger.rs`), which travels out on `MegaTransactionOutcome::inspector_ledger`.

Every gas lane is two numbers, because the ledger's two consumers ask different questions.
The conservation law needs the **net**, since gas written into one object and taken back out of another really did leave the envelope where it was.
The block guard needs the **gross**, since two edits that cancel are two edits: a `+1` before a frame reads its own remaining gas and a `−1` after it has read it net to nothing and leave the frame holding a number the EVM would never have produced, and the same pair split across a surviving frame and a rolled-back one moves what the sender pays.
`InspectorLedger::is_zero` — the guard's question — is defined over the gross halves; `conjured_gas` — the law's term — over the nets.
`Lane::book` moves both, which is what makes it impossible to move a lane without the guard seeing it.

### What each rewrite shape costs

Read the table by the *argument the rewrite reaches through*, not by the tool that makes it: two tools editing the same argument are one row.

| Rewrite shape | Support | Where it is booked | What enforcement sees |
| --- | --- | --- | --- |
| Read-only observation | Supported, free | nothing | an empty ledger, and numbers identical to an uninspected run |
| Gas written into a live interpreter's counter (`initialize_interp`, `step`, `step_end`, `log_full`) | Supported | `InspectorLedger::gas`, at the callback boundary | nothing: the checkpoint baseline shifts by the same amount, and the gas clamp is re-derived on the spot so injected gas buys no compute headroom |
| A frame input's `gas_limit`, raised or lowered (`frame_start`, `call`, `create`) | Supported | `InspectorLedger::env`, at the callback boundary | nothing: a frame's compute budget comes from the tracker, not from its gas limit |
| A frame input's semantic fields — target, caller, value, scheme, calldata, static flag (`frame_start`, `call`, `create`) | Supported | `InspectorLedger::interventions`, at the callback boundary | nothing: it changes what the frame does, not what it costs |
| A synthetic outcome that skips the frame entirely (`frame_start`, `call`, `create`) | Supported | `InspectorLedger::interventions`; nothing on the `env` lane — the edited inputs never reach a frame — and the gas the outcome carries on the `result` lane, measured against the envelope the answering callback was handed rather than as a difference across it | the frame's envelope is settled at `finalize_frame` as `FrameExit::RefusedSynthetically` |
| A finished frame result's remaining gas (`call_end`, `create_end`, `frame_end`) | Supported | `InspectorLedger::result`, at the frame's settlement point rather than at the callback boundary | nothing |
| A finished frame result's returned output (`call_end`, `create_end`, `frame_end`) | Supported | `InspectorLedger::interventions`, at the callback boundary | nothing |
| A finished outcome's metadata — a call's `memory_offset`, a creation's `address`, the two flags beside them (`call_end`, `create_end`, `frame_end`) | Supported | `InspectorLedger::interventions`, at the callback boundary | nothing: it changes what the caller reads next, not what the frame cost |
| A refund written into any `Gas` a callback holds | Supported | `InspectorLedger::refund`, at the callback boundary, nominally | nothing: a refund moves `tx_gas_used`, not the `limit - remaining` the conservation law is stated over |
| The EIP-8037 state-gas pool or spend counter, on any `Gas` or on a call's inputs | Supported | `InspectorLedger::reservoir` / `::state_gas`, settled once from the figures the transaction ends with | the pool lowers the envelope the receipt reports, so it joins the law's `I` term; the spend counter moves the receipt's state-gas figure and nothing else |
| A successful frame result rewritten into a revert or a halt | Supported | `InspectorLedger::interventions` — no gas moves | the journal decision follows the final result, so the frame's state is rolled back with it; a precompile's executed/destroyed split follows it too |
| A failed **call** frame rewritten into a success | Supported | `InspectorLedger::interventions` | the journal commits, so the frame's state follows the result its caller was handed |
| A failed **contract creation** rewritten into a success | **Refused** | `InspectorLedger::rejected_rewrites`, alongside `interventions` | `reject_forbidden_create_rewrite` restores the original classification and fails the transaction with `EVMError::Custom`; debug builds assert |
| Any constant-time reading of a live interpreter's working set — its program counter, its code's identity, revm's `continue_execution` flag, the stack's length, the return buffer's identity, the memory's size and window offset, the memo of how far that memory has been paid for (`Gas::memory`), the frame's four identifying fields and its calldata's identity, the static flag, the spec id | Supported | `InspectorLedger::interventions`, at the callback boundary, off `inspector.rs::WorkingSet` | nothing directly — but a stepped program counter deletes an instruction from the frame, and growing the memory and the memo together skips the next expanding opcode's charge, which is why these need a booking at all |
| The *contents* of the interpreter's stack, memory, return buffer, calldata or code, at unchanged identities | Supported, unmeasured | nothing — telling whether they came back changed needs a snapshot of unbounded state | the EVM executes on the edited state and meters it as its own work, because it is |
| A direct journal write (`tstore`, `log`, …) | Supported, unmetered | nothing — no argument the shim holds describes it | `MegaETH`'s data-size / KV / state-growth lanes do not see it; it moves no gas, so the conservation law is unaffected |
| The gas a pending `InterpreterAction` carries, reached through `LoopControl` (`step_end`) | Supported | the lane the action the callback left behind names: `InspectorLedger::result` for a `Return` action, settled at the frame's settlement point because that action *is* the frame's result a moment later; `InspectorLedger::env` for a `NewFrame` one, booked at the child's `frame_start`; `InspectorLedger::gas` when the callback removed the action, because the frame then carries on spending its counter | nothing |
| A pending action's classification or output, or an action installed, removed or swapped for the other variant | Supported | `InspectorLedger::interventions`, alongside whatever gas the change moved on the lane above | it changes what the EVM does next, not what the frame has spent |
| `CfgEnv` or the active gas schedule | **Refused** | — | the gas-schedule pin panics; the schedule belongs to the spec, and a rewritten one has no accounting lane that could rescue it |

Two independent stops back the creation refusal: the shim restores the classification, and `frame.rs`'s `FrameJournalVerdict::CreateRejected` carries no code and no commit branch, so even with the refusal removed such a rewrite deposits nothing.

Booking is a *reported* quantity throughout. No resource limit is ever compared against the ledger, and `MegaTransactionOutcome::compute_gas_enforced` comes off the enforcement lane rather than out of the reported total — so an inspector cannot buy a transaction headroom on any dimension.

### The window a counter edit reaches nothing through

A gas-counter edit made while the interpreter is already holding a `Return` action is written into an object nobody reads again: revm's inspected loop runs `step_end` after the instruction that set the action, and the action carries its own snapshot of the gas, which is what becomes the frame's result.
The shim books nothing for such an edit and still shifts the settlement baseline for it — `MegaETH`'s tail settlement reads the counter after the action is set, so without the shift the edit would read as work the frame performed.
The predicate is the pending action's variant, not "the loop is ending": a `NewFrame` action ends the loop too, and that frame resumes on exactly this counter.

What the counter no longer speaks for, the action does, and the shim measures both against the same reading.
A frame holds `counter` with no action pending, `counter + f.gas_limit` with a `NewFrame` action, and the action's own copy with a `Return` one (`inspector.rs::held`); the counter lane books the counter's movement exactly when the EVM will read it again, and the action lane books the rest.
The two together account for every unit of gas the frame holds, whatever the callback did to the action's shape.

### Every gas an inspector can reach

The shape table above is written over rewrites this repository has thought of.
This one is written over the `Inspector` trait's own signatures, and is closed: `tests/rex7/gas_surface.rs` pins it against what upstream's derived `Debug` renders, field by field, and fails on one that has no verdict here.

Read it by the object the gas sits in.
The first six rows are the lanes measured across a callback boundary; the three after them are the numbers that share those objects and are measured somewhere else; the rest are the ones that need no lane, each with the reason.

| Gas carrier | Reachable at | Verdict |
| --- | --- | --- |
| `Interpreter::gas` → `remaining` | `initialize_interp`, `step`, `step_end`, `log_full` | `InspectorLedger::gas`, at the callback boundary, when the action the callback left behind leaves the counter live |
| A pending `InterpreterAction::Return(_)`'s `gas` → `remaining` | `step_end` | `InspectorLedger::result`, staged and settled at the frame's settlement point |
| A pending `InterpreterAction::NewFrame(_)`'s `gas_limit` | `step_end` | `InspectorLedger::env`, staged and booked at the child's `frame_start` |
| `FrameInput` / `CallInputs` / `CreateInputs` → `gas_limit` | `frame_start`, `call`, `create` | `InspectorLedger::env`, at the callback boundary — unless the same callback answers the frame, in which case this value is the baseline the row below is measured against |
| The `Option<FrameResult>` / `Option<CallOutcome>` / `Option<CreateOutcome>` a callback **returns** → `gas` → `remaining` | `frame_start`, `call`, `create` | `InspectorLedger::result`, settled at `finalize_frame` against that baseline |
| `FrameResult` / `CallOutcome` / `CreateOutcome` → `result.gas` → `remaining` | `frame_end`, `call_end`, `create_end` | `InspectorLedger::result`, at the frame's settlement point |
| Every `Gas` above → `refunded` | every callback that holds one | `InspectorLedger::refund`, at the callback boundary. Nominal: neither the EIP-3529 cap nor the chain of successful frame returns an edit must survive is attributable to one callback, and the lane feeds no identity — so over-stating it costs nothing, while under-stating it would let a rewritten receipt into a block. |
| Every `Gas` above → `reservoir`, and `CallInputs` / `CreateInputs` → `reservoir` | every callback that holds one | `InspectorLedger::reservoir`, settled once from the figure the transaction ends with, and a term of the law because the receipt reports the pool as unspent. `MegaETH` produces none of it, so there is no difference to take; revm propagates it between frames by replacement, so a boundary difference would book edits the EVM goes on to erase. |
| Every `Gas` above → `gas_limit` | every callback that holds one | Inert. op-revm normalises the top-level gas object to the transaction's own limit before the settlement point, and no REX7 lane reads a frame's limit; the two that do are the REX4 legacy stipend's burn and rescue caps, which REX5 mode does not take. |
| Every `Gas` above → `state_gas_spent` | every callback that holds one | `InspectorLedger::state_gas`, settled at the same point. Not a term of the law: it moves the receipt's state-gas figure, not the envelope. Its *other* effect — a failing frame folds it into its caller's pool — arrives inside the reservoir lane, which is read after the fold. |
| Every `Gas` above → `memory` (`MemoryGas`: `words_num`, `expansion_cost`) | every callback that holds one | Not a budget but a memo of how far the frame's memory has been paid for — and the number the next expanding opcode compares its requirement against, so moving it *together with the memory* skips that opcode's charge while leaving every interpreter invariant intact. Booked on `InspectorLedger::interventions`, off `WorkingSet`. |
| `CallInputs` / `CreateInputs` semantic fields, including `charged_new_account_state_gas`; `InterpreterResult::result` and `::output`; `CallOutcome::memory_offset` / `::was_precompile_called` / `::precompile_call_logs` / `::charged_new_account_state_gas`; `CreateOutcome::address` | `frame_start`, `call`, `create`, `frame_end`, `call_end`, `create_end` | Not gas. Booked on `InspectorLedger::interventions` by the rewrite comparison. |
| Every constant-time reading of `Interpreter`'s own fields — `bytecode` (program counter, code identity, `continue_execution`), `stack` (length), `return_data` (buffer identity), `memory` (size, window offset), `gas` → `memory` (the memo), `input` (target, code address, caller, value, calldata identity), `runtime_flag` (static flag, spec id) | the four live-interpreter callbacks | Not gas, and the whole of what a boundary can read off a live interpreter in constant time. Booked on `InspectorLedger::interventions`, off `WorkingSet`. |
| `Interpreter::extend` | the four live-interpreter callbacks | Not gas, and not readable: `InterpreterTypes::Extend` carries no trait bound, so a shim generic over the interpreter has nothing it can call on it. `MegaETH` configures it as `()`. |
| The **contents** of the interpreter's `stack`, `memory`, `return_data` buffer, calldata and code, at unchanged identities | the four live-interpreter callbacks | Not gas. The EVM executes on whatever it finds and meters that as its own work, because it is. |
| `&mut CTX` — the journal, and through `MegaContext`'s `DerefMut` the transaction, the block, the configuration and `MegaETH`'s own trackers | every callback but `selfdestruct` | Not gas the EVM handed over. Unmeasured for the reason the journal is: telling whether any of it came back changed needs a snapshot of unbounded state that no callback boundary can take at a cost the inspected path can carry. The gas schedule is the exception — the schedule pin rejects a rewritten one, at the next transaction rather than within this one. |
| Everything passed by value (`Log`; `selfdestruct`'s three arguments) and the inputs the `*_end` callbacks take by shared reference | — | No mutable reach at all. |

**There are no open rows, and the table cannot be left with one.**
`Coverage::NotClosed` still exists, so a surface that reaches what `MegaETH` reports and that no lane books is nameable — but `tests/rex7/gas_surface.rs::test_the_table_carries_no_open_gap` fails on any row that carries it.
Writing a gap down is how it gets closed; leaving it written down is how a table stops being a statement about the code.
What no test can catch is a gap _mis_-classified as `Inert` or `NotGas`: those verdicts are claims about what the EVM does with a number, and only the measurement each was written from backs them.
There are two cautionary cases, and they failed differently.
`state_gas_spent` sat under "EIP-8037 is off, so nothing reads it", which is true of every instruction and not of the receipt — a wrong verdict.
`MemoryGas` had the right verdict and the wrong reason: "editing this desynchronises it from the memory and the EVM reads out of bounds" is true of each field alone and false of the pair moved together with the memory, which is exactly the rewrite the row was excusing.
A reason that only covers half its own input space is the harder of the two to see, because the row reads as considered.

**What the closure pin does and does not reach.**
A field upstream adds to any of these structs shows up in its `Debug` rendering, matches no row, and fails the test by name.
`Interpreter` itself is one of those structs, which is the row that had been missing: its fields were named in prose, the prose did not say `bytecode`, and an inspector could step the program counter past an instruction with every lane and every counter reading zero.
A variant upstream adds to `InterpreterAction`, `FrameInput` or `FrameResult` fails the build, because the module matches all three exhaustively with no catch-all.
A *callback* upstream adds to the `Inspector` trait does neither — the trait gives every method a default body, so an unimplemented one silently does nothing — which is why the obligation below is written out.

### Rules for changing this

- **Add the shim's counterpart when adding an `Inspector` callback.**
  An unwrapped callback is an unmeasured hole, not a compile error.
  `tests/rex7/inspector_cheat_matrix.rs` enumerates every callback × shape pair and fails on one that is neither covered nor excused, which is what turns a new callback into a red test.
- **On a revm bump, re-read the trait's method list against `tests/rex7/gas_surface.rs`'s `CALLBACKS`, and give any new callback a row in the shape table and a column in the cheat matrix.**
  This is the one direction no pin reaches, and it is the direction that adds reach.
  The field-level and variant-level pins in the same file cover everything else, and both fail loudly on their own.
- **Book a result rewrite from the frame's settlement point, not from the callback boundary.**
  Whether such an edit moves the transaction's envelope depends on how the frame ends: a returning or reverting frame's remaining gas goes back to its caller, a halting one's does not.
  The gas an intercepting callback puts into a synthetic outcome travels through that same lane.
- **Take every constant-time reading of the interpreter, not a chosen list of them.**
  `WorkingSet` is the snapshot the four live-interpreter callbacks are compared across, and the rule it is built on is stated over the *cost* of a reading rather than over a list of interesting ones: if it is `O(1)` off a field of `Interpreter`, it is in the snapshot.
  A list is only as complete as whoever wrote it, and the four-reading list that preceded this rule left `bytecode` out entirely.
  Two tests hold the rule: `tests/rex7/gas_surface.rs` pins `Interpreter`'s field set against upstream's `Debug`, and `inspector.rs`'s own unit tests move each reading in turn and fail if it moves nothing, or if a reading exists that no case moves.
  The snapshot is stated twice — once as `WorkingSet::of`, which records the readings, and once as `WorkingSet::unchanged`, which compares them against a live interpreter without building a second snapshot — and the same unit tests hold the two lists together: each case asserts both that the reading it moved is named and that `unchanged` returns `false`, so a reading in one list and not the other is a reading the shim takes and never compares.
  On a revm bump, re-read the trait methods the snapshot reads through — `Jumps`, `LoopControl`, `LegacyBytecode`, `StackTr`, `ReturnData`, `MemoryTr`, `InputsTr`, `RuntimeFlag` — for a new constant-time accessor, which is a new reading and not a compile error anywhere.
  A reading that would need unbounded work is the one thing the rule does not ask for; it belongs in the contents row, which has no lane.
- **Compare every reading at every callback, not once per frame.**
  The four fields a frame's identity is made of — its target, the address of the code it runs, its caller and its value — together with its calldata identity, its static flag and its spec id, cannot change while it runs, which makes them the readings a cheaper shim would compare once per frame instead of twice per opcode.
  They can change — an inspector writes them — and the shape that exploits a per-frame comparison is an edit made in `step` and undone in `step_end`, which leaves the frame's identity equal to the EVM's at every point outside those two callbacks while the instruction in between reads something else.
  `tests/rex7/ledger_blind_spots.rs::test_a_frame_invariant_moved_and_moved_back_is_booked` is that shape, and it costs the transaction nothing, so no gas lane can stand in for the comparison.
  Making a reading cheaper is free to do; taking it less often needs an argument that this test survives.
- **Book a lane through `Lane::book`, never by writing its net.**
  The gross half is what `is_zero` reads, so a booking that moves only the net is a rewrite the guard admits — and one that cancels against a later booking is exactly the shape that is invisible from the net alone.
- **Keep every rewrite out of a block.**
  Supporting a rewrite is not the same as admitting one: the canonical block-execution path refuses a transaction whose ledger is non-zero, in release builds as well as debug, because an inspector is one node's configuration and its edits reach the receipt.
  That is why a rewrite which moves no gas still has to be booked — on `InspectorLedger::interventions` — or the guard admits it.
  An EVM an embedder drives itself is deliberately not covered: it produces no block, so there is nothing for two nodes to disagree about.
  See `tests/block_executor/inspector_guard.rs`.

## WHERE TO LOOK
- New spec opcode delta: `instructions.rs` (`mini_rex`, `rex`, `rex2`, `rex3`, `rex4`, `rex5`, `rex6`, `rex7` tables; `rex6` still aliases `rex5` and expresses its deltas as `is_enabled` dispatch inside the shared handlers; `rex7` is a standalone checkpoint table built from revm's base table, with the 17 storage / CALL / CREATE / SELFDESTRUCT / not-yet-activated slots inherited from `rex6`, and with 15 volatile `*_checkpoint` handlers plus `gas_checkpoint` registered as rex7-only).
- Volatile access detention trigger changes: `host.rs` and volatile wrappers in `instructions.rs`.
- Call forwarding and stipend interplay: `instructions.rs` + `../limit/storage_call_stipend.rs`.
- New external gas pricing path: `host.rs` gas helper methods.
- Execution result fields exposed to callers: `execution.rs` and `mod.rs::execute_transaction`.
