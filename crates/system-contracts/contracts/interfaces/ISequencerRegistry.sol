// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ISequencerRegistry
/// @notice Interface for the SequencerRegistry system contract.
/// @dev Tracks two independent roles: system address (Oracle/system-tx authority) and
///      sequencer (mini-block signing). Each role has its own change lifecycle.
interface ISequencerRegistry {
    /// @notice Historical role-change record: packed into one slot.
    struct ChangeRecord {
        uint96 fromBlock;
        address addr;
    }

    /// @notice Thrown when a query targets a future block.
    error FutureBlock();

    /// @notice Thrown when a query targets a block before the registry was deployed.
    error BeforeInitialBlock();

    /// @notice Thrown when the caller is not the current admin.
    error NotAdmin();

    /// @notice Thrown when a zero address is passed where a non-zero address is required.
    error ZeroAddress();

    /// @notice Thrown when activationBlock is not strictly greater than block.number.
    error InvalidActivationBlock();

    /// @notice Thrown when activationBlock exceeds uint96 range (packed ChangeRecord).
    error ActivationBlockTooLarge();

    // ========================= System Address Role =========================

    /// @notice Emitted when a system address change is scheduled.
    event SystemAddressChangeScheduled(
        address indexed oldSystemAddress,
        address indexed newSystemAddress,
        uint256 activationBlock
    );

    /// @notice Returns the current effective system address.
    function currentSystemAddress() external view returns (address);

    /// @notice Returns the system address that was active at the given block number.
    function systemAddressAt(uint256 blockNumber) external view returns (address);

    /// @notice Schedules a system address change.
    function scheduleNextSystemAddressChange(address newSystemAddress, uint256 activationBlock) external;

    // ============================ Sequencer Role ============================

    /// @notice Emitted when a sequencer change is scheduled.
    event SequencerChangeScheduled(
        address indexed oldSequencer,
        address indexed newSequencer,
        uint256 activationBlock
    );

    /// @notice Returns the current effective sequencer address.
    function currentSequencer() external view returns (address);

    /// @notice Returns the sequencer that was active at the given block number.
    function sequencerAt(uint256 blockNumber) external view returns (address);

    /// @notice Schedules a sequencer change.
    function scheduleNextSequencerChange(address newSequencer, uint256 activationBlock) external;

    // ============================== Shared ==============================

    /// @notice Applies all pending role changes that are due. Called as a pre-block system call.
    function applyPendingChanges() external;

    /// @notice Emitted when the admin is transferred.
    event AdminTransferred(address indexed oldAdmin, address indexed newAdmin);

    /// @notice Returns the current admin address.
    function admin() external view returns (address);

    /// @notice Transfers admin to a new address.
    function transferAdmin(address newAdmin) external;
}
