//! The Oracle system contract.
//!
//! The Oracle holds the protocol's key-value storage (`getSlot` / `setSlot`) and is the surface
//! through which a contract reaches the node's oracle service: `sendHint` carries a message to
//! the service backend. Its Solidity source is
//! `crates/system-contracts/contracts/Oracle.sol`.
//!
//! Its storage is read through the service too, by the Host: an `SLOAD` in the Oracle's own frame
//! loads the slot through the journal and answers the service's value when it has one, the loaded
//! value otherwise. A node that replays a block without the service must price and witness it as
//! the node that built it did, so the read is always priced cold and the slot is loaded whichever
//! source answered.

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
/// Three conditions admit a hint, and none is a statement about the frame that runs afterwards:
///
/// - the call was forwarded gas. A call with none cannot run the dispatcher of the bytecode at all,
///   so its hint would be a free message to the service;
/// - the call carries no value. `sendHint` is not payable, so a value-bearing call reverts in the
///   bytecode and its hint would be one the caller never sent;
/// - the calling frame may read volatile data. A hint is how a contract asks the oracle service for
///   what it will read from the Oracle's storage, and a frame whose volatile-data access
///   `MegaAccessControl` switched off — its own switch, or one a frame above it set — cannot read
///   it. Its hint is dropped, not refused: the call runs the bytecode as any other, because a hint
///   is a message to the service, not state, and dropping it changes nothing the call returns.
///
/// The gas and switch conditions are the legacy engine's, carried over; the value condition is
/// new. A hint that fails any of them is neither forwarded nor counted.
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
/// decoder ignores are paid for; a payload that would cross the transaction's limit is neither
/// forwarded nor counted, and the limit stops the transaction at the frame the call would have
/// started. So the data size a transaction keeps holds exactly the hints that were admitted.
///
/// `depth` is the depth of the frame the call would start; the calling frame is one level above
/// it, and a transaction that calls the Oracle directly has no frame above it, so nothing it
/// switched off can withhold its hint.
pub(crate) fn intercept<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inputs: &CallInputs,
    depth: usize,
) -> Option<FrameResult> {
    let selector = peek_selector(&inputs.input, ctx)?;
    if selector != IOracle::sendHintCall::SELECTOR ||
        inputs.transfers_value() ||
        inputs.gas_limit == 0 ||
        depth.checked_sub(1).is_some_and(|caller| ctx.detention.is_access_disabled(caller))
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
