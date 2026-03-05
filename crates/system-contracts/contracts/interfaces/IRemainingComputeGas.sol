// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title IRemainingComputeGas
/// @notice Interface for querying transaction-level remaining compute gas.
/// @dev The call is intercepted by MegaETH EVM.
interface IRemainingComputeGas {
    /// @notice The call was not intercepted by the EVM (called on unsupported network).
    error NotIntercepted();

    /// @notice Returns remaining transaction-level compute gas.
    /// @dev Remaining is `max(0, effective_tx_compute_limit - tx_compute_used)`.
    ///      The effective limit includes gas detention (`detained_limit`).
    /// @return remaining The remaining compute gas in the current transaction.
    function remainingComputeGas() external view returns (uint64 remaining);
}
