# AGENTS.md

## OVERVIEW
External dependency abstraction for block-scoped SALT and oracle data consumed during EVM execution.

## STRUCTURE
- `mod.rs`: `ExternalEnvTypes`, `ExternalEnvs`, and `EmptyExternalEnv` defaults.
- `factory.rs`: `ExternalEnvFactory` trait for block-scoped environment creation.
- `salt.rs`: SALT trait and bucket-id derivation rules.
- `oracle.rs`: oracle trait for storage reads and hint side effects.
- `gas.rs`: `BucketMultipliers`, the per-transaction cache of bucket multipliers the state-gas price hook reads `SaltEnv` through.
- `hasher/`: hashing utilities used for deterministic bucket-id computation.

## KEY PATTERNS
- Block context is captured at environment creation time, not passed per query.
- SALT and oracle are independent traits but consumed together via `ExternalEnvs` bundle.
- External errors are propagated to host and then stashed in EVM context error channel.
- `EmptyExternalEnv` must stay deterministic and side-effect free.
- A bucket's capacity is read once per transaction and answered from `BucketMultipliers` afterwards, so a state gas charge and the refill that undoes it are priced at the same capacity by construction.
- `BucketMultipliers` does not hold the environment; it takes a `&SaltEnv` per call, so there is no second copy of it and no `Clone` bound on the engine's environment types.
- `MIN_BUCKET_SIZE` is the smallest capacity a backend may report.
  A report below it is a broken backend, not a cheap bucket: it is a failed lookup (`BucketError::BelowMinimum`) and fails the transaction, rather than being rounded up to `m = 1` or priced at `m = 0`.

## ANTI-PATTERNS
- Do not query live chain state directly from opcode handlers.
- All external reads should route through trait objects created by `ExternalEnvFactory`.
- Do not couple oracle hint side effects to SALT behavior.
- Keep SALT and oracle traits independently testable.

## WHERE TO LOOK
- Add a new external backend implementation: implement `SaltEnv`/`OracleEnv` and an `ExternalEnvFactory`.
- Change oracle storage retrieval behavior: `oracle.rs` trait impls (the host integration is not here yet).
- Change bucket-id mapping logic: `salt.rs` and `hasher/` helpers.
- Change how a bucket capacity becomes a gas multiplier: `gas.rs`. What that multiplier is applied to is the pricing hook in `evm/host.rs`.
