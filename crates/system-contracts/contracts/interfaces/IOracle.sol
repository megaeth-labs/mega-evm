// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title IOracle
/// @notice Interface for the Oracle system contract.
/// @dev The Oracle contract provides key-value storage and logging capabilities for MegaETH.
/// It allows contracts to read and write arbitrary data indexed by slot numbers.
interface IOracle {
    /// @notice Executes multiple calls in a single transaction.
    /// @param data Array of encoded function calls to execute.
    /// @return results Array of return values from each call.
    function multiCall(bytes[] memory data) external returns (bytes[] memory results);

    /// @notice Reads a single storage slot.
    /// @param slot The slot number to read.
    /// @return value The 32-byte value stored at the slot.
    function getSlot(uint256 slot) external view returns (bytes32 value);

    /// @notice Writes a value to a single storage slot.
    /// @param slot The slot number to write to.
    /// @param value The 32-byte value to store.
    function setSlot(uint256 slot, bytes32 value) external;

    /// @notice Reads multiple storage slots in a single call.
    /// @param slots Array of slot numbers to read.
    /// @return values Array of 32-byte values stored at each slot.
    function getSlots(uint256[] memory slots) external view returns (bytes32[] memory values);

    /// @notice Writes values to multiple storage slots in a single call.
    /// @param slots Array of slot numbers to write to.
    /// @param values Array of 32-byte values to store at each slot.
    function setSlots(uint256[] memory slots, bytes32[] memory values) external;

    /// @notice Sends a hint to the oracle service backend.
    /// @dev This is a view function that doesn't modify state but is intercepted by the EVM
    /// to forward hints to the oracle service. Available from Rex2 hardfork.
    /// @param topic A bytes32 topic identifier for the hint.
    /// @param data Arbitrary data payload for the hint.
    function sendHint(bytes32 topic, bytes memory data) external view;

    /// @notice Emits a single log entry with the given topic and data.
    /// @param topic A bytes32 topic identifier for the log.
    /// @param data Arbitrary data payload for the log.
    function emitLog(bytes32 topic, bytes memory data) external;

    /// @notice Emits multiple log entries with the same topic but different data.
    /// @param topic A bytes32 topic identifier for all logs.
    /// @param dataVector Array of data payloads, one per log entry.
    function emitLogs(bytes32 topic, bytes[] memory dataVector) external;
}
