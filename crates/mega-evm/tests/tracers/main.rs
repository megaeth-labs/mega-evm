//! Snapshot tests of `revm-inspectors` tracer output on the Satin engine.
//!
//! These live next to the other Satin integration tests under `crates/mega-evm/tests/`, not
//! under `bin/mega-evme`: pinning the JSON against the engine's own `test-utils` keeps the
//! snapshots independent of CLI argument parsing and of the legacy 1.7.1 leg `mega-evme` still
//! links.
//!
//! The geth views are what a node returns: each is built by revm-inspectors' `DebugInspector`,
//! whose `get_result` is what reth's debug API calls. The CLI (`bin/mega-evme/src/common/trace.rs`)
//! calls the trace builders directly, without setting the root frame's gas limit to the
//! transaction's, so the root `gas` it prints is the first frame's budget instead.
//!
//! The EIP-3155 trace is the revm fork's `TracerEip3155`, the tracer the state-test runner runs
//! Satin under.
//!
//! The snapshots are the output of the tracers `Cargo.lock` resolves: revm-inspectors 0.40.4 (the
//! workspace asks for `0.40.1` or later) and the revm fork's inspector crate. A lock update that
//! changes what a tracer prints moves them, and fails the named tracer-shape assertions it
//! touches, such as the keyless struct logs' gap and the detention step's rendering.
//!
//! Each scenario pins six snapshots under `tests/tracers/snapshots/`: the call tracer with and
//! without logs, the prestate tracer in both modes and the opcode tracer as sorted JSON, and the
//! EIP-3155 trace as its JSON lines. The comparison runs only at the spec's byte prices.
//!
//! Review a change with `cargo insta review`, which shows each snapshot and accepts or rejects it
//! on its own. Without `cargo-insta`, diff each `.snap.new` the failing run left against its
//! `.snap` and accept it on its own, by moving it over the `.snap` without its `assertion_line:`
//! header line. A tracer-shape change moves many snapshots at once; each is still reviewed and
//! accepted on its own, never all at once. A snapshot no test refers to is unreferenced, which
//! `cargo insta test --check --unreferenced=reject` rejects.

mod gas;
mod harness;
mod scenarios;
#[path = "../shared/snapshot.rs"]
mod snapshot;
