//! Error types for sandbox execution.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::string::String;

use alloy_primitives::Bytes;
use alloy_sol_types::SolError;
use mega_system_contracts::keyless_deploy::IKeylessDeploy;

use crate::MegaHaltReason;

/// Error types for keyless deployment operations.
///
/// These map directly to the Solidity errors defined in `IKeylessDeploy`.
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
    /// Internal error during sandbox execution
    InternalError(String),
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
        KeylessDeployError::InternalError(message) => {
            IKeylessDeploy::InternalError { message }.abi_encode().into()
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
    if let Ok(e) = IKeylessDeploy::InternalError::abi_decode(output) {
        return Some(KeylessDeployError::InternalError(e.message));
    }
    if IKeylessDeploy::NotIntercepted::abi_decode(output).is_ok() {
        return Some(KeylessDeployError::NotIntercepted);
    }
    None
}
