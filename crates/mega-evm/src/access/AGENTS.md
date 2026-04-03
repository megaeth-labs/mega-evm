# AGENTS.md

## OVERVIEW
Volatile-data access tracking primitives used to enforce compute-gas detention and Rex4 subtree-level volatile access disable.

## STRUCTURE
- `mod.rs`: public module exports.
- `tracker.rs`: mutable tracker state, cap merge logic, and depth-scoped disable semantics.
- `volatile.rs`: bitflags and typed conversions for volatile access categories.

## KEY PATTERNS
- Tracker stores both a bitmap of accessed categories and the most restrictive raw detention cap.
- Cap aggregation is order-independent via min-cap merge.
- Disable scope is depth-based.
- `disable_access(depth)` keeps the shallower depth if already active.
- `enable_access(caller_depth)` only succeeds when caller is at or above activation depth.
- `enable_access_if_returning(current_depth)` prevents disable leakage into sibling frames.
- Reset clears access state and disable depth, but preserves configured cap parameters.

## ANTI-PATTERNS
- Do not store spec-dependent effective detained limits in this module.
- This module stores raw caps only.
- Do not clear `disable_depth` on every frame return.
- Only clear when returning above the activation depth.
- Do not mutate bitmap bits from unrelated subsystems.
- Access marks should come from host/instruction touch points only.

## WHERE TO LOOK
- Adjust volatile category mapping bits: `volatile.rs`.
- Change detention cap merge policy: `tracker.rs::apply_or_create_limit`.
- Change disable/enable call-tree semantics: `tracker.rs::{disable_access,enable_access,enable_access_if_returning}`.
- Change beneficiary/oracle detection helpers: `tracker.rs` and host call sites in `../evm/host.rs`.
