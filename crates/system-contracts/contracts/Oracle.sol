// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ISemver} from "./interfaces/ISemver.sol";
import {IOracle} from "./interfaces/IOracle.sol";

/// @title Oracle
/// @author MegaETH
/// @notice Oracle provides a simple interface to directly read and set storage slots.
contract Oracle is ISemver, IOracle {
    /// @notice The address authorized to modify oracle data.
    /// @dev Only this address can call setter functions.
    address public immutable MEGA_SYSTEM_ADDRESS;

    /// @notice Thrown when a non-system address attempts to call a restricted function.
    error NotSystemAddress();

    /// @notice Thrown when array lengths don't match in batch operations.
    /// @param slotsLength The length of the slots array.
    /// @param valuesLength The length of the values array.
    error InvalidLength(uint256 slotsLength, uint256 valuesLength);

    /// @notice Emitted when a log is emitted by the oracle contract.
    /// @param topic A user-defined identifier for the type of log (e.g., event category).
    /// @param data Arbitrary data to include in the log.
    event Log(bytes32 indexed topic, bytes data);

    /// @notice Restricts function access to the system address only.
    /// @dev Reverts with NotSystemAddress if caller is not MEGA_SYSTEM_ADDRESS.
    modifier onlySystemAddress() {
        _;
        // This check is placed after the _; to facilitate off-chain simulation.
        // EVM inspector will be able to see the execution trace even if the sender is not the system address.
        _onlySystemAddress();
    }

    /// @notice Checks if the caller is the system address.
    /// @dev Reverts with NotSystemAddress if caller is not MEGA_SYSTEM_ADDRESS.
    function _onlySystemAddress() internal view {
        if (msg.sender != MEGA_SYSTEM_ADDRESS) revert NotSystemAddress();
    }

    /// @notice Returns the semantic version of this contract.
    /// @return version string in semver format.
    function version() external pure returns (string memory) {
        return "1.1.0";
    }

    /// @notice Initializes the Oracle contract with the system address.
    /// @param _megaSystemAddress The address authorized to modify oracle data.
    constructor(address _megaSystemAddress) {
        MEGA_SYSTEM_ADDRESS = _megaSystemAddress;
    }

    /// @inheritdoc IOracle
    function multiCall(bytes[] calldata data) external returns (bytes[] memory results) {
        results = new bytes[](data.length);
        for (uint256 i = 0; i < data.length;) {
            (bool success, bytes memory result) = address(this).delegatecall(data[i]);
            if (!success) {
                // Bubble up the revert reason
                if (result.length > 0) {
                    assembly {
                        revert(add(32, result), mload(result))
                    }
                } else {
                    revert("Multicall: call failed");
                }
            }
            results[i] = result;
            unchecked {
                ++i;
            }
        }
    }

    /// @inheritdoc IOracle
    function sendHint(bytes32 topic, bytes calldata data) external view {}

    /// @inheritdoc IOracle
    function emitLog(bytes32 topic, bytes calldata data) public onlySystemAddress {
        _emitLog(topic, data);
    }

    /// @inheritdoc IOracle
    function emitLogs(bytes32 topic, bytes[] calldata dataVector) external onlySystemAddress {
        // Gas optimized loop: avoid redundant SLOADs and function calls.
        uint256 len = dataVector.length;
        for (uint256 i = 0; i < len;) {
            _emitLog(topic, dataVector[i]);
            unchecked {
                ++i;
            }
        }
    }

    /// @notice Emits a Log event with the given topic and data.
    function _emitLog(bytes32 topic, bytes calldata data) internal {
        emit Log(topic, data);
    }

    /// @inheritdoc IOracle
    function getSlot(uint256 slot) external view returns (bytes32 value) {
        assembly {
            value := sload(slot)
        }
    }

    /// @inheritdoc IOracle
    function setSlot(uint256 slot, bytes32 value) external onlySystemAddress {
        assembly {
            sstore(slot, value)
        }
    }

    /// @inheritdoc IOracle
    function getSlots(uint256[] calldata slots) external view returns (bytes32[] memory values) {
        values = new bytes32[](slots.length);
        assembly {
            let valuesPtr := add(values, 0x20)
            let slotsPtr := slots.offset
            let length := slots.length

            for { let i := 0 } lt(i, length) { i := add(i, 1) } {
                let slot := calldataload(add(slotsPtr, mul(i, 0x20)))
                mstore(add(valuesPtr, mul(i, 0x20)), sload(slot))
            }
        }
    }

    /// @inheritdoc IOracle
    function setSlots(uint256[] calldata slots, bytes32[] calldata values) external onlySystemAddress {
        if (slots.length != values.length) revert InvalidLength(slots.length, values.length);
        assembly {
            let slotsPtr := slots.offset
            let valuesPtr := values.offset
            let length := slots.length

            for { let i := 0 } lt(i, length) { i := add(i, 1) } {
                let slot := calldataload(add(slotsPtr, mul(i, 0x20)))
                let value := calldataload(add(valuesPtr, mul(i, 0x20)))
                sstore(slot, value)
            }
        }
    }
}
