//! The interceptor dispatch: which system contract answers a call, and how its answer is built.
//!
//! A `CALL` or `STATICCALL` whose target is a system contract with an interceptor and whose
//! first four bytes name one of that contract's intercepted methods is answered by the engine,
//! before revm builds a frame for it. Everything else runs the contract's own bytecode.
//!
//! # The dispatch order
//!
//! 1. **The scheme guard.** Only `CALL` and `STATICCALL` reach the dispatch; `CALLCODE` and
//!    `DELEGATECALL` run the callee's code in the caller's context, where a system contract's
//!    semantics would apply to the wrong account, so they are refused before any interceptor (see
//!    [`MegaEvm::intercept`](crate::MegaEvm)).
//! 2. **The address.** One comparison against the `0x6342…` range decides whether any interceptor
//!    can be concerned, before a byte of calldata is read ([`intercepted_contract`]).
//! 3. **The selector.** The contract's own dispatch peeks the first four bytes of the input
//!    ([`peek_selector`]) without materialising the rest. A selector the contract does not
//!    intercept is *not* intercepted: the call falls through, which is to say the deployed bytecode
//!    runs, and what that bytecode answers is the contract's own:
//!
//!    - the two control contracts have a fallback that reverts with `NotIntercepted()`, so every
//!      selector they do not intercept ends there;
//!    - `KeylessDeploy` has no fallback, so a selector it does not declare reverts with empty data;
//!      a `keylessDeploy` call the dispatch did not take — one a contract makes — reaches the
//!      method body and its own `NotIntercepted()`, as a dispatched one does after its charge;
//!    - the Oracle's other selectors are methods it runs (`getSlot`, `version`), and one it does
//!      not declare reverts with empty data.
//!
//!    A selector matches on its own four bytes, whatever follows them.
//! 4. **The value policy.** A method that takes no value answers a value-bearing call with
//!    `NonZeroTransfer()` ([`reject_non_zero_transfer`]), or with the error its own ABI names. The
//!    policy is per method, after the selector matched, so a value-bearing call to an unknown
//!    selector still falls through.
//!
//! # The answer
//!
//! An answer is a [`synthetic_call_result`]: the forwarded gas untouched, the caller's reservoir
//! carried, and the calling opcode's upfront state-gas flags, so the caller settles it exactly
//! like a frame revm ran.

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolError;
use revm::{
    context::{ContextTr, LocalContextTr},
    handler::FrameResult,
    interpreter::{CallInput, CallInputs, InstructionResult},
};

use crate::{
    synthetic_call_result,
    system::{IMegaAccessControl, ORACLE_CONTRACT_ADDRESS},
    ExternalEnvTypes, MegaContext,
};

/// The revert data of `NonZeroTransfer()`: the answer to a call that carries value to a method
/// that takes none. Both control contracts declare the error, with the same selector.
pub const NON_ZERO_TRANSFER_REVERT_DATA: [u8; 4] = IMegaAccessControl::NonZeroTransfer::SELECTOR;

/// The first 19 bytes every system contract address shares; only the last byte tells the six
/// apart. The dispatch compares them in one go, so a call to any other address leaves the
/// dispatch after a single comparison.
const SYSTEM_CONTRACT_PREFIX: [u8; 19] = {
    let mut prefix = [0_u8; 19];
    let address = ORACLE_CONTRACT_ADDRESS.0 .0;
    let mut i = 0;
    while i < prefix.len() {
        prefix[i] = address[i];
        i += 1;
    }
    prefix
};

/// A system contract whose calls an interceptor answers.
///
/// The High-Precision Timestamp wrapper and the `SequencerRegistry` are not here: they have no
/// interceptor and run their bytecode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterceptedContract {
    /// The Oracle: `sendHint` reaches the node's oracle service.
    Oracle,
    /// `KeylessDeploy`: `keylessDeploy` deploys a pre-EIP-155 transaction.
    KeylessDeploy,
    /// `MegaAccessControl`: the volatile-data access switch.
    AccessControl,
    /// `MegaLimitControl`: what the running call has left.
    LimitControl,
}

/// The system contract with an interceptor `address` names, if any.
///
/// The hot path of the dispatch: every call a transaction makes runs through it, so it is one
/// comparison against the shared prefix and one match on the last byte.
#[inline]
pub(crate) fn intercepted_contract(address: &Address) -> Option<InterceptedContract> {
    let bytes = address.0 .0;
    if bytes[..SYSTEM_CONTRACT_PREFIX.len()] != SYSTEM_CONTRACT_PREFIX {
        return None;
    }
    match bytes[SYSTEM_CONTRACT_PREFIX.len()] {
        1 => Some(InterceptedContract::Oracle),
        3 => Some(InterceptedContract::KeylessDeploy),
        4 => Some(InterceptedContract::AccessControl),
        5 => Some(InterceptedContract::LimitControl),
        _ => None,
    }
}

/// Answers a call to a system contract, or `None` when nothing intercepts it and the contract's
/// own bytecode runs.
///
/// `depth` is the depth of the frame the call would start, which is the calling frame's journal
/// depth. The caller has already applied the scheme guard.
///
/// The inputs are taken mutably because an interceptor may charge the frame it hands on
/// (`KeylessDeploy`'s fixed overhead); an interceptor that answers the call charges its own
/// answer instead.
#[inline]
pub(crate) fn intercept<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inputs: &mut CallInputs,
    depth: usize,
) -> Option<FrameResult> {
    match intercepted_contract(&inputs.target_address)? {
        InterceptedContract::Oracle => crate::system::oracle::intercept(ctx, inputs, depth),
        InterceptedContract::KeylessDeploy => crate::system::keyless::intercept(ctx, inputs, depth),
        InterceptedContract::AccessControl => crate::system::control::intercept(ctx, inputs, depth),
        InterceptedContract::LimitControl => crate::system::limit_control::intercept(ctx, inputs),
    }
}

/// The first four bytes of a call input, without materialising the rest of it.
///
/// `None` when the input is shorter than four bytes: no selector, so nothing is intercepted.
/// For an input that lives in shared memory only the four-byte head is borrowed.
#[inline]
pub(crate) fn peek_selector<CTX: ContextTr>(input: &CallInput, ctx: &CTX) -> Option<[u8; 4]> {
    let mut selector = [0_u8; 4];
    match input {
        CallInput::Bytes(bytes) => {
            selector.copy_from_slice(bytes.get(..4)?);
        }
        CallInput::SharedBuffer(range) => {
            if range.len() < 4 {
                return None;
            }
            let head = ctx.local().shared_memory_buffer_slice(range.start..range.start + 4)?;
            selector.copy_from_slice(&head);
        }
    }
    Some(selector)
}

/// The answer to a call that carries value to a method that takes none: a revert with
/// `NonZeroTransfer()`. `None` when the call carries no value and the method may run.
///
/// Every intercepted method of the two control contracts and of `KeylessDeploy` reads state or
/// steers execution and takes no value. A method that takes value on purpose states why, in its
/// own interceptor, instead of calling this.
#[inline]
pub(crate) fn reject_non_zero_transfer(inputs: &CallInputs) -> Option<FrameResult> {
    inputs.transfer_value().is_some_and(|value| !value.is_zero()).then(|| {
        synthetic_call_result(
            inputs,
            InstructionResult::Revert,
            Bytes::from_static(&NON_ZERO_TRANSFER_REVERT_DATA),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::{
        keyless::KEYLESS_DEPLOY_ADDRESS, IMegaLimitControl, ACCESS_CONTROL_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, LIMIT_CONTROL_ADDRESS, SEQUENCER_REGISTRY_ADDRESS,
    };
    use alloy_primitives::{address, keccak256};

    /// Every system contract with an interceptor is the one the dispatch names, and the ones
    /// without an interceptor are not intercepted.
    #[test]
    fn test_every_system_address_maps_to_its_interceptor() {
        assert_eq!(
            intercepted_contract(&ACCESS_CONTROL_ADDRESS),
            Some(InterceptedContract::AccessControl)
        );
        assert_eq!(
            intercepted_contract(&LIMIT_CONTROL_ADDRESS),
            Some(InterceptedContract::LimitControl)
        );
        assert_eq!(
            intercepted_contract(&ORACLE_CONTRACT_ADDRESS),
            Some(InterceptedContract::Oracle)
        );
        assert_eq!(
            intercepted_contract(&KEYLESS_DEPLOY_ADDRESS),
            Some(InterceptedContract::KeylessDeploy)
        );
        assert_eq!(intercepted_contract(&HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS), None);
        assert_eq!(intercepted_contract(&SEQUENCER_REGISTRY_ADDRESS), None);
    }

    /// An address next to the range, and one that shares the last byte but not the prefix, are
    /// not intercepted: the prefix comparison is the whole address but its last byte.
    #[test]
    fn test_an_address_outside_the_range_is_not_intercepted() {
        for address in [
            address!("0x6342000000000000000000000000000000000000"),
            address!("0x6342000000000000000000000000000000000007"),
            address!("0x6342000000000000000000000000000000010004"),
            address!("0x6343000000000000000000000000000000000004"),
            address!("0x0000000000000000000000000000000000000004"),
            Address::ZERO,
        ] {
            assert_eq!(intercepted_contract(&address), None, "{address}");
        }
    }

    /// The `NonZeroTransfer()` revert data is the error both control contracts declare.
    #[test]
    fn test_non_zero_transfer_revert_data_is_the_abi_error() {
        let expected: [u8; 4] = keccak256("NonZeroTransfer()")[..4].try_into().unwrap();
        assert_eq!(NON_ZERO_TRANSFER_REVERT_DATA, expected);
        assert_eq!(NON_ZERO_TRANSFER_REVERT_DATA, IMegaLimitControl::NonZeroTransfer::SELECTOR);
    }
}
