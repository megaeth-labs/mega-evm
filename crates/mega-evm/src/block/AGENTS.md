# AGENTS.md

## OVERVIEW
Block execution orchestration for MegaETH, including hardfork-to-spec resolution, system pre/post calls, and block-level resource admission.

## STRUCTURE
- `executor.rs`: `MegaBlockExecutor` lifecycle, pre/post execution changes, tx commit policy.
- `factory.rs`: executor factory wiring from hardfork config and EVM factory.
- `hardfork.rs`: `MegaHardfork` definitions, activation checks, spec mapping.
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

## ANTI-PATTERNS
- Do not apply post-execution limit counters before a tx outcome is commit-eligible.
- Do not bypass `pre_execution_changes` in replay or simulation paths that aim for chain equivalence.
- Do not infer spec from tx fields.
- Always derive spec from hardfork activation at block timestamp.
- Do not hardcode gas-limit assumptions outside `BlockLimits` plumbing.

## WHERE TO LOOK
- Add a new hardfork activation condition: `hardfork.rs` and `MegaHardforkConfig` wiring.
- Change tx inclusion behavior under block pressure: `limit.rs` and `executor.rs::run_transaction`/commit methods.
- Add pre-block or post-block system call: `eips.rs` and `executor.rs::{pre_execution_changes,post_execution_changes}`.
- Change block-level default limits for a hardfork: `limit.rs::from_hardfork_and_block_gas_limit`.
- Surface new block execution metadata: `result.rs`.
