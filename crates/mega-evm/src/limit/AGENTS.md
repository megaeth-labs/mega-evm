# AGENTS.md

## OVERVIEW
Resource metering subsystem for transaction and frame limits across compute gas, data size, KV updates, and state growth.

## STRUCTURE
- `limit.rs`: `AdditionalLimit` coordinator and frame/tx lifecycle hooks.
- `destroyed.rs`: closed `InstructionResult` classification for the destroyed-remainder protocol (swallow / return / unreachable, no catch-all) and the producer × accounting-site table a revm bump diffs against.
- `compute_gas.rs`: compute gas tracking, detention limits, frame budgets.
- `data_size.rs`: tx/frame data accounting with revert-aware discard paths.
- `kv_update.rs`: tx/frame KV accounting with revert-aware discard paths.
- `state_growth.rs`: net-new account/slot growth accounting.
- `frame_limit.rs`: generic 98/100 frame-limit tracker utilities.
- `storage_call_stipend.rs`: dual-mode stipend — REX5+ separated allowance drained at `storage_gas_ext` sites; REX4 legacy inflation with compute cap and burn-on-return.
- `mod.rs`: `LimitKind`, `LimitCheck`, revert-data ABI surface.

## KEY PATTERNS
- Limit-check order is deterministic and shared by all opcode paths.
- Distinguish TX-level exceed (halt/OutOfGas) from frame-local exceed (revert).
- All trackers push/pop per-frame in lockstep with EVM frame lifecycle hooks.
- `AdditionalLimit::finalize_frame` is the single point a frame's outcome is settled — the destroyed-remainder booking, the frame-init refusal booking, the gas rescue, and the REX7 frame-local absorb — and it runs after the last callback that can rewrite the frame's classification and before the journal decision.
  Put a new frame-exit settlement there, not in a lifecycle hook on either side of it.
  The pops stay in `before_frame_return_result`: the paths that reach a caller without ever running a frame would double-pop.
- A frame result's remaining gas is swallowed or returned by `destroyed_disposition`, not by `is_ok_or_revert()`.
  Every `InstructionResult` variant has an arm; a new variant is a compile error until it is classified.
  All four readers are inside `finalize_frame`: the three destroyed bookings plus the inspector-edit split, which asks the same question about the same remainder.
  A site that only mirrors an upstream branch keyed on `is_ok_or_revert()` — the precompile dispatch undoing revm's `spend_all()` and rebuilding the `Gas` object revm's refund logic reads — keeps upstream's predicate, because it has to move with upstream rather than with our classification.
  A new destroyed-remainder *producer* belongs on the table in `destroyed.rs`, with its own accounting site.
  The early-fail arms of `make_call_frame` / `make_create_frame` / `classify_create_return` are a second closed set with no type-level tie — diff them by hand on a revm bump against `tests/rex7/result_space_tripwire.rs`.
- A frame-local exceed a frame could not latch — the one defined against its *caller's* budget after the merge — is settled in `before_frame_return_result` instead, and under REX7 before the pops rather than after them.
  `peek_check_limit_after_pop` answers the post-merge question over `FrameLimitTracker::view_after_pop`, so the reading is the merged one and only the timing moves; the pop that follows reads a revert and discards the frame's usage.
  Every dimension answers it with its own `check_limit` body over a `FrameLimitView`, and the two readings are cross-checked against each other on every frame return in debug builds.
  Add a dimension's `check_limit_after_pop` when adding a dimension, and extend `view_after_pop` when adding a lane the pop moves.
- Synthetic frame results still require empty-frame pushes for stack alignment.
- Gas rescue must exclude any system-granted stipend gas.
- Revert paths must roll back discardable usage for data/KV/state growth trackers.

## ANTI-PATTERNS
- Do not introduce per-frame gas grants without explicit unwind-on-return behavior.
- Do not encode frame-local exceeds as halts.
- They must be reverts with bounded payload.
- Do not read tracker totals after an exceeded-limit revert path unless using tracker-owned finalized APIs.
- Do not run a fresh `check_limit()` inside `finalize_frame`: a per-frame exceed is defined by the frame's usage weighed against its *caller's* budget after the merge, which nothing at that point can read.
  The pre-pop settlement in `before_frame_return_result` is where that question belongs, and it reads the merged numbers rather than the current ones.
- Avoid duplicating limit checks inside opcode handlers when the tracker already enforces the same dimension.

## WHERE TO LOOK
- New limit dimension: add tracker module, wire into `AdditionalLimit`, extend `LimitKind`/`LimitCheck`.
- Change exceed semantics or revert data: `mod.rs` and `limit.rs` helper builders.
- Change compute detention behavior: `compute_gas.rs` and detention callers in `evm` module.
- Change frame budget forwarding logic: `frame_limit.rs` and each tracker’s frame hooks.
- Change storage call stipend semantics: `storage_call_stipend.rs` and `limit.rs` integration points.
- Classify a new `InstructionResult` variant or add a destroyed-remainder producer: `destroyed.rs` plus `tests/rex7/result_space_tripwire.rs`.
