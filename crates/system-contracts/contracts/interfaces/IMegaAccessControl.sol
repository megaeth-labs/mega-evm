// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title IMegaAccessControl
/// @notice Interface for the MegaAccessControl system contract.
/// @dev This contract provides functions to control resource limits during EVM execution.
///      Functions are intercepted by the MegaETH EVM and enforced at the execution level.
interface IMegaAccessControl {
    /// @notice Enum identifying the type of volatile data that was accessed.
    /// @dev Discriminant values match the bit positions in the VolatileDataAccess bitmap.
    enum VolatileDataAccessType {
        BlockNumber,        // 0  — NUMBER opcode
        Timestamp,          // 1  — TIMESTAMP opcode
        Coinbase,           // 2  — COINBASE opcode
        Difficulty,         // 3  — DIFFICULTY opcode
        GasLimit,           // 4  — GASLIMIT opcode
        BaseFee,            // 5  — BASEFEE opcode
        PrevRandao,         // 6  — PREVRANDAO opcode
        BlockHash,          // 7  — BLOCKHASH opcode
        BlobBaseFee,        // 8  — BLOBBASEFEE opcode
        BlobHash,           // 9  — BLOBHASH opcode
        Beneficiary,        // 10 — BALANCE/EXTCODESIZE/EXTCODECOPY/EXTCODEHASH/CALL/STATICCALL/DELEGATECALL/CALLCODE on beneficiary
        Oracle              // 11 — SLOAD on oracle contract
    }

    /// @notice The call was not intercepted by the EVM (called on unsupported network).
    error NotIntercepted();

    /// @notice Volatile data access was attempted in a frame where it is disabled.
    /// @param accessType The type of volatile data that was accessed.
    error VolatileDataAccessDisabled(VolatileDataAccessType accessType);

    /// @notice Re-enabling was attempted but a parent frame disabled access.
    error DisabledByParent();

    /// @notice Disables volatile data access for the caller's frame and all inner calls.
    /// @dev When called, the caller's own frame and any inner
    ///      CALL/STATICCALL/DELEGATECALL/CALLCODE that accesses volatile data
    ///      (block env fields, beneficiary balance, oracle) will revert with
    ///      `VolatileDataAccessDisabled()`.
    ///      Available from Rex4 hardfork.
    function disableVolatileDataAccess() external view;

    /// @notice Re-enables volatile data access for the caller's frame and inner calls.
    /// @dev Succeeds if access is not disabled, or the caller (or an ancestor at the
    ///      same depth) disabled it. Reverts with `DisabledByParent()` if a parent
    ///      frame disabled access.
    ///      Available from Rex4 hardfork.
    function enableVolatileDataAccess() external view;

    /// @notice Queries whether volatile data access is disabled at the current call depth.
    /// @return disabled True if volatile data access is disabled.
    function isVolatileDataAccessDisabled() external view returns (bool disabled);
}
