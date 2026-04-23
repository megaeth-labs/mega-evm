// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ISemver} from "./interfaces/ISemver.sol";
import {ISequencerRegistry} from "./interfaces/ISequencerRegistry.sol";

/// @title SequencerRegistry
/// @author MegaETH
/// @notice System contract tracking two independent roles: system address and sequencer.
/// @dev Deployed by mega-evm via raw state patch. Initial storage is seeded at deploy time
///      (no constructor execution). Due changes are applied via pre-block system call.
contract SequencerRegistry is ISemver, ISequencerRegistry {
    /// @notice The current system address used for system transactions and Oracle authorization.
    address private _currentSystemAddress;

    /// @notice The current sequencer used for mini-block signing.
    address private _currentSequencer;

    /// @notice The admin that can schedule role changes and transfer admin ownership.
    address private _admin;

    /// @notice The bootstrap system address returned before the first system address change.
    address private _initialSystemAddress;

    /// @notice The bootstrap sequencer returned before the first sequencer change.
    address private _initialSequencer;

    /// @notice The first block where this registry became valid for historical lookups.
    uint256 private _initialFromBlock;

    /// @notice The next system address waiting to be applied.
    address private _pendingSystemAddress;

    /// @notice The block at which the pending system address becomes active.
    uint256 private _systemAddressActivationBlock;

    /// @notice The next sequencer waiting to be applied.
    address private _pendingSequencer;

    /// @notice The block at which the pending sequencer becomes active.
    uint256 private _sequencerActivationBlock;

    /// @notice Historical system address changes, ordered by activation block.
    ChangeRecord[] private _systemAddressHistory;

    /// @notice Historical sequencer changes, ordered by activation block.
    ChangeRecord[] private _sequencerHistory;

    // =========================================================================
    // ISemver
    // =========================================================================

    function version() external pure returns (string memory) {
        return "1.0.0";
    }

    // =========================================================================
    // System Address Role
    // =========================================================================

    /// @inheritdoc ISequencerRegistry
    function currentSystemAddress() public view returns (address) {
        return _currentSystemAddress;
    }

    /// @inheritdoc ISequencerRegistry
    function systemAddressAt(uint256 blockNumber) external view returns (address) {
        if (blockNumber > block.number) revert FutureBlock();
        if (blockNumber < _initialFromBlock) revert BeforeInitialBlock();

        uint256 len = _systemAddressHistory.length;
        for (uint256 i = len; i > 0; i--) {
            ChangeRecord storage record = _systemAddressHistory[i - 1];
            if (record.fromBlock <= blockNumber) {
                return record.addr;
            }
        }
        return _initialSystemAddress;
    }

    /// @inheritdoc ISequencerRegistry
    function scheduleNextSystemAddressChange(
        address newSystemAddress,
        uint256 activationBlock
    ) external onlyAdmin {
        if (activationBlock <= block.number) revert InvalidActivationBlock();

        if (activationBlock == type(uint256).max) {
            if (newSystemAddress != address(0)) revert ZeroAddress();
            delete _pendingSystemAddress;
            delete _systemAddressActivationBlock;
            emit SystemAddressChangeScheduled(currentSystemAddress(), address(0), type(uint256).max);
            return;
        }

        if (activationBlock > type(uint96).max) revert ActivationBlockTooLarge();
        if (newSystemAddress == address(0)) revert ZeroAddress();

        _pendingSystemAddress = newSystemAddress;
        _systemAddressActivationBlock = activationBlock;

        emit SystemAddressChangeScheduled(currentSystemAddress(), newSystemAddress, activationBlock);
    }

    // =========================================================================
    // Sequencer Role
    // =========================================================================

    /// @inheritdoc ISequencerRegistry
    function currentSequencer() public view returns (address) {
        return _currentSequencer;
    }

    /// @inheritdoc ISequencerRegistry
    function sequencerAt(uint256 blockNumber) external view returns (address) {
        if (blockNumber > block.number) revert FutureBlock();
        if (blockNumber < _initialFromBlock) revert BeforeInitialBlock();

        uint256 len = _sequencerHistory.length;
        for (uint256 i = len; i > 0; i--) {
            ChangeRecord storage record = _sequencerHistory[i - 1];
            if (record.fromBlock <= blockNumber) {
                return record.addr;
            }
        }
        return _initialSequencer;
    }

    /// @inheritdoc ISequencerRegistry
    function scheduleNextSequencerChange(
        address newSequencer,
        uint256 activationBlock
    ) external onlyAdmin {
        if (activationBlock <= block.number) revert InvalidActivationBlock();

        if (activationBlock == type(uint256).max) {
            if (newSequencer != address(0)) revert ZeroAddress();
            delete _pendingSequencer;
            delete _sequencerActivationBlock;
            emit SequencerChangeScheduled(currentSequencer(), address(0), type(uint256).max);
            return;
        }

        if (activationBlock > type(uint96).max) revert ActivationBlockTooLarge();
        if (newSequencer == address(0)) revert ZeroAddress();

        _pendingSequencer = newSequencer;
        _sequencerActivationBlock = activationBlock;

        emit SequencerChangeScheduled(currentSequencer(), newSequencer, activationBlock);
    }

    // =========================================================================
    // Shared: apply + admin
    // =========================================================================

    /// @inheritdoc ISequencerRegistry
    function applyPendingChanges() external {
        _applySystemAddress();
        _applySequencer();
    }

    function _applySystemAddress() internal {
        address pending = _pendingSystemAddress;
        if (pending == address(0)) return;

        uint256 activation = _systemAddressActivationBlock;
        if (block.number < activation) return;

        _currentSystemAddress = pending;
        _systemAddressHistory.push(ChangeRecord({fromBlock: uint96(activation), addr: pending}));

        delete _pendingSystemAddress;
        delete _systemAddressActivationBlock;
    }

    function _applySequencer() internal {
        address pending = _pendingSequencer;
        if (pending == address(0)) return;

        uint256 activation = _sequencerActivationBlock;
        if (block.number < activation) return;

        _currentSequencer = pending;
        _sequencerHistory.push(ChangeRecord({fromBlock: uint96(activation), addr: pending}));

        delete _pendingSequencer;
        delete _sequencerActivationBlock;
    }

    /// @inheritdoc ISequencerRegistry
    function admin() public view returns (address) {
        return _admin;
    }

    /// @dev Reverts if msg.sender is not the current admin.
    modifier onlyAdmin() {
        _onlyAdmin();
        _;
    }

    function _onlyAdmin() internal view {
        if (msg.sender != admin()) revert NotAdmin();
    }

    /// @inheritdoc ISequencerRegistry
    function transferAdmin(address newAdmin) external onlyAdmin {
        if (newAdmin == address(0)) revert ZeroAddress();

        address oldAdmin = admin();
        _admin = newAdmin;

        emit AdminTransferred(oldAdmin, newAdmin);
    }
}
