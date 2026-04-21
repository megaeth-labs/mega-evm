// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ISequencerRegistry
/// @notice Interface for the SequencerRegistry system contract.
/// @dev Records the current sequencer, pending rotations, and rotation history.
///      The contract uses compile-time constants for the initial sequencer and admin.
///      `address(0)` in storage means "use the constant default".
interface ISequencerRegistry {
    /// @notice Thrown when a query targets a future block.
    error FutureBlock();

    /// @notice Thrown when the caller is not the current admin.
    error NotAdmin();

    /// @notice Thrown when a zero address is passed where a non-zero address is required.
    error ZeroAddress();

    /// @notice Thrown when activationBlock is not strictly greater than block.number.
    error InvalidActivationBlock();

    /// @notice Thrown when activationBlock exceeds uint96.
    error ActivationBlockTooLarge();

    /// @notice Emitted when a sequencer rotation is scheduled.
    /// @param oldSequencer The current sequencer at the time of scheduling.
    /// @param newSequencer The new sequencer that will take effect at activationBlock.
    /// @param activationBlock The block number at which the new sequencer takes effect.
    event SequencerChangeScheduled(
        address indexed oldSequencer,
        address indexed newSequencer,
        uint256 activationBlock
    );

    /// @notice Emitted when the admin is transferred.
    /// @param oldAdmin The previous admin.
    /// @param newAdmin The new admin.
    event AdminTransferred(address indexed oldAdmin, address indexed newAdmin);

    /// @notice Returns the current effective sequencer address.
    /// @dev If no rotation has ever occurred, returns the compile-time INITIAL_SEQUENCER constant.
    /// @return The current sequencer address.
    function currentSequencer() external view returns (address);

    /// @notice Returns the sequencer that was active at the given block number.
    /// @dev Reverts with FutureBlock if blockNumber > block.number.
    ///      If no rotation record covers the queried block, returns INITIAL_SEQUENCER.
    /// @param blockNumber The block number to query.
    /// @return The sequencer address active at that block.
    function sequencerAt(uint256 blockNumber) external view returns (address);

    /// @notice Schedules a sequencer rotation starting at activationBlock.
    /// @dev Only callable by the current admin. At most one pending schedule exists at a time.
    ///      A new schedule overwrites any previous pending schedule.
    ///      To cancel a pending schedule, set activationBlock to type(uint256).max.
    /// @param newSequencer The address of the new sequencer. Must be non-zero for non-cancel.
    /// @param activationBlock Must be strictly greater than block.number.
    function scheduleNextSequencerChange(address newSequencer, uint256 activationBlock) external;

    /// @notice Applies a pending sequencer change if it is due.
    /// @dev Permissionless. Called by mega-evm as a pre-block system call.
    ///      No-op if no pending change exists or block.number < activationBlock.
    ///      When applied: updates _currentSequencer, appends to rotation history, clears pending.
    function applyPendingChange() external;

    /// @notice Returns the current effective admin address.
    /// @dev If admin has never been transferred, returns the compile-time INITIAL_ADMIN constant.
    /// @return The current admin address.
    function admin() external view returns (address);

    /// @notice Transfers admin to a new address.
    /// @dev Only callable by the current admin. newAdmin must be non-zero.
    /// @param newAdmin The new admin address.
    function transferAdmin(address newAdmin) external;
}
