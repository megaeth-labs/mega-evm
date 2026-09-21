//! The Oracle system contract.
//!
//! The Oracle holds the protocol's key-value storage (`getSlot` / `setSlot`) and is the surface
//! through which a contract reaches the node's oracle service: `sendHint` carries a message to
//! the service backend. Its Solidity source is
//! `crates/system-contracts/contracts/Oracle.sol`.

use alloy_evm::Database;
use alloy_primitives::{address, Address};
use alloy_sol_types::SolCall;
use revm::{handler::FrameResult, interpreter::CallInputs};

use crate::{system::intercept::peek_selector, ExternalEnvTypes, MegaContext, OracleEnv};

/// The address of the Oracle system contract.
pub const ORACLE_CONTRACT_ADDRESS: Address = address!("0x6342000000000000000000000000000000000001");

/// The code of the Oracle contract.
pub use mega_system_contracts::oracle::LATEST_CODE as ORACLE_CONTRACT_CODE;

/// The code hash of the Oracle contract.
pub use mega_system_contracts::oracle::LATEST_CODE_HASH as ORACLE_CONTRACT_CODE_HASH;

pub use mega_system_contracts::oracle::IOracle;

/// Forwards the hint of a `sendHint(bytes32,bytes)` call to the node's oracle service, and
/// never answers the call: the deployed bytecode runs, whatever the interceptor did.
///
/// `sendHint` is the one intercepted selector of the Oracle, and the only one that is a side
/// effect rather than an answer. The Oracle's storage methods are ordinary code, and reading
/// its storage through the node's oracle service is the Host's business.
///
/// Two conditions admit a hint, and neither is a statement about the frame that runs afterwards:
///
/// - the call was forwarded gas. A call with none cannot run the dispatcher of the bytecode at all,
///   so its hint would be a free message to the service;
/// - the call carries no value. `sendHint` is not payable, so a value-bearing call reverts in the
///   bytecode and its hint would be one the caller never sent.
///
/// The gas condition is the legacy engine's, carried over; the value condition is new.
///
/// Gas above zero does not promise that the bytecode succeeds: a hint forwarded with one gas
/// reaches the service and the frame it was sent from then runs out of gas. An admitted hint is a
/// synchronous, irreversible side effect — the service holds it whatever the frame, or the
/// transaction, does next. That is also why its bytes are counted on the transaction rather than
/// on the frame.
///
/// A `STATICCALL` does forward: `sendHint` is a view method and writes nothing.
///
/// The payload is counted toward the transaction's data size before it is decoded
/// ([`AdditionalLimit::record_hint_bytes`](crate::AdditionalLimit)), so trailing bytes an ABI
/// decoder ignores are paid for; a payload that crosses the transaction's limit is not
/// forwarded, and the limit stops the transaction at the frame the call would have started.
pub(crate) fn intercept<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inputs: &CallInputs,
) -> Option<FrameResult> {
    let selector = peek_selector(&inputs.input, ctx)?;
    if selector != IOracle::sendHintCall::SELECTOR ||
        inputs.transfers_value() ||
        inputs.gas_limit == 0
    {
        return None;
    }
    let payload = inputs.input.bytes(&*ctx);
    if ctx.additional_limit.record_hint_bytes(payload.len() as u64).exceeded_limit() {
        return None;
    }
    // Trailing bytes after a valid envelope are dropped by the decoder and a malformed envelope
    // is refused; either way the payload is paid for above.
    let call = IOracle::sendHintCall::abi_decode(&payload).ok()?;
    ctx.external_envs().oracle_env.on_hint(inputs.caller, call.topic, call.data);
    None
}
