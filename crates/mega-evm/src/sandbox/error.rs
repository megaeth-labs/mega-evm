//! Error types for sandbox execution.

use alloy_primitives::{Address, Bytes};
use mega_system_contracts::keyless_deploy::InvalidReason;

/// Error types for keyless deployment operations.
///
/// These map directly to the Solidity errors defined in IKeylessDeploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeylessDeployError {
    /// The gas limit for sandbox execution is too low
    GasLimitLessThanIntrinsic {
        /// The intrinsic gas required for the operation
        intrinsic_gas: u64,
        /// The gas limit provided by the caller
        provided_gas: u64,
    },
    /// The transaction data is malformed (invalid RLP encoding)
    MalformedEncoding,
    /// The transaction is not a contract creation (to address is not empty)
    NotContractCreation,
    /// The transaction is not pre-EIP-155 (v must be 27 or 28)
    NotPreEIP155,
    /// The call tried to transfer ether (maps to NoEtherTransfer)
    NoEtherTransfer,
    /// Failed to recover signer from signature (invalid signature)
    InvalidSignature,
    /// The signer does not have enough balance to cover gas + value
    InsufficientBalance,
    /// The deploy address already has code (contract already exists)
    ContractAlreadyExists,
    /// The sandbox execution reverted
    ExecutionReverted,
    /// The sandbox execution halted (out of gas, stack overflow, etc.)
    ExecutionHalted,
    /// Contract creation succeeded but no address was returned (unexpected EVM behavior)
    NoContractCreated,
    /// The created contract address doesn't match the expected address (internal bug)
    AddressMismatch,
    /// Internal database error during sandbox execution
    DatabaseError,
}

impl From<InvalidReason> for KeylessDeployError {
    fn from(reason: InvalidReason) -> Self {
        match reason {
            InvalidReason::MalformedEncoding => KeylessDeployError::MalformedEncoding,
            InvalidReason::NotContractCreation => KeylessDeployError::NotContractCreation,
            InvalidReason::NotPreEIP155 => KeylessDeployError::NotPreEIP155,
            _ => KeylessDeployError::ExecutionReverted, // Fallback for any new variants
        }
    }
}

impl KeylessDeployError {
    /// Converts this error to the corresponding Solidity InvalidReason if applicable.
    pub fn to_invalid_reason(self) -> Option<InvalidReason> {
        match self {
            KeylessDeployError::MalformedEncoding => Some(InvalidReason::MalformedEncoding),
            KeylessDeployError::NotContractCreation => Some(InvalidReason::NotContractCreation),
            KeylessDeployError::NotPreEIP155 => Some(InvalidReason::NotPreEIP155),
            _ => None,
        }
    }
}

/// Encodes a successful keyless deploy result as ABI-encoded bytes.
///
/// The return type is `address`, which is ABI-encoded as a 32-byte value
/// with the address right-aligned (first 12 bytes are zeros).
pub fn encode_success_result(deployed_address: Address) -> Bytes {
    let mut result = [0u8; 32];
    result[12..].copy_from_slice(deployed_address.as_slice());
    Bytes::copy_from_slice(&result)
}

/// DeploymentFailed() error selector.
/// selector: keccak256("DeploymentFailed()")[:4] = 0x30116425
const DEPLOYMENT_FAILED_SELECTOR: [u8; 4] = [0x30, 0x11, 0x64, 0x25];

/// Encodes a keyless deploy error as ABI-encoded revert data.
///
/// This matches the Solidity error selectors from IKeylessDeploy.sol.
pub fn encode_error_result(error: KeylessDeployError) -> Bytes {
    match error {
        KeylessDeployError::MalformedEncoding => {
            // InvalidKeylessDeploymentTransaction(InvalidReason.MalformedEncoding)
            // selector: keccak256("InvalidKeylessDeploymentTransaction(uint8)")[:4]
            // = 0x5a3c9cf3
            let mut data = Vec::with_capacity(36);
            data.extend_from_slice(&[0x5a, 0x3c, 0x9c, 0xf3]); // selector
            data.extend_from_slice(&[0u8; 31]); // padding
            data.push(0); // MalformedEncoding = 0
            Bytes::from(data)
        }
        KeylessDeployError::NotContractCreation => {
            // InvalidKeylessDeploymentTransaction(InvalidReason.NotContractCreation)
            let mut data = Vec::with_capacity(36);
            data.extend_from_slice(&[0x5a, 0x3c, 0x9c, 0xf3]); // selector
            data.extend_from_slice(&[0u8; 31]); // padding
            data.push(1); // NotContractCreation = 1
            Bytes::from(data)
        }
        KeylessDeployError::NotPreEIP155 => {
            // InvalidKeylessDeploymentTransaction(InvalidReason.NotPreEIP155)
            let mut data = Vec::with_capacity(36);
            data.extend_from_slice(&[0x5a, 0x3c, 0x9c, 0xf3]); // selector
            data.extend_from_slice(&[0u8; 31]); // padding
            data.push(2); // NotPreEIP155 = 2
            Bytes::from(data)
        }
        KeylessDeployError::NoEtherTransfer => {
            // NoEtherTransfer()
            // selector: keccak256("NoEtherTransfer()")[:4]
            // = 0x6a12f104
            Bytes::copy_from_slice(&[0x6a, 0x12, 0xf1, 0x04])
        }
        // All other errors map to DeploymentFailed() since the Solidity interface
        // doesn't define specific errors for them
        KeylessDeployError::GasLimitLessThanIntrinsic { .. } |
        KeylessDeployError::InvalidSignature |
        KeylessDeployError::InsufficientBalance |
        KeylessDeployError::ContractAlreadyExists |
        KeylessDeployError::ExecutionReverted |
        KeylessDeployError::ExecutionHalted |
        KeylessDeployError::NoContractCreated |
        KeylessDeployError::AddressMismatch |
        KeylessDeployError::DatabaseError => Bytes::copy_from_slice(&DEPLOYMENT_FAILED_SELECTOR),
    }
}
