//! Error types of keyless deployment.

use alloy_primitives::Bytes;
use alloy_sol_types::SolError;
use mega_system_contracts::keyless_deploy::IKeylessDeploy;

use crate::MegaHaltReason;

/// Error types for keyless deployment operations.
///
/// Every variant maps to the Solidity error of the same name in `IKeylessDeploy`;
/// [`encode_error_result`] and [`decode_error_result`] convert at the ABI boundary.
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
    /// The signer cannot fund the deployment's value. The deployment pays no gas out of the
    /// signer's balance, so the value is all it must hold.
    InsufficientBalance,
    /// The deploy address already has code (contract already exists)
    ContractAlreadyExists,
    /// The signer nonce is higher than allowed for keyless deploy
    SignerNonceTooHigh {
        /// The on-chain nonce of the recovered signer
        signer_nonce: u64,
    },
    /// The deployment's creation reverted.
    ExecutionReverted {
        /// The gas used
        gas_used: u64,
        /// The output
        output: Bytes,
    },
    /// The deployment's creation halted (out of gas, an invalid opcode, code the creation
    /// refuses).
    ExecutionHalted {
        /// The gas used
        gas_used: u64,
        /// The reason
        reason: MegaHaltReason,
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
    /// The keyless transaction's init code exceeds the configured maximum init code size
    /// (EIP-3860), which no validation of the outer transaction holds it to.
    InitCodeTooLarge {
        /// The init code length in bytes.
        size: u64,
        /// The configured max init code size.
        max: u64,
    },
    /// The recovered signer has non-empty, non-EIP-7702 bytecode: EIP-3607 for a creator that
    /// is not the transaction's sender, which no validation of the outer transaction checks.
    SignerHasCode,
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
        KeylessDeployError::InitCodeTooLarge { size, max } => {
            IKeylessDeploy::InitCodeTooLarge { size, max }.abi_encode().into()
        }
        KeylessDeployError::SignerHasCode => IKeylessDeploy::SignerHasCode {}.abi_encode().into(),
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
            reason: MegaHaltReason::Base(revm::context::result::HaltReason::OutOfGas(
                revm::context::result::OutOfGasError::Basic,
            )),
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
    if let Ok(e) = IKeylessDeploy::InitCodeTooLarge::abi_decode(output) {
        return Some(KeylessDeployError::InitCodeTooLarge { size: e.size, max: e.max });
    }
    if IKeylessDeploy::SignerHasCode::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::SignerHasCode);
    }
    if IKeylessDeploy::NotIntercepted::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::NotIntercepted);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `InitCodeTooLarge { size, max }` is an externally visible ABI selector. Pinning the
    /// round-trip catches any drift between the Solidity error definition and the Rust
    /// encode/decode (e.g. forgotten arm in `encode_error_result` or `decode_error_result`,
    /// or accidental field reordering).
    #[test]
    fn test_init_code_too_large_roundtrip_preserves_size_and_max() {
        let original = KeylessDeployError::InitCodeTooLarge { size: 600_000, max: 548_864 };
        let encoded = encode_error_result(original.clone());
        let decoded = decode_error_result(&encoded).expect("must decode");
        assert_eq!(decoded, original);
    }

    /// `SignerHasCode` is selector-only; pinning the round-trip catches arm drift.
    #[test]
    fn test_signer_has_code_roundtrip_is_selector_only() {
        let encoded = encode_error_result(KeylessDeployError::SignerHasCode);
        assert_eq!(encoded.len(), 4, "SignerHasCode must be selector-only");
        assert!(matches!(decode_error_result(&encoded), Some(KeylessDeployError::SignerHasCode)));
    }
}
