# AGENTS.md

## OVERVIEW
CLI toolbox for direct MegaEVM execution (`run`, `tx`, `replay`, `t8n`) with optional forking, tracing, and state dump workflows.

## STRUCTURE
- `src/main.rs`: CLI bootstrap and panic hook.
- `src/cmd.rs`: top-level command dispatch and error surface.
- `src/common/`: shared CLI args, state loading, tracing, tx parsing, output printers.
- `src/run/`: bytecode execution command.
- `src/tx/`: full transaction execution command with raw-tx override support.
- `src/replay/`: RPC-backed historical transaction replay through block executor.
- `src/t8n/`: state transition tooling and IO helpers.

## KEY PATTERNS
- Shared argument groups are flattened from `run` argument structs into sibling commands.
- Command handlers follow staged flow: parse inputs → build state/env → execute → print summary/receipt/trace.
- Replay uses block executor flow, including pre-execution system calls and preceding transactions.
- Logging is structured via tracing macros, with explicit progress milestones.
- Output paths keep both human-readable summaries and optional machine artifacts (trace/state dump).

## ANTI-PATTERNS
- Do not duplicate chain/spec parsing logic across commands.
- Add shared parsing in `src/common/` and reuse.
- Do not print partial execution output before final outcome object assembly.
- Keep receipt/summary/trace emission in the output step.
- Do not mutate command-level defaults in one subcommand without mirroring related aliases/help text.

## WHERE TO LOOK
- Add a new top-level command: `src/cmd.rs` enum + module wiring in `src/main.rs`.
- Add a new shared CLI option family: `src/common/*` and flatten into command structs.
- Change state-forking or prestate merge semantics: `src/common/state.rs`.
- Change replay hardfork/spec selection: `src/replay/{cmd.rs,hardforks.rs}`.
- Change receipt/summary formatting: `src/common/outcome.rs` and printer helpers.
