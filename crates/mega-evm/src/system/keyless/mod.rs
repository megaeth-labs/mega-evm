//! The `KeylessDeploy` system contract, and keyless deployment (Nick's Method): a pre-EIP-155
//! signed creation, deployed at the address its signer's first creation gets on every chain, with
//! the gas limit the caller chooses.
//!
//! # A native creation
//!
//! A `keylessDeploy(bytes,uint256)` call a transaction makes is rewritten into the creation it
//! stands for, before the frame starts and before any interceptor or inspector sees it
//! ([`rewrite`]). The call becomes a frame no code runs in: it pays the fixed overhead and what the
//! `CREATE` opcode charges its frame, then starts the creation as its child — as the signer, at the
//! signer's Nick's-Method address, with `gasLimitOverride` capped to what the call has left. From
//! there the creation is an ordinary `CREATE` frame: priced, limited and journaled as one, seen by
//! an inspector as one, and returned into the call as one returns into the frame that started it.
//! The call then answers in the `IKeylessDeploy` ABI ([`settle`]). There is no second EVM and no
//! state to merge.
//!
//! What the native frame means for a deployment:
//!
//! - `ORIGIN` and `GASPRICE` in the init code are the transaction's own.
//! - The signer's nonce is bumped by the creation, as any creator's is, and stays bumped when the
//!   deployment fails: that is the replay barrier. A deployment is refused once the signer's nonce
//!   is above 1 (`SignerNonceTooHigh`), so a signer gets at most two attempts that fail — the first
//!   bumps its nonce to 1, the second to 2 — and a third is refused.
//! - A signer with no account is charged its account as state gas, once, by the call: the nonce
//!   bump creates it, and no other charge does.
//! - The deployed contract's account is charged as the `CREATE` opcode charges it, and given back
//!   when the deployment fails, at the price it was charged.
//! - `gasUsed` is what the creation spent, with no transaction intrinsic: the sandbox of the legacy
//!   engine ran the deployment as a transaction of its own, this engine does not.
//! - A limit the deployment crosses stops it as it stops any frame: a frame budget reverts the
//!   creation, and the call returns `ExecutionReverted` with the limit's revert data; a transaction
//!   limit stops the transaction, and the call reverts with the stop, taking back the signer's
//!   nonce with everything else.
//!
//! # The error ABI
//!
//! The contract's errors are unchanged, and so are the rules that produce them. Four of them have
//! no producer here: `ParentBudgetExceeded` and `InvalidTransaction` were the sandbox's, which had
//! a budget of its own to preflight and a transaction of its own to validate;
//! `InsufficientComputeGas` was the compute-gas limit's, which is the execution cap on the regular
//! pool here, so a call that cannot pay the overhead runs out of gas; and `InternalError` was a
//! failed read, which fails the transaction with its cause here, as it does at every other site.
//! `AddressMismatch` and `NoContractCreated` stay as the answers to a creation revm reports at an
//! address other than the pinned one, which no deployment reaches.
//!
//! # The contract
//!
//! Every selector but `keylessDeploy` runs the deployed bytecode, which carries no fallback and
//! reverts with empty data, and so does a `keylessDeploy` call a contract makes: its method body
//! reverts with `NotIntercepted()`.
//!
//! The rest of the module is data: decoding the pre-EIP-155 transaction, recovering its signer,
//! deriving the deploy address, and mapping errors to and from the `IKeylessDeploy` ABI.

mod dispatch;
mod error;
mod settle;
mod tx;

pub use error::*;
pub use tx::*;

pub(crate) use dispatch::{rewrite, KeylessCall, Rewrite};
pub(crate) use settle::{give_back_history, settle};

use alloy_primitives::{address, Address};

/// The address of the `KeylessDeploy` system contract.
pub const KEYLESS_DEPLOY_ADDRESS: Address = address!("0x6342000000000000000000000000000000000003");

/// The code of the `KeylessDeploy` contract.
pub use mega_system_contracts::keyless_deploy::LATEST_CODE as KEYLESS_DEPLOY_CODE;

/// The code hash of the `KeylessDeploy` contract.
pub use mega_system_contracts::keyless_deploy::LATEST_CODE_HASH as KEYLESS_DEPLOY_CODE_HASH;

pub use mega_system_contracts::keyless_deploy::IKeylessDeploy;

/// The regular gas a `keylessDeploy` call pays before its deployment starts: the fixed cost of
/// decoding the transaction, recovering its signer and preparing the deployment. Provisional,
/// as the rest of the engine's numbers are.
pub const KEYLESS_DEPLOY_OVERHEAD_GAS: u64 = 100_000;
