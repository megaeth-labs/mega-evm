// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ISemver} from "./lib/ISemver.sol";

/// @title Oracle
/// @author MegaETH
/// @notice Oracle provides a simple interface to directly read and set storage slots.
contract Oracle is ISemver {
    /// @notice The address authorized to modify oracle data.
    /// @dev Only this address can call setter functions.
    address public immutable MEGA_SYSTEM_ADDRESS;

    /// @notice Thrown when a non-system address attempts to call a restricted function.
    error NotSystemAddress();

    /// @notice Thrown when array lengths don't match in batch operations.
    /// @param slotsLength The length of the slots array.
    /// @param valuesLength The length of the values array.
    error InvalidLength(uint256 slotsLength, uint256 valuesLength);

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
        return "1.0.0";
    }

    /// @notice Initializes the Oracle contract with the system address.
    /// @param _megaSystemAddress The address authorized to modify oracle data.
    constructor(address _megaSystemAddress) {
        MEGA_SYSTEM_ADDRESS = _megaSystemAddress;
    }

    /// @notice Reads a value from a specific storage slot.
    /// @param slot The storage slot to read from.
    /// @return value The bytes32 value stored at the slot.
    function getSlot(bytes32 slot) external view returns (bytes32 value) {
        assembly {
            value := sload(slot)
        }
    }

    /// @notice Writes a value to a specific storage slot.
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS.
    /// @param slot The storage slot to write to.
    /// @param value The bytes32 value to store.
    function setSlot(bytes32 slot, bytes32 value) external onlySystemAddress {
        assembly {
            sstore(slot, value)
        }
    }

    /// @notice Reads values from multiple storage slots in a single call.
    /// @param slots Array of storage slots to read from.
    /// @return values Array of bytes32 values stored at corresponding slots.
    function getSlots(bytes32[] calldata slots) external view returns (bytes32[] memory values) {
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

    /// @notice Writes values to multiple storage slots in a single transaction.
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS. Arrays must have equal length.
    /// @param slots Array of storage slots to write to.
    /// @param values Array of bytes32 values to store at corresponding slots.
    function setSlots(bytes32[] calldata slots, bytes32[] calldata values) external onlySystemAddress {
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
