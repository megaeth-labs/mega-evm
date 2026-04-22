// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ISemver} from "./interfaces/ISemver.sol";
import {ISequencerRegistry} from "./interfaces/ISequencerRegistry.sol";

/// @title SequencerRegistry
/// @author MegaETH
/// @notice System contract for managing the active sequencer and rotation history.
/// @dev Uses compile-time constants for the initial sequencer and admin.
///      Storage slot zero-values indicate "use the constant default".
///      Deployed by mega-evm without constructor execution — initial state comes from constants.
contract SequencerRegistry is ISemver, ISequencerRegistry {
    // =========================================================================
    // Protocol constants — must match the Rust-side REX5 constants.
    // These are compiled into bytecode and verified at build time.
    // =========================================================================

    /// @notice The initial sequencer address, used when no rotation has occurred.
    /// @dev Compile-time constant — must match the Rust-side REX5 protocol constant.
    address public constant INITIAL_SEQUENCER = address(0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d);

    /// @notice The initial admin address, used when admin has not been transferred.
    /// @dev Compile-time constant — must match the Rust-side REX5 protocol constant.
    address public constant INITIAL_ADMIN = address(0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d);

    // =========================================================================
    // Storage
    // =========================================================================

    /// @dev The current sequencer. Zero means INITIAL_SEQUENCER is in effect.
    address private _currentSequencer;

    /// @dev The current admin. Zero means INITIAL_ADMIN is in effect.
    address private _admin;

    /// @dev The pending sequencer for the next rotation. Zero means no pending rotation.
    address private _pendingSequencer;

    /// @dev The block number at which the pending rotation takes effect.
    uint256 private _activationBlock;

    /// @dev A record of a sequencer rotation.
    ///      `fromBlock` is uint96 so `fromBlock` and `sequencer` pack in one storage slot.
    struct RotationRecord {
        uint96 fromBlock;
        address sequencer;
    }

    /// @dev History of applied rotations. Only written by applyPendingChange().
    RotationRecord[] private _rotations;

    // =========================================================================
    // ISemver
    // =========================================================================

    /// @notice Returns the semantic version of this contract.
    /// @return Semver string.
    function version() external pure returns (string memory) {
        return "1.0.0";
    }

    // =========================================================================
    // Read methods
    // =========================================================================

    /// @inheritdoc ISequencerRegistry
    function currentSequencer() public view returns (address) {
        address current = _currentSequencer;
        return current == address(0) ? INITIAL_SEQUENCER : current;
    }

    /// @inheritdoc ISequencerRegistry
    function admin() public view returns (address) {
        address currentAdmin = _admin;
        return currentAdmin == address(0) ? INITIAL_ADMIN : currentAdmin;
    }

    /// @inheritdoc ISequencerRegistry
    function sequencerAt(uint256 blockNumber) external view returns (address) {
        if (blockNumber > block.number) revert FutureBlock();

        // Search rotations in reverse to find the last entry where fromBlock <= blockNumber.
        uint256 len = _rotations.length;
        for (uint256 i = len; i > 0; i--) {
            RotationRecord storage record = _rotations[i - 1];
            if (record.fromBlock <= blockNumber) {
                return record.sequencer;
            }
        }

        // No rotation covers this block — return the initial sequencer.
        return INITIAL_SEQUENCER;
    }

    // =========================================================================
    // Admin methods
    // =========================================================================

    /// @dev Reverts if msg.sender is not the current admin.
    modifier onlyAdmin() {
        _onlyAdmin();
        _;
    }

    /// @dev Internal helper for onlyAdmin modifier to reduce code size.
    function _onlyAdmin() internal view {
        if (msg.sender != admin()) revert NotAdmin();
    }

    /// @inheritdoc ISequencerRegistry
    function scheduleNextSequencerChange(
        address newSequencer,
        uint256 activationBlock
    ) external onlyAdmin {
        if (activationBlock <= block.number) revert InvalidActivationBlock();

        // Cancel: activationBlock == type(uint256).max clears pending state.
        // newSequencer must be address(0) on cancel to keep the API self-consistent.
        if (activationBlock == type(uint256).max) {
            if (newSequencer != address(0)) revert ZeroAddress();
            delete _pendingSequencer;
            delete _activationBlock;
            emit SequencerChangeScheduled(currentSequencer(), address(0), type(uint256).max);
            return;
        }

        if (activationBlock > type(uint96).max) revert ActivationBlockTooLarge();
        if (newSequencer == address(0)) revert ZeroAddress();

        _pendingSequencer = newSequencer;
        _activationBlock = activationBlock;

        emit SequencerChangeScheduled(currentSequencer(), newSequencer, activationBlock);
    }

    /// @inheritdoc ISequencerRegistry
    function applyPendingChange() external {
        address pending = _pendingSequencer;
        if (pending == address(0)) return; // no pending

        uint256 activation = _activationBlock;
        if (block.number < activation) return; // not yet due

        // Apply rotation
        _currentSequencer = pending;
        _rotations.push(RotationRecord({fromBlock: uint96(activation), sequencer: pending}));

        // Clear pending state
        delete _pendingSequencer;
        delete _activationBlock;
    }

    /// @inheritdoc ISequencerRegistry
    function transferAdmin(address newAdmin) external onlyAdmin {
        if (newAdmin == address(0)) revert ZeroAddress();

        address oldAdmin = admin();
        _admin = newAdmin;

        emit AdminTransferred(oldAdmin, newAdmin);
    }
}
