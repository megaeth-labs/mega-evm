//! Sandbox execution environment for keyless contract deployment.
//!
//! This module provides isolated EVM execution for implementing "Nick's Method" - a technique
//! for deterministic contract deployment that allows deploying contracts to the same address
//! across different chains using pre-EIP-155 transactions.
//!
//! # Overview
//!
//! The sandbox executes keyless deployment transactions in an isolated environment, then
//! applies only the resulting state changes to the parent context. This approach:
//!
//! - Validates pre-EIP-155 legacy transactions
//! - Recovers the signer and computes the deterministic deploy address
//! - Executes contract creation with a nonce override of 0
//! - Merges state changes back to the parent journal
//!
//! # Spam Protection
//!
//! The sandbox is designed to guarantee that **every execution that starts and completes is paid
//! for** — either by the signer through merged sandbox state on normal completion, or by the
//! outer caller through `record_cost` on Rex5 residual-overflow rejects. This prevents spam
//! attacks where an attacker triggers execution but avoids paying.
//!
//! ## General Protections
//!
//! 1. **Top-level only (`depth == 0`)**: Sandbox interception only occurs for direct transaction
//!    calls. Calls from contracts (depth > 0) hit the Solidity fallback which reverts with
//!    `NotIntercepted()`. This prevents wrapping the call to observe and revert.
//!
//! 2. **Execution errors return success**: Failures like `ExecutionReverted`, `ExecutionHalted`,
//!    and `EmptyCodeDeployed` return normally with error data so state changes (including the
//!    signer's balance deduction via merged state) are committed.
//!
//! 3. **Atomic state application**: `apply_sandbox_state` is the sole writer that makes a deploy
//!    (and the signer-side charge) survive. Pre-Rex5 keeps the legacy direct merge for replay
//!    compatibility, while Rex5 records stateful diffs in the parent journal so a later outer-frame
//!    revert or halt can unwind them.
//!
//! ## Rex5 Resource-Envelope Defenses
//!
//! Rex5 adds three layers to keep a parent-budget-exceeding sandbox from either corrupting
//! accounting or escaping its charge:
//!
//! 1. **Upfront cap**: sandbox TX limits are tightened to the parent's remaining envelope before
//!    execution starts, so normal execution cannot escape the envelope.
//!
//! 2. **Intrinsic preflight**: if the sandbox's known pre-frame intrinsic usage alone would exceed
//!    the envelope, the outer call reverts with `ParentBudgetExceeded` before any sandbox EVM
//!    construction.
//!
//! 3. **Post-merge safety net**: residual overflow (e.g., a single-opcode overshoot at a TX-level
//!    compute-gas check) is caught after the sandbox completes. On this path the sandbox state is
//!    NOT merged, the sandbox's EVM gas is charged to the outer caller via `record_cost`, remaining
//!    outer gas is rescued for refund, and the outer call halts with `OutOfGas` — matching the revm
//!    "halted transactions commit only pre-execution state" convention. Only this path halts the
//!    outer call.
//!
//! # Module Structure
//!
//! - `execution` - Core sandbox execution logic and the main entry point
//!   [`execute_keyless_deploy_call`]
//! - `state` - Type-erased database wrapper ([`SandboxDb`]) for isolated execution
//! - `state_merge` - Replay-safe merge of sandbox state into the parent journal
//! - `tx` - Transaction decoding and validation for pre-EIP-155 transactions
//! - `error` - Error types ([`KeylessDeployError`]) that map to Solidity errors in `IKeylessDeploy`
//!
//! # Type Erasure Strategy
//!
//! A key challenge in implementing sandbox execution is preventing infinite type instantiation.
//! If `MegaContext<DB>` creates a sandbox with `MegaContext<DB>`, the compiler would attempt
//! to instantiate `MegaContext<MegaContext<MegaContext<...>>>`, causing an Internal Compiler
//! Error (ICE) during monomorphization.
//!
//! The solution is [`SandboxDb`], a type-erased database wrapper:
//!
//! ```text
//! MegaContext<RealDB>
//!     └─> sandbox execution
//!         └─> MegaContext<SandboxDb>  // SandboxDb is a concrete type, not MegaContext<...>
//! ```
//!
//! [`SandboxDb`] implements the `Database` trait while internally holding a type-erased
//! reference to the parent database via the `ErasedDatabase` trait. This breaks the recursive
//! type chain while still allowing the sandbox to read from the parent's state.
//!
//! # Usage
//!
//! The sandbox is invoked automatically when a call to the `KeylessDeploy` precompile is
//! intercepted in `MegaContext::call`. The entry point is [`execute_keyless_deploy_call`],
//! which validates the transaction, creates a sandbox environment, executes the deployment,
//! and returns the result.
//!
//! [`execute_keyless_deploy_call`]: execution::execute_keyless_deploy_call
//! [`SandboxDb`]: state::SandboxDb

mod error;
mod execution;
mod state;
mod state_merge;
mod tx;

pub use error::*;
pub use execution::*;
pub use state::*;
pub use tx::*;
