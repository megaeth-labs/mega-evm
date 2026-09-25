//! The `KeylessDeploy` system contract, and keyless deployment (Nick's Method): a pre-EIP-155
//! signed creation, deployed at the address its signer's first creation gets on every chain, with
//! the gas limit the caller chooses.
//!
//! # A native creation
//!
//! A `keylessDeploy(bytes,uint256)` call a transaction makes is dispatched before any interceptor
//! sees it ([`is_dispatched`]), and its frame is built as any call's is, but runs no code: its
//! actions are made by hand ([`run`]). On its first run it pays the fixed overhead and what the
//! `CREATE` opcode charges its frame, then starts the creation as its child — as the signer, at
//! the signer's Nick's-Method address, with `gasLimitOverride` capped to what the call has left.
//! From there the creation is an ordinary `CREATE` frame: priced, limited and journaled as one,
//! and returned into the call as one returns into the frame that started it ([`settle`]). On its
//! resume the call answers in the `IKeylessDeploy` ABI. An inspector sees both frames: the call,
//! and the creation as its child. There is no second EVM and no state to merge.
//!
//! What the native frame means for a deployment:
//!
//! - The call is a frame on revm's stack, built on the contract as any call to it is: at depth 0,
//!   with its own journal checkpoint and gas. No code runs in it, and the contract's bytecode never
//!   runs for a call the dispatch takes. The contract's account and code are loaded to start the
//!   transaction's first frame, as for any transaction to the contract, so a witness holds them
//!   either way; the frame touches the account, as a call touches its target, and a call that
//!   reverts takes the touch back. An inspector sees one call frame, started and ended as any
//!   call's, with no interpreter initialized and no step in it.
//! - A call carrying value is refused `NoEtherTransfer()`, having paid the overhead, before revm
//!   builds its frame: no value moves, and nothing its start would write is counted.
//! - The dispatch is by address and selector, whatever the address holds: where the state holds no
//!   contract code, the frame is built on the contract's own. A chain holds it from the fork that
//!   deploys it on.
//! - `ORIGIN` and `GASPRICE` in the init code are the transaction's own.
//! - The creation runs at depth 1, as the call's child: one level below the same init code sent as
//!   a creation transaction, which runs at depth 0. Its frames reach the call-stack limit one frame
//!   earlier. The call is the frame above the constructor, and its charges — the overhead and the
//!   `CREATE` opcode's regular gas — are compute the transaction spent before the constructor ran:
//!   a detained constructor has spent them already, and one detained from the transaction's start
//!   hears `remainingComputeGas()` answer the cap less them.
//! - The signer's nonce is bumped by the creation, as any creator's is, and a deployment is refused
//!   once the signer's nonce is above 1 (`SignerNonceTooHigh`). A deployment from nonce 0 keeps the
//!   bump, which is the replay barrier. From nonce 1 the creation's bump is taken back, whether the
//!   deployment succeeds or fails, only when it is the last nonce change the deployment made: the
//!   signer stays at 1, as in the legacy engine. The call is permissionless and the signed
//!   transaction public, so no number of failing calls, whoever makes them, gets such a signer's
//!   deployment refused, and a deployment that succeeded answers a resubmission with
//!   `ContractAlreadyExists()`. The bump goes with its write record and that record's history,
//!   unless the creation succeeded and moved value out of the signer, whose account then keeps a
//!   write of its own. A signer whose own code spends a nonce in the constructor that survives it
//!   keeps every bump, the creation's included: it ends above 1, and every later deployment of it
//!   is refused `SignerNonceTooHigh`. On a default configuration only a delegated signer's code can
//!   do that, with a `CREATE` or `CREATE2`, successful or not: a signer with other code is refused
//!   `SignerHasCode()` unless EIP-3607 is disabled. The nonce is never moved back under a later
//!   bump, because a later bump may stand for an account.
//! - A signer with no account is charged its account as state gas, once, by the call: the nonce
//!   bump creates it, and no other charge does.
//! - The call pays what the `CREATE` opcode charges its frame: its regular gas, which stays spent,
//!   and the deployed contract's account, given back when the deployment fails, at the price it was
//!   charged.
//! - `gasUsed` is what the creation spent, with no transaction intrinsic: the sandbox of the legacy
//!   engine ran the deployment as a transaction of its own, this engine does not. It is counted
//!   before refunds: a refund the creation earns goes to the transaction, as any frame's does.
//! - A limit the deployment crosses stops it as it stops any frame: a frame budget reverts the
//!   creation, and the call returns `ExecutionReverted` with the limit's revert data; a transaction
//!   limit stops the transaction, and the call reverts with the stop, taking back the signer's
//!   nonce with everything else.
//!
//! # The error ABI
//!
//! The contract's errors are unchanged, and so are the rules that produce them and the order they
//! are checked in, so a call several rules refuse reports the legacy engine's error. Four have
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
//! Every selector but `keylessDeploy` runs the deployed bytecode: a method the contract declares,
//! such as `version()`, answers, and a selector it does not declare finds no fallback and reverts
//! with empty data. A `keylessDeploy` call a contract makes runs the bytecode too, and its method
//! body reverts with `NotIntercepted()`.
//!
//! The rest of the module is data: decoding the pre-EIP-155 transaction, recovering its signer,
//! deriving the deploy address, and mapping errors to and from the `IKeylessDeploy` ABI.

mod dispatch;
mod error;
mod settle;
mod tx;

pub use error::*;
pub use tx::*;

pub(crate) use dispatch::{is_dispatched, ready, run, runs, KeylessFrame};
pub(crate) use settle::{returning, settle};

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
