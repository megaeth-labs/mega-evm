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

    /// @notice The address allowed to call `acceptAdmin()` to complete a two-step admin transfer,
    ///         or `address(0)` if no transfer is pending.
    address private _pendingAdmin;

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

    /// @notice The minimum number of blocks between scheduling a sequencer change and its
    ///         activation block. Seeded at deploy/upgrade time (no constructor execution);
    ///         the contract exposes no setter, so changing it requires a bytecode upgrade.
    uint256 private _minRotationDelay;

    /// @notice EIP-712 domain type hash used for sequencer rotation proofs.
    bytes32 private constant EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");

    /// @notice EIP-712 struct type hash of the rotation message signed by the new sequencer key.
    bytes32 private constant SEQUENCER_ROTATION_TYPEHASH =
        keccak256("SequencerRotation(address newSequencer,uint256 activationBlock)");

    /// @notice Upper bound for the `s` value of a valid signature (secp256k1n / 2). Signatures
    ///         with a higher `s` are malleable duplicates and are rejected.
    uint256 private constant SECP256K1N_HALF =
        0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0;

    // =========================================================================
    // ISemver
    // =========================================================================

    function version() external pure returns (string memory) {
        return "2.0.0";
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
        uint256 activationBlock,
        bytes calldata newSequencerSignature
    ) external onlyAdmin {
        if (activationBlock <= block.number) revert InvalidActivationBlock();

        if (activationBlock == type(uint256).max) {
            // Cancel: clears the pending change without requiring a possession proof — the
            // signature parameter is ignored so a lost new key can never block a cancel.
            if (newSequencer != address(0)) revert ZeroAddress();
            delete _pendingSequencer;
            delete _sequencerActivationBlock;
            emit SequencerChangeScheduled(currentSequencer(), address(0), type(uint256).max);
            return;
        }

        if (activationBlock > type(uint96).max) revert ActivationBlockTooLarge();
        if (newSequencer == address(0)) revert ZeroAddress();
        if (activationBlock < block.number + _minRotationDelay) revert RotationDelayTooShort();

        // Possession proof: the new sequencer key must have signed this exact rotation.
        // `ecrecover` runs last so every cheap validation failure is caught first.
        address signer =
            _recoverRotationSigner(rotationDigest(newSequencer, activationBlock), newSequencerSignature);
        if (signer != newSequencer) revert InvalidRotationProof();

        _pendingSequencer = newSequencer;
        _sequencerActivationBlock = activationBlock;

        emit SequencerChangeScheduled(currentSequencer(), newSequencer, activationBlock);
    }

    /// @inheritdoc ISequencerRegistry
    function minRotationDelay() external view returns (uint256) {
        return _minRotationDelay;
    }

    /// @inheritdoc ISequencerRegistry
    /// @dev The domain separator is computed on demand instead of being cached at construction
    ///      because this contract is installed via raw state patch and no constructor ever runs.
    function rotationDigest(address newSequencer, uint256 activationBlock) public view returns (bytes32) {
        bytes32 domainSeparator = keccak256(
            abi.encode(
                EIP712_DOMAIN_TYPEHASH,
                keccak256(bytes("MegaETH SequencerRegistry")),
                keccak256(bytes("1")),
                block.chainid,
                address(this)
            )
        );
        bytes32 structHash = keccak256(abi.encode(SEQUENCER_ROTATION_TYPEHASH, newSequencer, activationBlock));
        return keccak256(abi.encodePacked("\x19\x01", domainSeparator, structHash));
    }

    /// @dev Recovers the signer of `digest` from a 65-byte `(r, s, v)` signature. Reverts with
    ///      `InvalidRotationProof` on any malformed input: wrong length, malleable high-`s`,
    ///      `v` outside `{27, 28}`, or a failed recovery.
    function _recoverRotationSigner(bytes32 digest, bytes calldata signature) private pure returns (address) {
        if (signature.length != 65) revert InvalidRotationProof();

        bytes32 r = bytes32(signature[0:32]);
        bytes32 s = bytes32(signature[32:64]);
        uint8 v = uint8(signature[64]);

        if (uint256(s) > SECP256K1N_HALF) revert InvalidRotationProof();
        if (v != 27 && v != 28) revert InvalidRotationProof();

        address signer = ecrecover(digest, v, r, s);
        if (signer == address(0)) revert InvalidRotationProof();
        return signer;
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

    /// @inheritdoc ISequencerRegistry
    function pendingAdmin() public view returns (address) {
        return _pendingAdmin;
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
    /// @dev Two-step transfer: sets `_pendingAdmin` and does NOT change `_admin`. The new admin
    ///      becomes effective only when they call `acceptAdmin()`. Passing `address(0)` cancels
    ///      any previously pending transfer. A subsequent call overwrites the pending slot.
    function transferAdmin(address newAdmin) external onlyAdmin {
        _pendingAdmin = newAdmin;
        emit AdminTransferStarted(admin(), newAdmin);
    }

    /// @inheritdoc ISequencerRegistry
    function acceptAdmin() external {
        address pending = _pendingAdmin;
        if (msg.sender != pending) revert NotPendingAdmin();

        address oldAdmin = _admin;
        _admin = pending;
        delete _pendingAdmin;

        emit AdminTransferred(oldAdmin, pending);
    }
}
