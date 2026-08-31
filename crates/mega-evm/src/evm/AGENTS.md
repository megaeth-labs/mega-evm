# AGENTS.md

## OVERVIEW
MegaEVM execution core that wraps revm/op-revm with MegaETH instruction tables, host behavior, and execution interfaces.

## STRUCTURE
- `mod.rs`: `MegaEvm` wrapper, inspector toggling, execution convenience APIs.
- `context.rs`: execution context composition and state wiring.
- `execution.rs`: transaction execution flow and result shaping.
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
- Every inspector is wrapped in `MeasuredInspector` (`inspector.rs`) before it reaches the inner EVM; the public accessors hand the unwrapped one back, so the type a caller names is unchanged.
  The shim snapshots the interpreter's gas counter and a frame input's `gas_limit` across each callback and books the difference — the EVM does not execute inside a callback, so anything that moves across one is the inspector's.
  A gas-counter edit is kept out of REX7 compute accounting (the checkpoint baseline is shifted by it) and out of the compute headroom (the gas clamp is re-derived immediately).
  A raised frame `gas_limit` is booked as conjured gas so the destroyed-remainder derivation still balances.
  Add the shim's counterpart when adding an `Inspector` callback: an unwrapped callback is an unmeasured hole, not a compile error.
- What the shim does **not** yet cover, because the answer depends on a frame's final classification and that is not settled until after the last mutating callback: a `call_end` / `create_end` that rewrites a frame result's gas or its classification, and the gas an intercepting callback puts into a synthetic outcome.
  Those still move REX7 accounting off the uninspected path, and a large enough move trips the conservation `debug_assert`.
  One shape is refused outright rather than left to diverge: a `create_end` (or the `frame_end` after it) turning a failed creation into a successful one — see `reject_forbidden_create_rewrite`.

## WHERE TO LOOK
- New spec opcode delta: `instructions.rs` (`mini_rex`, `rex`, `rex2`, `rex3`, `rex4`, `rex5`, `rex6`, `rex7` tables; `rex6` still aliases `rex5` and expresses its deltas as `is_enabled` dispatch inside the shared handlers; `rex7` is a standalone checkpoint table built from revm's base table, with the 17 storage / CALL / CREATE / SELFDESTRUCT / not-yet-activated slots inherited from `rex6`, and with 15 volatile `*_checkpoint` handlers plus `gas_checkpoint` registered as rex7-only).
- Volatile access detention trigger changes: `host.rs` and volatile wrappers in `instructions.rs`.
- Call forwarding and stipend interplay: `instructions.rs` + `../limit/storage_call_stipend.rs`.
- New external gas pricing path: `host.rs` gas helper methods.
- Execution result fields exposed to callers: `execution.rs` and `mod.rs::execute_transaction`.
