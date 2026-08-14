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
  That alignment assumes the inspector does not rewrite a frame result and does not edit the interpreter gas counter.
  A rewriting inspector — one that changes `CallOutcome` / `CreateOutcome` in `call_end` / `create_end`, or that spends or refunds interpreter gas in `step` / `step_end` — will make REX7 compute accounting diverge: the exceptional-halt burn split is settled before `frame_end`, and the plain-segment delta is measured from the interpreter counter, so either edit moves the reported `compute_gas_used` / `compute_gas_destroyed` off the uninspected path.
  Observational inspectors (`NoOpInspector`, tracers that only read) stay aligned.

## WHERE TO LOOK
- New spec opcode delta: `instructions.rs` (`mini_rex`, `rex`, `rex2`, `rex3`, `rex4`, `rex5`, `rex6`, `rex7` tables; `rex6` still aliases `rex5` and expresses its deltas as `is_enabled` dispatch inside the shared handlers; `rex7` is a standalone checkpoint table built from revm's base table, with the 17 storage / CALL / CREATE / SELFDESTRUCT / not-yet-activated slots inherited from `rex6`, and with 15 volatile `*_checkpoint` handlers plus `gas_checkpoint` registered as rex7-only).
- Volatile access detention trigger changes: `host.rs` and volatile wrappers in `instructions.rs`.
- Call forwarding and stipend interplay: `instructions.rs` + `../limit/storage_call_stipend.rs`.
- New external gas pricing path: `host.rs` gas helper methods.
- Execution result fields exposed to callers: `execution.rs` and `mod.rs::execute_transaction`.
