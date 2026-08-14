# AGENTS.md

## OVERVIEW
CLI toolbox for direct MegaEVM execution (`run`, `tx`, `replay`, `cache`) with optional forking, tracing, and state dump workflows.

## STRUCTURE
- `src/main.rs`: CLI bootstrap and panic hook.
- `src/cmd.rs`: top-level command dispatch and error surface.
- `src/common/`: shared CLI args, state loading, tracing, tx parsing, output printers.
- `src/run/`: bytecode execution command.
- `src/tx/`: full transaction execution command with raw-tx override support.
- `src/replay/`: RPC-backed historical transaction replay through block executor, plus the batch driver.
- `src/cache/`: cache-file merge utilities (the single envelope JSON shape every cache file uses) backing the `cache merge` subcommand and the lock-protected merge-on-persist, plus the sidecar advisory lock every cache-file writer takes.

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
- Change how a replay target's endpoint answers are judged coherent (metadata shape, genesis placement, parent linkage, inclusion anchor, block-body membership): `src/replay/coherence.rs` — the single source of those verdicts and their wording, shared by both replay drivers, which adapt them into their own failure shapes.
- Change how a mined block is executed (state fork, body walk, per-target isolation, early stop, receipt harvest): `src/replay/kernel.rs` — the shared execution kernel both replay drivers run their mined targets through. Fetching, coherence guards, entry assembly and error adaptation stay with the driver.
- Change how a pending target is replayed: `src/replay/cmd.rs`, `execute_pending` — deliberately off the kernel, because its one block fills both the fork and the environment role and its metadata is exactly what the online cache refuses to keep.
- Change receipt/summary formatting: `src/common/outcome.rs` and printer helpers.
- Change cache merge behavior (CLI or merge-on-persist): `src/cache/{mod.rs,merge.rs}`.
- Change how cache files are locked against concurrent writers: `src/cache/lock.rs` — the one place a cache-file write may acquire its lock, and every caller must fail closed when it cannot.
- Change process exit classification: `src/common/exit.rs` — the single exit site for command results.
