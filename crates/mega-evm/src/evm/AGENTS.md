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
- A transaction that unwinds with a panic skips the handler's cleanup; `MegaEvm::discard_tx` (`mod.rs`) is the embedder's reset for that case and `MegaContext::discard_tx` (`context.rs`) its `MegaETH` half.
  A new per-transaction field on `MegaContext` must be reset there unless every transaction overwrites it before reading it.
- `discard_tx` clears the entire journal, including any unfinalized `transact_one` / `transact_many` history.
- A call that fails with an error leaves no writes behind on any entry point.
  `MegaHandler::catch_error` delegates journal cleanup to op-revm and finishes it if failed-deposit recovery itself returns an error.
  It also clears the context's stashed database error, which the handler takes only when it builds the result, so an error returned before that (from post-execution, say) does not fail the next transaction.
  The exception is the pre-REX5 keyless-deploy sandbox merge, which writes the state map without journal entries (frozen behavior); only a finalize drops it, so a `_one` caller keeps it after an error.
  Every Mega entry point that finalizes the journal on success also finalizes it on error.
  The revm 43 defaults for `transact_commit`, `inspect`, `inspect_tx`, `inspect_tx_commit`, and `inspect_commit` finalize on error.
  Mega's implementations in `interfaces.rs` and its required `replay` implementation preserve that contract.
  The upstream `system_call_with_caller` default exits early on error, while Mega's override and its inherent `transact_system_call_with_gas_limit` finalize on both outcomes.
  When a revm upgrade adds a finalizing or committing entry point, check its error cleanup and cover it in the entry-point table in `tests/block_executor/discard_transaction.rs`.
- State the next transaction inherits (the pre-REX1 detained compute gas limit) is recorded by `MegaContext::on_tx_end`, which the handler runs at the end of every run (`execution_result` and `catch_error`), so a discard restores what the previous transaction left even when the panic landed before `on_new_tx`.

## WHERE TO LOOK
- New spec opcode delta: `instructions.rs` (`mini_rex`, `rex`, `rex2`, `rex3`, `rex4`, `rex5`, `rex6` tables; `rex6` aliases its predecessor, expressing its deltas as `is_enabled` dispatch inside the shared handlers).
- Volatile access detention trigger changes: `host.rs` and volatile wrappers in `instructions.rs`.
- Call forwarding and stipend interplay: `instructions.rs` + `../limit/storage_call_stipend.rs`.
- New external gas pricing path: `host.rs` gas helper methods.
- Execution result fields exposed to callers: `execution.rs` and `mod.rs::execute_transaction`.
- State left behind by a panicked transaction: `mod.rs::discard_tx` and `context.rs::discard_tx`, pinned line by line in `tests/block_executor/discard_transaction.rs`.
