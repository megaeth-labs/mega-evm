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
//! The sandbox is designed to guarantee that **any execution that starts and completes will
//! charge the signer for gas consumed**. This prevents spam attacks where an attacker triggers
//! execution but avoids paying. Key protections:
//!
//! 1. **Top-level only (`depth == 0`)**: Sandbox interception only occurs for direct transaction
//!    calls. Calls from contracts (depth > 0) hit the Solidity fallback which reverts with
//!    `NotIntercepted()`. This prevents wrapping the call to observe and revert.
//!
//! 2. **Execution errors return success**: Failures like `ExecutionReverted`, `ExecutionHalted`,
//!    and `EmptyCodeDeployed` return normally with error data, ensuring state changes (including
//!    gas charges) are committed.
//!
//! 3. **Atomic state application**: Once sandbox execution completes, `apply_sandbox_state` always
//!    succeeds—there is no path for the outer transaction to revert after the signer has been
//!    charged.
//!
//! **Invariant**: If sandbox execution starts and produces an outcome (Success or Failure),
//! the signer WILL be charged for gas consumed. The outer transaction cannot revert this.
//!
//! # Module Structure
//!
//! - `execution` - Core sandbox execution logic and the main entry point
//!   [`execute_keyless_deploy_call`]
//! - `state` - Type-erased database wrapper ([`SandboxDb`]) for isolated execution
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
mod tx;

pub use error::*;
pub use execution::*;
pub use state::*;
pub use tx::*;
