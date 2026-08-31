# AGENTS.md

## OVERVIEW
Resource metering subsystem for transaction and frame limits across compute gas, data size, KV updates, and state growth.

## STRUCTURE
- `limit.rs`: `AdditionalLimit` coordinator and frame/tx lifecycle hooks.
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
- Synthetic frame results still require empty-frame pushes for stack alignment.
- Gas rescue must exclude any system-granted stipend gas.
- Revert paths must roll back discardable usage for data/KV/state growth trackers.

## ANTI-PATTERNS
- Do not introduce per-frame gas grants without explicit unwind-on-return behavior.
- Do not encode frame-local exceeds as halts.
- They must be reverts with bounded payload.
- Do not read tracker totals after an exceeded-limit revert path unless using tracker-owned finalized APIs.
- Do not run a fresh `check_limit()` inside `finalize_frame`: a per-frame exceed is defined by the frame's usage weighed against its *caller's* budget after the merge, which is only answerable once the frame is back with its caller.
- Avoid duplicating limit checks inside opcode handlers when the tracker already enforces the same dimension.

## WHERE TO LOOK
- New limit dimension: add tracker module, wire into `AdditionalLimit`, extend `LimitKind`/`LimitCheck`.
- Change exceed semantics or revert data: `mod.rs` and `limit.rs` helper builders.
- Change compute detention behavior: `compute_gas.rs` and detention callers in `evm` module.
- Change frame budget forwarding logic: `frame_limit.rs` and each tracker’s frame hooks.
- Change storage call stipend semantics: `storage_call_stipend.rs` and `limit.rs` integration points.
