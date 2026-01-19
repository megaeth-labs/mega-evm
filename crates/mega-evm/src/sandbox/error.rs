//! Error types for sandbox execution.

use alloy_primitives::Bytes;
use alloy_sol_types::SolError;
use mega_system_contracts::keyless_deploy::IKeylessDeploy;

use crate::MegaHaltReason;

/// Error types for keyless deployment operations.
///
/// These map directly to the Solidity errors defined in IKeylessDeploy.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Contract creation succeeded but no address was returned (unexpected EVM behavior)
    NoContractCreated,
    /// The created contract address doesn't match the expected address (internal bug)
    AddressMismatch,
    /// Internal error during sandbox execution
    InternalError(String),
}

/// Encodes a keyless deploy error as ABI-encoded revert data.
///
/// Uses the generated Solidity error bindings from IKeylessDeploy.sol.
pub fn encode_error_result(error: KeylessDeployError) -> Bytes {
    match error {
        KeylessDeployError::GasLimitLessThanIntrinsic { intrinsic_gas, provided_gas } => {
            IKeylessDeploy::GasLimitLessThanIntrinsic {
                intrinsicGas: intrinsic_gas,
                providedGas: provided_gas,
            }
            .abi_encode()
            .into()
        }
        KeylessDeployError::MalformedEncoding => {
            IKeylessDeploy::MalformedEncoding {}.abi_encode().into()
        }
        KeylessDeployError::NotContractCreation => {
            IKeylessDeploy::NotContractCreation {}.abi_encode().into()
        }
        KeylessDeployError::NotPreEIP155 => IKeylessDeploy::NotPreEIP155 {}.abi_encode().into(),
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
        KeylessDeployError::ExecutionReverted { gas_used, output } => {
            IKeylessDeploy::ExecutionReverted { gasUsed: gas_used, output }.abi_encode().into()
        }
        KeylessDeployError::ExecutionHalted { gas_used, .. } => {
            IKeylessDeploy::ExecutionHalted { gasUsed: gas_used }.abi_encode().into()
        }
        KeylessDeployError::NoContractCreated => {
            IKeylessDeploy::NoContractCreated {}.abi_encode().into()
        }
        KeylessDeployError::AddressMismatch => {
            IKeylessDeploy::AddressMismatch {}.abi_encode().into()
        }
        KeylessDeployError::InternalError(message) => {
            IKeylessDeploy::InternalError { message }.abi_encode().into()
        }
    }
}
