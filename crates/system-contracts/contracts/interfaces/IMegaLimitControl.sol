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

    /// @notice Returns the compute gas the calling frame can still spend.
    /// @dev The figure is read when the call reaches the EVM, after the calling opcode charged its
    ///      own costs. The gas forwarded to this call comes back untouched, so it is not deducted.
    ///      How compute gas is metered, and what limits it, is the active spec's.
    /// @return remaining The compute gas the calling frame can still spend.
    function remainingComputeGas() external view returns (uint64 remaining);
}
