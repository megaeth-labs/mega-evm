# AGENTS.md

## OVERVIEW
Thin CLI front end over the `mega-state-test` runner library (`crates/mega-state-test`), which runs Ethereum's execution-spec state tests on the Satin engine.
This crate is not published; the runner, the modes, the deviation registry and the fixture types all live in `mega-state-test`.

## STRUCTURE
- `src/main.rs`: flag parsing, the printed summary, and the exit-code contract.
- `src/btest.rs`: the `btest` subcommand, the blockchain tests' gate, with the same contract; `main` hands it every argument after `btest`, so the state tests' command is unchanged.
- `tests/cli.rs`: the exit-code contract CI relies on.

## WHERE TO LOOK
- Change CLI flags, the summary or the exit codes: `src/main.rs`, and `src/btest.rs` for the blockchain tests.
- Anything about execution, judging, skips, deviations or fixture types: `crates/mega-state-test` (see its `AGENTS.md`).
