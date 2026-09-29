# AGENTS.md

## OVERVIEW
CLI toolbox for direct MegaEVM execution (`run`, `tx`, `replay`) with optional forking, tracing, and state dump workflows.
Two engines: the in-tree sources run Satin; a spec from `Equivalence` to `Rex6` runs on the released `mega-evme` 1.7.1 and `mega-evm` 1.7.1, linked as `mega-evme-legacy` / `mega-evm-legacy` (feature `legacy`, on by default).

## STRUCTURE
- `src/main.rs`: CLI bootstrap and panic hook.
- `src/cmd.rs`: argument parsing, engine selection, and dispatch; a legacy spec's arguments are handed unchanged to the 1.7.1 CLI.
- `src/engine.rs`: which engine a spec or a block runs on; the hand-off to the legacy CLI.
- `src/common/`: shared CLI args, state loading, tracing, tx parsing, output printers.
- `src/run/`: bytecode execution command.
- `src/tx/`: full transaction execution command with raw-tx override support.
- `src/replay/`: RPC-backed historical transaction replay through block executor (Satin; a legacy spec is the 1.7.1 CLI's).
- `src/block/`: `replay --block`, whole blocks on either engine compared with the chain: inputs and the block cache (`inputs.rs`), the parent state in plain data (`state.rs`), one executor per engine exchanging only plain data (`satin.rs`, `legacy.rs`), records and the comparison (`record.rs`), the driver (`cmd.rs`).
- `tests/satin-differences.md`: the pinned differences between the engines, rendered by `tests/differences.rs` (`UPDATE_EVME_DIFFERENCES=1` rewrites it).

## KEY PATTERNS
- Shared argument groups are flattened from `run` argument structs into sibling commands.
- Command handlers follow staged flow: parse inputs → build state/env → execute → print summary/receipt/trace.
- Replay uses block executor flow, including pre-execution system calls and preceding transactions.
- Logging is structured via tracing macros, with explicit progress milestones.
- Output paths keep both human-readable summaries and optional machine artifacts (trace/state dump).

- Output on Satin is additive to the legacy output: every legacy field keeps its name and meaning, and what only Satin counts goes in the `satin` object (`tests/integration.rs` checks it on every fixture pair).

## ANTI-PATTERNS
- Do not edit or re-implement the legacy leg: it is the released 1.7.1 code, pinned with its dependency versions (`tests/legacy_line.rs`).
- Do not use `cargo ... -p mega-evme`: the name also matches the linked 1.7.1 package. Use `--manifest-path bin/mega-evme/Cargo.toml`.
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
