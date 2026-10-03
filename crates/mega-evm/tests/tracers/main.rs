//! Golden tests of `revm-inspectors` tracer output on the Satin engine.
//!
//! These live next to the other Satin integration tests under `crates/mega-evm/tests/`, not
//! under `bin/mega-evme`: pinning the JSON against the engine's own `test-utils` keeps the goldens
//! independent of CLI argument parsing and of the legacy 1.7.1 leg `mega-evme` still links.
//!
//! The geth views are what a node returns: each is built by revm-inspectors' `DebugInspector`,
//! whose `get_result` is what reth's debug API calls. The CLI (`bin/mega-evme/src/common/trace.rs`)
//! calls the trace builders directly, without setting the root frame's gas limit to the
//! transaction's, so the root `gas` it prints is the first frame's budget instead.
//!
//! Default: compare pretty JSON, and the EIP-3155 trace as JSON lines, byte-for-byte with
//! `tests/tracers/goldens/<scenario>/`.
//! Rewrite: `UPDATE_GOLDENS=1 cargo test -p mega-evm --test tracers`.

mod gas;
mod harness;
mod scenarios;
