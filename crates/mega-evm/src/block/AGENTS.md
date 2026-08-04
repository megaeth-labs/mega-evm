# AGENTS.md

## OVERVIEW
Block execution orchestration for MegaETH, including hardfork-to-spec resolution, system pre/post calls, and block-level resource admission.

## STRUCTURE
- `executor.rs`: `MegaBlockExecutor` lifecycle, pre/post execution changes, tx commit policy.
- `factory.rs`: executor factory wiring from hardfork config and EVM factory.
- `hardfork.rs`: `MegaHardfork` definitions, activation checks, spec mapping.
- `chain.rs`: canonical chain IDs and per-chain hardfork activation schedules (mainnet, testnet, all-activated fallback for unknown chains).
- `limit.rs`: `BlockLimits` config and `BlockLimiter` pre/post checks.
- `eips.rs`: EIP system calls (blockhashes, beacon root, balance increments).
- `helpers.rs`: utility helpers for block execution.
- `result.rs`: block execution result types.

## KEY PATTERNS
- Pre-execution and post-execution limits are intentionally separated.
- Pre-checks reject/skip before execution.
- Post-checks can drop outcomes before commit.
- System contract deployments are idempotent state patches and are hardfork-gated.
- Executor constructor asserts hardfork/spec coherence for non-test builds.
- Block limiter state is cumulative and must be updated only on committed outcomes.
- `pre_execution_changes` collects `Option<EvmState>` outcomes from each helper into a vector; `commit_system_call_outcomes` walks them and calls `system_caller.on_state(source, &state)` **before** `db.commit(state)` for every entry. The `on_state` hook feeds the stateless witness generator with the complete read/write set. Helpers must therefore return all accounts and slots they touched (including reads). See `crates/mega-evm/src/system/AGENTS.md` → `PRE-BLOCK STATE CHANGE CONTRACT` for the helper-side contract.

## ANTI-PATTERNS
- Do not apply post-execution limit counters before a tx outcome is commit-eligible.
- Do not bypass `pre_execution_changes` in replay or simulation paths that aim for chain equivalence.
- Do not infer spec from tx fields.
- Gate on the one resolved spec through the right projection: `spec.is_enabled(MegaSpecId::X)` compares BEHAVIOR (both sides project through `behavior()`, so alias windows roll semantics back), `spec.reaches(MegaSpecId::X)` compares POSITION (one-way setup; alias windows do not retract it). The `is_<fork>_active_at_timestamp` predicates are position projections for behavior-introducing forks and raw event queries for alias forks (`is_mini_rex_1/2_active_at_timestamp`), whose occurrence is not recoverable from the ladder; `mega_fork_activation` answers raw scheduling for any fork.
- A published chain schedule must pass `MegaHardforks::validate_schedule` (rung gaps, activation ordering, required per-fork params). The execution layer stays tolerant of malformed schedules — position-compared setup stays additive — but that tolerance is the fail-safe, not permission to publish one; `hardfork_schedule` debug-asserts it and node startup should check it. A new `HardforkParams` type must be registered in `validate_schedule`.
- One spec, two projections. `spec_id` is monotone (forks map 1:1 onto an ascending ladder; rollbacks are alias rungs like `MINI_REX_1`). Behavior (`is_enabled`) gates execution semantics: EVM behavior, block limits, transaction classification — it rolls back inside an alias window. Position (`reaches`) gates one-way chain setup: system-contract predeploys, pre-block rules, expected installed bytecode versions — retracting it during a rollback window would drop the Oracle predeploys' read-only witness entries that the on-state hook feeds to stateless proofs and the state-sync transition shard.
- Do not express "a chain running spec N" as `with_all_activated().without(fork)`. Removing a middle rung leaves later forks active, so the resolved spec stays at the top of the ladder. Use `with_all_activated_through(MegaSpecId::N)`.
- Do not hardcode gas-limit assumptions outside `BlockLimits` plumbing.
- Do not commit outcomes without first firing `on_state`. The two-step `on_state` → `commit` ordering is the witness-recorder contract; swapping or skipping it corrupts stateless proofs.

## WHERE TO LOOK
- Add a new hardfork activation condition: `hardfork.rs` and `MegaHardforkConfig` wiring.
- Change when a fork activates on mainnet/testnet (or the unknown-chain fallback): `chain.rs`.
- Change tx inclusion behavior under block pressure: `limit.rs` and `executor.rs::run_transaction`/commit methods.
- Add pre-block or post-block system call: `eips.rs` and `executor.rs::{pre_execution_changes,post_execution_changes}`.
- Change block-level default limits for a hardfork: `limit.rs::from_hardfork_and_block_gas_limit`.
- Surface new block execution metadata: `result.rs`.
