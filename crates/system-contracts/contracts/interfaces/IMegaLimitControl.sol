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

    /// @notice Returns remaining compute gas of the current call.
    /// @dev In Rex4+, returns the caller's per-frame remaining compute gas (not the 98/100 forwarded child budget).
    /// @dev For direct TX calls with no active frame, returns TX compute limit minus intrinsic compute gas.
    /// @dev In pre-Rex4, this falls back to transaction-level remaining compute gas.
    /// @return remaining The remaining compute gas of the current call.
    function remainingComputeGas() external view returns (uint64 remaining);
}
