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
- `EmptyExternalEnv` must stay deterministic and side-effect free. It answers no oracle slot, so the Oracle's storage is read from the database.
- A read of the Oracle's storage loads the slot through the journal and is priced cold whichever source answered it: a node that replays a block without the oracle service must price and witness it as the node that built it did, so neither the read's price, nor the price of a later write to the slot, nor the witness may depend on whether the service had a value.
- The service's value is returned even over a value the Oracle's frame stored earlier in the transaction.
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
- Change oracle storage retrieval behavior: `oracle.rs` trait impls; the Host reads through them in `evm/host.rs` (`oracle_sload`): the slot loaded through the journal, the service's value when it has one and the loaded value otherwise, always priced cold.
- Change bucket-id mapping logic: `salt.rs` and `hasher/` helpers.
- Change how a bucket capacity becomes a gas multiplier: `gas.rs`. What that multiplier is applied to is the pricing hook in `evm/host.rs`.
