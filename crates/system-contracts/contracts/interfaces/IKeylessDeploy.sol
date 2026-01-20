// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title IKeylessDeploy
/// @notice Interface for the KeylessDeploy system contract.
/// @dev This contract enables deploying contracts using pre-EIP-155 transactions (Nick's Method)
/// with custom gas limits, solving the problem of contracts failing to deploy on MegaETH
/// due to the different gas model.
interface IKeylessDeploy {
    /// @notice The transaction data is not valid RLP encoding.
    error MalformedEncoding();

    /// @notice The transaction is not a contract creation (to address is not empty).
    error NotContractCreation();

    /// @notice The transaction is not pre-EIP-155 (v must be 27 or 28).
    error NotPreEIP155();

    /// @notice The caller tried to transfer ether to this contract.
    error NoEtherTransfer();

    /// @notice Failed to recover signer from signature (invalid signature).
    error InvalidSignature();

    /// @notice The signer does not have enough balance to cover gas + value.
    error InsufficientBalance();

    /// @notice The deploy address already has code (contract already exists).
    error ContractAlreadyExists();

    /// @notice The signer nonce is higher than allowed for keyless deploy.
    /// @param signerNonce The on-chain nonce of the recovered signer.
    error SignerNonceTooHigh(uint64 signerNonce);

    /// @notice The sandbox execution reverted.
    /// @param gasUsed The amount of gas used before reverting.
    /// @param output The revert output data.
    error ExecutionReverted(uint64 gasUsed, bytes output);

    /// @notice The sandbox execution halted (out of gas, stack overflow, etc.).
    /// @param gasUsed The amount of gas used before halting.
    error ExecutionHalted(uint64 gasUsed);

    /// @notice Contract creation succeeded but returned empty bytecode.
    /// @param gasUsed The amount of gas used.
    error EmptyCodeDeployed(uint64 gasUsed);

    /// @notice Contract creation succeeded but no address was returned (internal bug).
    error NoContractCreated();

    /// @notice The created contract address doesn't match the expected address (internal bug).
    error AddressMismatch();

    /// @notice The gas limit override is less than the gas limit in the keyless transaction.
    /// @param txGasLimit The gas limit from the keyless transaction.
    /// @param providedGasLimit The gas limit override provided by the caller.
    error GasLimitTooLow(uint64 txGasLimit, uint64 providedGasLimit);

    /// @notice Internal error during sandbox execution.
    /// @param message The error message.
    error InternalError(string message);

    /// @notice The call was not intercepted by the EVM (called on unsupported network).
    error NotIntercepted();

    /// @notice Deploys a contract using a pre-EIP-155 signed transaction with a custom gas limit.
    /// @dev The keyless deployment transaction must be a valid RLP-encoded legacy transaction:
    ///      - nonce: any value
    ///      - gasPrice: any value (typically 100 gwei for Nick's Method)
    ///      - gasLimit: any value (must be <= gasLimitOverride)
    ///      - to: must be empty (contract creation)
    ///      - value: any value (typically 0)
    ///      - data: contract creation bytecode
    ///      - v: must be 27 or 28 (pre-EIP-155, no chain ID)
    ///      - r: signature component
    ///      - s: signature component
    /// @param keylessDeploymentTransaction The RLP-encoded pre-EIP-155 signed transaction.
    /// @param gasLimitOverride The gas limit for the inner deployment transaction.
    ///        Must be >= the gas limit in the keyless transaction.
    /// @return gasUsed The amount of gas used by the deployment transaction execution.
    /// @return deployedAddress The address of the deployed contract.
    function keylessDeploy(bytes calldata keylessDeploymentTransaction, uint256 gasLimitOverride)
        external returns (uint64 gasUsed, address deployedAddress);
}
