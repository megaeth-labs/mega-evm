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

### What each rewrite shape costs

Read the table by the *argument the rewrite reaches through*, not by the tool that makes it: two tools editing the same argument are one row.

| Rewrite shape | Support | Where it is booked | What enforcement sees |
| --- | --- | --- | --- |
| Read-only observation | Supported, free | nothing | an empty ledger, and numbers identical to an uninspected run |
| Gas written into a live interpreter's counter (`initialize_interp`, `step`, `step_end`, `log_full`) | Supported | `InspectorLedger::gas`, at the callback boundary | nothing: the checkpoint baseline shifts by the same amount, and the gas clamp is re-derived on the spot so injected gas buys no compute headroom |
| A frame input's `gas_limit`, raised or lowered (`frame_start`, `call`, `create`) | Supported | `InspectorLedger::env`, at the callback boundary | nothing: a frame's compute budget comes from the tracker, not from its gas limit |
| A frame input's semantic fields — target, caller, value, scheme, calldata, static flag (`frame_start`, `call`, `create`) | Supported | `InspectorLedger::interventions`, at the callback boundary | nothing: it changes what the frame does, not what it costs |
| A synthetic outcome that skips the frame entirely (`frame_start`, `call`, `create`) | Supported | `InspectorLedger::interventions`; nothing on the `env` lane — the edited inputs never reach a frame — and the outcome's own gas on the `result` lane | the frame's envelope is settled at `finalize_frame` as `FrameExit::RefusedSynthetically` |
| A finished frame result's remaining gas (`call_end`, `create_end`, `frame_end`) | Supported | `InspectorLedger::result`, at the frame's settlement point rather than at the callback boundary | nothing |
| A finished frame result's returned output (`call_end`, `create_end`, `frame_end`) | Supported | `InspectorLedger::interventions`, at the callback boundary | nothing |
| A successful frame result rewritten into a revert or a halt | Supported | `InspectorLedger::interventions` — no gas moves | the journal decision follows the final result, so the frame's state is rolled back with it; a precompile's executed/destroyed split follows it too |
| A failed **call** frame rewritten into a success | Supported | `InspectorLedger::interventions` | the journal commits, so the frame's state follows the result its caller was handed |
| A failed **contract creation** rewritten into a success | **Refused** | `InspectorLedger::rejected_rewrites`, alongside `interventions` | `reject_forbidden_create_rewrite` restores the original classification and fails the transaction with `EVMError::Custom`; debug builds assert |
| The interpreter's stack or memory | Supported, unmeasured | nothing — no argument the shim holds describes it | the EVM executes on the edited state and meters it as its own work, because it is |
| A direct journal write (`tstore`, `log`, …) | Supported, unmetered | nothing — no argument the shim holds describes it | `MegaETH`'s data-size / KV / state-growth lanes do not see it; it moves no gas, so the conservation law is unaffected |
| The pending `InterpreterAction`, reached through `LoopControl` | Supported, unmeasured | nothing | an edit to the action's gas does move the envelope; measuring it at the callback boundary would be unsound, because whether it moves anything depends on the frame's final classification |
| `CfgEnv` or the active gas schedule | **Refused** | — | the gas-schedule pin panics; the schedule belongs to the spec, and a rewritten one has no accounting lane that could rescue it |

Two independent stops back the creation refusal: the shim restores the classification, and `frame.rs`'s `FrameJournalVerdict::CreateRejected` carries no code and no commit branch, so even with the refusal removed such a rewrite deposits nothing.

Booking is a *reported* quantity throughout. No resource limit is ever compared against the ledger, and `MegaTransactionOutcome::compute_gas_enforced` comes off the enforcement lane rather than out of the reported total — so an inspector cannot buy a transaction headroom on any dimension.

### The window a counter edit reaches nothing through

A gas-counter edit made while the interpreter is already holding a `Return` action is written into an object nobody reads again: revm's inspected loop runs `step_end` after the instruction that set the action, and the action carries its own snapshot of the gas, which is what becomes the frame's result.
The shim books nothing for such an edit and still shifts the settlement baseline for it — `MegaETH`'s tail settlement reads the counter after the action is set, so without the shift the edit would read as work the frame performed.
The predicate is the pending action's variant, not "the loop is ending": a `NewFrame` action ends the loop too, and that frame resumes on exactly this counter.

### Rules for changing this

- **Add the shim's counterpart when adding an `Inspector` callback.**
  An unwrapped callback is an unmeasured hole, not a compile error.
  `tests/rex7/inspector_cheat_matrix.rs` enumerates every callback × shape pair and fails on one that is neither covered nor excused, which is what turns a new callback into a red test.
- **Book a result rewrite from the frame's settlement point, not from the callback boundary.**
  Whether such an edit moves the transaction's envelope depends on how the frame ends: a returning or reverting frame's remaining gas goes back to its caller, a halting one's does not.
  The gas an intercepting callback puts into a synthetic outcome travels through that same lane.
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
