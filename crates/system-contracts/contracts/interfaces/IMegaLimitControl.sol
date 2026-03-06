// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title IMegaLimitControl
/// @notice Interface for limit-related control and query methods.
/// @dev The call is intercepted by MegaETH EVM.
interface IMegaLimitControl {
    /// @notice The call was not intercepted by the EVM (called on unsupported network).
    error NotIntercepted();
    /// @notice The call carries non-zero transferred ETH for a view/control method.
    error NonZeroTransfer();

    /// @notice Returns remaining transaction-level compute gas.
    /// @dev Remaining is `max(0, effective_tx_compute_limit - tx_compute_used)`.
    ///      The effective limit includes gas detention (`detained_limit`).
    /// @return remaining The remaining compute gas in the current transaction.
    function remainingComputeGas() external view returns (uint64 remaining);
}
