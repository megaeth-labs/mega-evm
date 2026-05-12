//! Error types for sandbox execution.

use alloy_primitives::Bytes;
use alloy_sol_types::SolError;
use mega_system_contracts::keyless_deploy::IKeylessDeploy;

use crate::{LimitKind, MegaHaltReason};

/// Error types for keyless deployment operations.
///
/// Most variants map directly to Solidity errors defined in `IKeylessDeploy`.
/// Internal-only variants are mapped at the ABI boundary by `encode_error_result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeylessDeployError {
    /// The transaction data is malformed (invalid RLP encoding)
    MalformedEncoding,
    /// The transaction is not a contract creation (to address is not empty)
    NotContractCreation,
    /// The transaction is not pre-EIP-155 (v must be 27 or 28)
    NotPreEIP155,
    /// The nonce in the signed transaction is not zero
    NonZeroTxNonce {
        /// The nonce value in the signed transaction
        tx_nonce: u64,
    },
    /// The call tried to transfer ether (maps to `NoEtherTransfer`)
    NoEtherTransfer,
    /// Failed to recover signer from signature (invalid signature)
    InvalidSignature,
    /// The signer does not have enough balance to cover gas + value
    InsufficientBalance,
    /// The deploy address already has code (contract already exists)
    ContractAlreadyExists,
    /// The signer nonce is higher than allowed for keyless deploy
    SignerNonceTooHigh {
        /// The on-chain nonce of the recovered signer
        signer_nonce: u64,
    },
    /// The sandbox execution reverted
    ExecutionReverted {
        /// The gas used
        gas_used: u64,
        /// The output
        output: Bytes,
    },
    /// The sandbox execution halted (out of gas, stack overflow, etc.)
    ExecutionHalted {
        /// The gas used
        gas_used: u64,
        /// The reason
        reason: MegaHaltReason,
    },
    /// Rex5 preflight rejected the call: the parent's remaining TX-level budget for some
    /// dimension is smaller than the sandbox's known pre-frame intrinsic usage, so the
    /// sandbox is guaranteed to fail internally and is not started.
    ///
    /// Returned as a Revert (like other validation errors) since no sandbox execution
    /// has started and no signer state needs to persist.
    ParentBudgetExceeded {
        /// The dimension whose parent remaining is too small.
        kind: LimitKind,
        /// The parent's remaining limit for that dimension, i.e. the cap the sandbox
        /// would have been given.
        limit: u64,
        /// The sandbox's known pre-frame intrinsic usage for that dimension.
        used: u64,
    },
    /// Contract creation succeeded but returned empty bytecode
    EmptyCodeDeployed {
        /// The gas used
        gas_used: u64,
    },
    /// Contract creation succeeded but no address was returned (unexpected EVM behavior)
    NoContractCreated,
    /// The created contract address doesn't match the expected address (internal bug)
    AddressMismatch,
    /// The gas limit override is less than the gas limit in the keyless transaction
    GasLimitTooLow {
        /// The gas limit from the keyless transaction
        tx_gas_limit: u64,
        /// The gas limit override provided by the caller
        provided_gas_limit: u64,
    },
    /// The remaining compute gas is insufficient to pay for the keyless deploy overhead.
    InsufficientComputeGas {
        /// The configured compute gas limit
        limit: u64,
        /// The actual compute gas usage
        used: u64,
    },
    /// Internal sandbox failure (DB I/O, header validation, etc.).
    /// Selector-only — precompile return data is consensus-affecting, so the wire must
    /// not pin consensus to upstream revm/op-revm `Display` impls.
    InternalError,
    /// Sandbox rejected the inner transaction as a tx-validation error
    /// (`IsTxError::is_tx_error() == true`). A dedicated selector lets relayer-side
    /// decoders distinguish this from a genuine internal failure. Selector-only for the
    /// same consensus-decoupling reason as `InternalError`.
    InvalidTransaction,
    /// The keylessDeploy call was not intercepted (only returned by Solidity contract for inner
    /// calls)
    NotIntercepted,
}

/// Encodes a keyless deploy error as ABI-encoded revert data.
///
/// Uses the generated Solidity error bindings from IKeylessDeploy.sol.
pub fn encode_error_result(error: KeylessDeployError) -> Bytes {
    match error {
        KeylessDeployError::MalformedEncoding => {
            IKeylessDeploy::MalformedEncoding {}.abi_encode().into()
        }
        KeylessDeployError::NotContractCreation => {
            IKeylessDeploy::NotContractCreation {}.abi_encode().into()
        }
        KeylessDeployError::NotPreEIP155 => IKeylessDeploy::NotPreEIP155 {}.abi_encode().into(),
        KeylessDeployError::NonZeroTxNonce { tx_nonce } => {
            IKeylessDeploy::NonZeroTxNonce { txNonce: tx_nonce }.abi_encode().into()
        }
        KeylessDeployError::NoEtherTransfer => {
            IKeylessDeploy::NoEtherTransfer {}.abi_encode().into()
        }
        KeylessDeployError::InvalidSignature => {
            IKeylessDeploy::InvalidSignature {}.abi_encode().into()
        }
        KeylessDeployError::InsufficientBalance => {
            IKeylessDeploy::InsufficientBalance {}.abi_encode().into()
        }
        KeylessDeployError::ContractAlreadyExists => {
            IKeylessDeploy::ContractAlreadyExists {}.abi_encode().into()
        }
        KeylessDeployError::SignerNonceTooHigh { signer_nonce } => {
            IKeylessDeploy::SignerNonceTooHigh { signerNonce: signer_nonce }.abi_encode().into()
        }
        KeylessDeployError::ExecutionReverted { gas_used, output } => {
            IKeylessDeploy::ExecutionReverted { gasUsed: gas_used, output }.abi_encode().into()
        }
        KeylessDeployError::ExecutionHalted { gas_used, .. } => {
            IKeylessDeploy::ExecutionHalted { gasUsed: gas_used }.abi_encode().into()
        }
        KeylessDeployError::ParentBudgetExceeded { kind, limit, used } => {
            IKeylessDeploy::ParentBudgetExceeded { kind: kind.as_u8(), limit, used }
                .abi_encode()
                .into()
        }
        KeylessDeployError::EmptyCodeDeployed { gas_used } => {
            IKeylessDeploy::EmptyCodeDeployed { gasUsed: gas_used }.abi_encode().into()
        }
        KeylessDeployError::NoContractCreated => {
            IKeylessDeploy::NoContractCreated {}.abi_encode().into()
        }
        KeylessDeployError::AddressMismatch => {
            IKeylessDeploy::AddressMismatch {}.abi_encode().into()
        }
        KeylessDeployError::GasLimitTooLow { tx_gas_limit, provided_gas_limit } => {
            IKeylessDeploy::GasLimitTooLow {
                txGasLimit: tx_gas_limit,
                providedGasLimit: provided_gas_limit,
            }
            .abi_encode()
            .into()
        }
        KeylessDeployError::InsufficientComputeGas { limit, used } => {
            IKeylessDeploy::InsufficientComputeGas { limit, used }.abi_encode().into()
        }
        KeylessDeployError::InternalError => IKeylessDeploy::InternalError {}.abi_encode().into(),
        KeylessDeployError::InvalidTransaction => {
            IKeylessDeploy::InvalidTransaction {}.abi_encode().into()
        }
        KeylessDeployError::NotIntercepted => IKeylessDeploy::NotIntercepted {}.abi_encode().into(),
    }
}

/// Decodes ABI-encoded revert data into a `KeylessDeployError`.
///
/// Returns `None` if the data doesn't match any known error format.
///
/// Note: For `ExecutionHalted`, the halt reason cannot be recovered from ABI encoding,
/// so a default `OutOfGas` reason is used.
pub fn decode_error_result(output: &[u8]) -> Option<KeylessDeployError> {
    if IKeylessDeploy::NoEtherTransfer::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::NoEtherTransfer);
    }
    if IKeylessDeploy::MalformedEncoding::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::MalformedEncoding);
    }
    if IKeylessDeploy::NotContractCreation::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::NotContractCreation);
    }
    if IKeylessDeploy::NotPreEIP155::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::NotPreEIP155);
    }
    if let Ok(e) = IKeylessDeploy::NonZeroTxNonce::abi_decode(output) {
        return Some(KeylessDeployError::NonZeroTxNonce { tx_nonce: e.txNonce });
    }
    if IKeylessDeploy::InvalidSignature::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::InvalidSignature);
    }
    if IKeylessDeploy::InsufficientBalance::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::InsufficientBalance);
    }
    if IKeylessDeploy::ContractAlreadyExists::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::ContractAlreadyExists);
    }
    if let Ok(e) = IKeylessDeploy::SignerNonceTooHigh::abi_decode(output) {
        return Some(KeylessDeployError::SignerNonceTooHigh { signer_nonce: e.signerNonce });
    }
    if let Ok(e) = IKeylessDeploy::ExecutionReverted::abi_decode(output) {
        return Some(KeylessDeployError::ExecutionReverted {
            gas_used: e.gasUsed,
            output: e.output,
        });
    }
    if let Ok(e) = IKeylessDeploy::ExecutionHalted::abi_decode(output) {
        // Note: The actual halt reason is lost in ABI encoding, use OutOfGas as placeholder
        return Some(KeylessDeployError::ExecutionHalted {
            gas_used: e.gasUsed,
            reason: MegaHaltReason::Base(op_revm::OpHaltReason::Base(
                revm::context::result::HaltReason::OutOfGas(
                    revm::context::result::OutOfGasError::Basic,
                ),
            )),
        });
    }
    if let Ok(e) = IKeylessDeploy::ParentBudgetExceeded::abi_decode(output) {
        return Some(KeylessDeployError::ParentBudgetExceeded {
            kind: LimitKind::from_u8(e.kind)?,
            limit: e.limit,
            used: e.used,
        });
    }
    if let Ok(e) = IKeylessDeploy::EmptyCodeDeployed::abi_decode(output) {
        return Some(KeylessDeployError::EmptyCodeDeployed { gas_used: e.gasUsed });
    }
    if IKeylessDeploy::NoContractCreated::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::NoContractCreated);
    }
    if IKeylessDeploy::AddressMismatch::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::AddressMismatch);
    }
    if let Ok(e) = IKeylessDeploy::GasLimitTooLow::abi_decode(output) {
        return Some(KeylessDeployError::GasLimitTooLow {
            tx_gas_limit: e.txGasLimit,
            provided_gas_limit: e.providedGasLimit,
        });
    }
    if IKeylessDeploy::InvalidTransaction::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::InvalidTransaction);
    }
    if IKeylessDeploy::InternalError::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::InternalError);
    }
    if IKeylessDeploy::NotIntercepted::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::NotIntercepted);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `InternalError` and `InvalidTransaction` carry no payload on the wire, so a
    /// roundtrip MUST produce the same variant. Pinned because both selectors are
    /// reachable on-chain via `RETURNDATACOPY` and any divergence between
    /// `encode_error_result` and `decode_error_result` would break consensus.
    #[test]
    fn test_internal_error_roundtrip_is_selector_only() {
        let encoded = encode_error_result(KeylessDeployError::InternalError);
        // Selector-only: the encoded form is exactly the 4-byte Solidity selector.
        assert_eq!(encoded.len(), 4, "InternalError must be selector-only");
        assert!(matches!(decode_error_result(&encoded), Some(KeylessDeployError::InternalError)));
    }

    #[test]
    fn test_invalid_transaction_roundtrip_is_selector_only() {
        let encoded = encode_error_result(KeylessDeployError::InvalidTransaction);
        assert_eq!(encoded.len(), 4, "InvalidTransaction must be selector-only");
        assert!(matches!(
            decode_error_result(&encoded),
            Some(KeylessDeployError::InvalidTransaction)
        ));
    }
}
