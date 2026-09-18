# AGENTS.md

## OVERVIEW
External dependency abstraction for block-scoped SALT and oracle data consumed during EVM execution.

## STRUCTURE
- `mod.rs`: `ExternalEnvTypes`, `ExternalEnvs`, and `EmptyExternalEnv` defaults.
- `factory.rs`: `ExternalEnvFactory` trait for block-scoped environment creation.
- `salt.rs`: SALT trait and bucket-id derivation rules.
- `oracle.rs`: oracle trait for storage reads and hint side effects.
- SALT pricing (the state-gas price hook that consumes `SaltEnv`) is not here yet; T3.2 adds it.
- `hasher/`: hashing utilities used for deterministic bucket-id computation.

## KEY PATTERNS
- Block context is captured at environment creation time, not passed per query.
- SALT and oracle are independent traits but consumed together via `ExternalEnvs` bundle.
- External errors are propagated to host and then stashed in EVM context error channel.
- `EmptyExternalEnv` must stay deterministic and side-effect free.

## ANTI-PATTERNS
- Do not query live chain state directly from opcode handlers.
- All external reads should route through trait objects created by `ExternalEnvFactory`.
- Do not couple oracle hint side effects to SALT behavior.
- Keep SALT and oracle traits independently testable.

## WHERE TO LOOK
- Add a new external backend implementation: implement `SaltEnv`/`OracleEnv` and an `ExternalEnvFactory`.
- Change oracle storage retrieval behavior: `oracle.rs` trait impls (the host integration arrives with T6.3).
- Change bucket-id mapping logic: `salt.rs` and `hasher/` helpers.
