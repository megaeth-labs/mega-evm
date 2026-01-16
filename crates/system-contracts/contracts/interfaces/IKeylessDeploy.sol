// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title InvalidReason
/// @notice Reason why a keyless deployment transaction is invalid.
enum InvalidReason {
    /// @notice The transaction data is not valid RLP encoding.
    MalformedEncoding,
    /// @notice The transaction is not a contract creation (to address is not empty).
    NotContractCreation,
    /// @notice The transaction is not pre-EIP-155 (v must be 27 or 28).
    NotPreEIP155
}

/// @title IKeylessDeploy
/// @notice Interface for the KeylessDeploy system contract.
/// @dev This contract enables deploying contracts using pre-EIP-155 transactions (Nick's Method)
/// with custom gas limits, solving the problem of contracts failing to deploy on MegaETH
/// due to the different gas model.
interface IKeylessDeploy {
    /// @notice Emitted when the keyless deployment transaction is invalid.
    /// @param reason The reason why the transaction is invalid.
    error InvalidKeylessDeploymentTransaction(InvalidReason reason);

    /// @notice Emitted when the caller tries to transfer ether to this contract.
    error NoEtherTransfer();

    /// @notice Emitted when the contract deployment fails.
    error DeploymentFailed();

    /// @notice Deploys a contract using a pre-EIP-155 signed transaction with a custom gas limit.
    /// @dev The keyless deployment transaction must be a valid RLP-encoded legacy transaction:
    ///      - nonce: any value
    ///      - gasPrice: any value (typically 100 gwei for Nick's Method)
    ///      - gasLimit: any value (will be overridden by the gas limit forwarded to this function)
    ///      - to: must be empty (contract creation)
    ///      - value: any value (typically 0)
    ///      - data: contract creation bytecode
    ///      - v: must be 27 or 28 (pre-EIP-155, no chain ID)
    ///      - r: signature component
    ///      - s: signature component
    /// @dev The gas limited forwarded to this function is the total gas limit of the transaction.
    /// @param keylessDeploymentTransaction The RLP-encoded pre-EIP-155 signed transaction.
    /// @return deployedAddress The address of the deployed contract.
    function keylessDeploy(bytes calldata keylessDeploymentTransaction)
        external returns (address deployedAddress);
}
