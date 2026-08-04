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
- `pre_execution_changes` collects `Option<EvmState>` outcomes from each helper into a vector; `commit_system_call_outcomes` walks them and calls `db.commit(state)` for every entry. The state hook that feeds the stateless witness generator lives on the revm `State` database (`State::set_state_hook`) and fires from inside `DatabaseCommit::commit`, so committing is what records the outcome. Helpers must therefore return all accounts and slots they touched (including reads). See `crates/mega-evm/src/system/AGENTS.md` → `PRE-BLOCK STATE CHANGE CONTRACT` for the helper-side contract.
- `MegaSystemCallOutcome.source` no longer reaches the hook (which sees only the state diff); it is retained for in-crate use.
- Commit-time block-limit re-validation: `commit_tx_result` / `commit_transaction_outcome` re-run `BlockLimiter::pre_execution_check` and return `Err` without committing anything, because another transaction may have filled the block between execute and commit. The infallible `BlockExecutor::commit_transaction` cannot return that error, so it latches it into `pending_commit_error` and `finish` fails the block with it.

## ANTI-PATTERNS
- Do not apply post-execution limit counters before a tx outcome is commit-eligible.
- Do not bypass `pre_execution_changes` in replay or simulation paths that aim for chain equivalence.
- Do not infer spec from tx fields.
- Always derive spec from hardfork activation at block timestamp.
- Do not hardcode gas-limit assumptions outside `BlockLimits` plumbing.
- Do not apply pre-block state changes by any route other than `db.commit`. The witness recorder hooks `commit`, so a change written around it is invisible to stateless proofs.
- Do not `expect`/`unwrap` a commit-time limit re-validation. It fails on a legitimate parallel-execution race, not on a broken invariant.

## WHERE TO LOOK
- Add a new hardfork activation condition: `hardfork.rs` and `MegaHardforkConfig` wiring.
- Change when a fork activates on mainnet/testnet (or the unknown-chain fallback): `chain.rs`.
- Change tx inclusion behavior under block pressure: `limit.rs` and `executor.rs::run_transaction`/commit methods.
- Add pre-block or post-block system call: `eips.rs` and `executor.rs::{pre_execution_changes,post_execution_changes}`.
- Change block-level default limits for a hardfork: `limit.rs::from_hardfork_and_block_gas_limit`.
- Surface new block execution metadata: `result.rs`.
