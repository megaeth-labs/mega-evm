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

    /// @notice Thrown when the caller is not the pending admin.
    error NotPendingAdmin();

    /// @notice Thrown when a zero address is passed where a non-zero address is required.
    error ZeroAddress();

    /// @notice Thrown when activationBlock is not strictly greater than block.number.
    error InvalidActivationBlock();

    /// @notice Thrown when activationBlock exceeds uint96 range (packed ChangeRecord).
    error ActivationBlockTooLarge();

    /// @notice Thrown when a sequencer change is scheduled fewer than `minRotationDelay` blocks
    ///         before its activation block.
    error RotationDelayTooShort();

    /// @notice Thrown when the new sequencer's possession proof is malformed or was not signed
    ///         by the new sequencer key over the exact rotation being scheduled.
    error InvalidRotationProof();

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
    /// @dev `newSequencerSignature` must be a 65-byte `(r, s, v)` EIP-712 signature by
    ///      `newSequencer` over `SequencerRotation(address newSequencer,uint256 activationBlock)`,
    ///      proving the scheduled key exists and consents to this exact rotation. The activation
    ///      block must be at least `minRotationDelay()` blocks in the future. Cancelling
    ///      (`newSequencer == address(0)`, `activationBlock == type(uint256).max`) requires no
    ///      signature.
    function scheduleNextSequencerChange(
        address newSequencer,
        uint256 activationBlock,
        bytes calldata newSequencerSignature
    ) external;

    /// @notice Returns the minimum number of blocks between scheduling a sequencer change and
    ///         its activation block.
    function minRotationDelay() external view returns (uint256);

    /// @notice Returns the EIP-712 digest the new sequencer key must sign to authorize the given
    ///         rotation. Exposed so tooling can build the exact message without replicating the
    ///         domain parameters.
    function rotationDigest(address newSequencer, uint256 activationBlock) external view returns (bytes32);

    // ============================== Shared ==============================

    /// @notice Applies all pending role changes that are due. Called as a pre-block system call.
    function applyPendingChanges() external;

    /// @notice Emitted when the current admin starts a two-step transfer by setting a pending admin.
    /// @dev Also emitted with `newPendingAdmin == address(0)` when the current admin cancels a
    ///      pending transfer.
    event AdminTransferStarted(address indexed currentAdmin, address indexed newPendingAdmin);

    /// @notice Emitted when the pending admin accepts the transfer and becomes the new admin.
    event AdminTransferred(address indexed oldAdmin, address indexed newAdmin);

    /// @notice Returns the current admin address.
    function admin() external view returns (address);

    /// @notice Returns the address that is currently allowed to call `acceptAdmin`, or
    ///         `address(0)` if no transfer is pending.
    function pendingAdmin() external view returns (address);

    /// @notice Sets the pending admin to `newAdmin`. The current admin remains in effect until
    ///         `newAdmin` calls `acceptAdmin()`. Pass `address(0)` to cancel a pending transfer.
    /// @dev Two-step transfer (sets pending; does not change `admin()` immediately).
    ///      A subsequent call overwrites any previously pending admin.
    function transferAdmin(address newAdmin) external;

    /// @notice Completes a two-step admin transfer. Must be called by the address previously
    ///         passed to `transferAdmin`. Sets `admin()` to the caller and clears the pending slot.
    function acceptAdmin() external;
}
