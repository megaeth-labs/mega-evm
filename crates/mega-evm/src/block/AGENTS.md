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
- Gate on a resolved spec value (`spec.is_enabled(MegaSpecId::X)`), resolved once from the block timestamp. The `is_<fork>_active_at_timestamp` predicates for spec-introducing forks are projections of the activated-spec floor — gating on them is therefore additive-by-construction too, but they cannot express the executing spec, and only `mega_fork_activation` answers whether a fork's activation event itself was scheduled (the patch-fork predicates `is_mini_rex_1/2_active_at_timestamp` stay event queries for exactly that reason).
- A published chain schedule must pass `MegaHardforks::validate_schedule` (rung gaps, activation ordering, required per-fork params). The execution layer stays tolerant of malformed schedules — the floor keeps setup additive — but that tolerance is the fail-safe, not permission to publish one; `hardfork_schedule` debug-asserts it and node startup should check it. A new `HardforkParams` type must be registered in `validate_schedule`.
- Pick the right one of the two resolved specs. `spec_id` is reversible (a patch hardfork may map back to an earlier spec, as `MiniRex1` does) and gates execution semantics: EVM behavior, block limits, the executor's spec-coherence assert, transaction classification. `max_activated_spec_id` is monotone and gates one-way chain setup: system-contract predeploys, pre-block system calls, expected installed bytecode versions. A spec rollback does not un-deploy a predeploy, so gating setup on `spec_id` would retract it for the duration of the rollback window — and with it the read-only witness entries the on-state hook feeds to stateless proofs and the state-sync transition shard.
- Do not express "a chain running spec N" as `with_all_activated().without(fork)`. Removing a middle rung leaves later forks active, so both the executing spec and the activated-spec floor stay at the top of the ladder. Use `with_all_activated_through(MegaSpecId::N)`.
- Do not hardcode gas-limit assumptions outside `BlockLimits` plumbing.
- Do not commit outcomes without first firing `on_state`. The two-step `on_state` → `commit` ordering is the witness-recorder contract; swapping or skipping it corrupts stateless proofs.

## WHERE TO LOOK
- Add a new hardfork activation condition: `hardfork.rs` and `MegaHardforkConfig` wiring.
- Change when a fork activates on mainnet/testnet (or the unknown-chain fallback): `chain.rs`.
- Change tx inclusion behavior under block pressure: `limit.rs` and `executor.rs::run_transaction`/commit methods.
- Add pre-block or post-block system call: `eips.rs` and `executor.rs::{pre_execution_changes,post_execution_changes}`.
- Change block-level default limits for a hardfork: `limit.rs::from_hardfork_and_block_gas_limit`.
- Surface new block execution metadata: `result.rs`.
