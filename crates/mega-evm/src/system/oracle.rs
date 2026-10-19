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
use revm::{
    handler::FrameResult,
    interpreter::{CallInputs, InstructionResult},
};

use crate::{
    synthetic_call_result, system::intercept::peek_selector, ExternalEnvTypes, MegaContext,
    OracleEnv,
};

/// The address of the Oracle system contract.
pub const ORACLE_CONTRACT_ADDRESS: Address = address!("0x6342000000000000000000000000000000000001");

/// The code of the Oracle contract.
pub use mega_system_contracts::oracle::LATEST_CODE as ORACLE_CONTRACT_CODE;

/// The code hash of the Oracle contract.
pub use mega_system_contracts::oracle::LATEST_CODE_HASH as ORACLE_CONTRACT_CODE_HASH;

pub use mega_system_contracts::oracle::IOracle;

/// Forwards the hint of a `sendHint(bytes32,bytes)` call to the node's oracle service, and lets
/// the deployed bytecode run; it answers the call only with the stop of a hint that would cross
/// the transaction's data-size limit.
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
/// reaches the service and the frame it was sent from then runs out of gas. A forwarded hint is a
/// synchronous, irreversible side effect — the service holds it whatever the frame, or the
/// transaction, does next. That is one reason its bytes are counted on the transaction rather than
/// on the frame; the other, which holds as well for an admitted payload that fails to decode and
/// is never forwarded, is that admitting it and decoding it is work done either way.
///
/// A `STATICCALL` does forward: `sendHint` is a view method and writes nothing.
///
/// The payload is counted toward the transaction's data size before it is decoded
/// ([`AdditionalLimit::record_hint_bytes`](crate::AdditionalLimit)), so trailing bytes an ABI
/// decoder ignores are paid for; a payload that would cross the transaction's limit is neither
/// forwarded nor counted, and the call is answered with the stop: a revert carrying
/// `MegaLimitExceeded`, on the gas it was forwarded, as the frame it would have started. So the
/// data size a transaction keeps holds exactly the hints that were admitted.
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
    let check = ctx.additional_limit.record_hint_bytes(payload.len() as u64);
    if check.exceeded_limit() {
        let output = ctx.additional_limit.note_frame_stop(&check);
        return Some(synthetic_call_result(inputs, InstructionResult::Revert, output));
    }
    // Trailing bytes after a valid envelope are dropped by the decoder and a malformed envelope
    // is refused; either way the payload is paid for above.
    let call = IOracle::sendHintCall::abi_decode(&payload).ok()?;
    ctx.external_envs().oracle_env.on_hint(inputs.caller, call.topic, call.data);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::MemoryDatabase, EvmTxRuntimeLimits, ExternalEnvs, LimitCheck, LimitKind,
        MegaSpecId, TestExternalEnvs,
    };
    use alloy_primitives::{Bytes, B256, U256};
    use core::convert::Infallible;
    use revm::interpreter::{CallInput, CallScheme, CallValue};

    const CONTRACT: Address = address!("00000000000000000000000000000000000c0001");
    const FORWARD: u64 = 100_000;

    /// A context recording the hints its oracle service receives, under a transaction data-size
    /// limit of `limit` bytes and no other limit.
    fn recording(
        limit: u64,
    ) -> (MegaContext<MemoryDatabase, TestExternalEnvs<Infallible>>, TestExternalEnvs<Infallible>)
    {
        let envs = TestExternalEnvs::<Infallible>::new();
        let ctx = MegaContext::new_with_external_envs(
            MemoryDatabase::default(),
            MegaSpecId::SATIN,
            ExternalEnvs::from(envs.clone()),
        )
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit));
        (ctx, envs)
    }

    /// A `CALL` from [`CONTRACT`] to the Oracle carrying `sendHint(topic, data)`.
    fn send_hint(data: &[u8]) -> CallInputs {
        let input = IOracle::sendHintCall { topic: B256::ZERO, data: Bytes::copy_from_slice(data) }
            .abi_encode();
        CallInputs {
            input: CallInput::Bytes(input.into()),
            return_memory_offset: 0..0,
            gas_limit: FORWARD,
            reservoir: 0,
            bytecode_address: ORACLE_CONTRACT_ADDRESS,
            known_bytecode: Default::default(),
            target_address: ORACLE_CONTRACT_ADDRESS,
            caller: CONTRACT,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: false,
        }
    }

    /// A hint whose payload would cross the transaction's data-size limit answers the call it was
    /// sent with with the stop itself: a revert carrying `MegaLimitExceeded` on the gas the call
    /// was forwarded, untouched. Nothing is forwarded and nothing is counted. A payload exactly at
    /// the limit is forwarded and counted, and the bytecode runs.
    #[test]
    fn test_a_hint_that_would_cross_the_limit_answers_its_call_with_the_stop() {
        let inputs = send_hint(&[7_u8; 64]);
        let payload = inputs.input.len() as u64;

        let (mut ctx, envs) = recording(payload - 1);
        let answer = intercept(&mut ctx, &inputs, 1).expect("the call is answered");
        let stop = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: payload - 1,
            used: payload,
            frame_local: false,
        };
        let result = answer.interpreter_result();
        assert_eq!(result.result, InstructionResult::Revert);
        assert_eq!(result.output, stop.revert_data());
        assert_eq!(result.gas.remaining(), FORWARD, "the forward is untouched");
        assert!(envs.recorded_hints().is_empty(), "nothing is forwarded");
        assert_eq!(ctx.additional_limit.usage().data_size, 0, "nothing is counted");

        let (mut ctx, envs) = recording(payload);
        assert!(intercept(&mut ctx, &inputs, 1).is_none(), "the bytecode runs");
        assert_eq!(envs.recorded_hints().len(), 1);
        assert_eq!(ctx.additional_limit.usage().data_size, payload);
    }
}
