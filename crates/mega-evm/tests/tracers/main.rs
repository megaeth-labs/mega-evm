//! Golden tests of `revm-inspectors` tracer output on the Satin engine.
//!
//! These live next to the other Satin integration tests under `crates/mega-evm/tests/`, not
//! under `bin/mega-evme`. The CLI already wires the same tracers
//! (`bin/mega-evme/src/common/trace.rs`); pinning their JSON against the engine's own `test-utils`
//! keeps the goldens independent of CLI argument parsing and of the legacy 1.7.1 leg `mega-evme`
//! still links.
//!
//! Default: compare pretty JSON, and the EIP-3155 trace as JSON lines, byte-for-byte with
//! `tests/tracers/goldens/<scenario>/`.
//! Rewrite: `UPDATE_GOLDENS=1 cargo test -p mega-evm --test tracers`.

mod harness;
mod scenarios;
