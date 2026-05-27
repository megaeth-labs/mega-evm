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
- `storage_call_stipend.rs`: Rex4 stipend grant, cap, and burn-on-return logic.
- `mod.rs`: `LimitKind`, `LimitCheck`, revert-data ABI surface.

## KEY PATTERNS
- Limit-check order is deterministic and shared by all opcode paths.
- Distinguish TX-level exceed (halt/OutOfGas) from frame-local exceed (revert).
- All trackers push/pop per-frame in lockstep with EVM frame lifecycle hooks.
- Synthetic frame results still require empty-frame pushes for stack alignment.
- Gas rescue must exclude any system-granted stipend gas.
- Revert paths must roll back discardable usage for data/KV/state growth trackers.

## ANTI-PATTERNS
- Do not introduce per-frame gas grants without explicit unwind-on-return behavior.
- Do not encode frame-local exceeds as halts.
- They must be reverts with bounded payload.
- Do not read tracker totals after an exceeded-limit revert path unless using tracker-owned finalized APIs.
- Avoid duplicating limit checks inside opcode handlers when the tracker already enforces the same dimension.

## WHERE TO LOOK
- New limit dimension: add tracker module, wire into `AdditionalLimit`, extend `LimitKind`/`LimitCheck`.
- Change exceed semantics or revert data: `mod.rs` and `limit.rs` helper builders.
- Change compute detention behavior: `compute_gas.rs` and detention callers in `evm` module.
- Change frame budget forwarding logic: `frame_limit.rs` and each tracker’s frame hooks.
- Change storage call stipend semantics: `storage_call_stipend.rs` and `limit.rs` integration points.
