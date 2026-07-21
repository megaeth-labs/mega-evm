# AGENTS.md

## OVERVIEW
Thin CLI front-end over the `mega-state-test` runner library (`crates/mega-state-test`), used to run Ethereum state tests against MegaEVM.
This crate is not published; the runner, fixture types, and validation logic all live in `mega-state-test`.

## STRUCTURE
- `src/main.rs`: CLI entrypoint, flag parsing, and path-level orchestration.

## WHERE TO LOOK
- Change CLI flags or path handling: `src/main.rs`.
- Anything about test execution, validation, skip policy, or fixture types: `crates/mega-state-test` (see its `AGENTS.md`).
