// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {Oracle} from "../src/Oracle.sol";

contract OracleTest is Test {
    Oracle public oracle;
    address public systemAddress;
    address public user;

    function setUp() public {
        systemAddress = 0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d; // Oracle's MEGA_SYSTEM_ADDRESS
        user = address(0x2);
        oracle = new Oracle(systemAddress);
    }

    function testVersion() public view {
        assertEq(oracle.version(), "2.0.0");
    }

    function testSystemAddress() public view {
        assertEq(oracle.MEGA_SYSTEM_ADDRESS(), systemAddress);
    }

    function testSetSlot() public {
        bytes32 slot = bytes32(uint256(0));
        bytes32 value = bytes32(uint256(12345));

        vm.prank(systemAddress);
        oracle.setSlot(slot, value);
        assertEq(oracle.getSlot(slot), value);
    }

    function testSetSlotFailsFromNonSystem() public {
        bytes32 slot = bytes32(uint256(0));
        bytes32 value = bytes32(uint256(12345));

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlot(slot, value);
    }

    function testGetSlotDefaultValue() public view {
        bytes32 slot = bytes32(uint256(0));
        assertEq(oracle.getSlot(slot), bytes32(0));
    }

    function testSetMultipleSlots() public {
        vm.startPrank(systemAddress);
        oracle.setSlot(bytes32(uint256(0)), bytes32(uint256(100)));
        oracle.setSlot(bytes32(uint256(1)), bytes32(uint256(200)));
        oracle.setSlot(bytes32(uint256(255)), bytes32(uint256(300)));
        vm.stopPrank();

        assertEq(oracle.getSlot(bytes32(uint256(0))), bytes32(uint256(100)));
        assertEq(oracle.getSlot(bytes32(uint256(1))), bytes32(uint256(200)));
        assertEq(oracle.getSlot(bytes32(uint256(255))), bytes32(uint256(300)));
    }

    function testOverwriteSlot() public {
        bytes32 slot = bytes32(uint256(0));

        vm.startPrank(systemAddress);
        oracle.setSlot(slot, bytes32(uint256(100)));
        oracle.setSlot(slot, bytes32(uint256(200)));
        vm.stopPrank();

        assertEq(oracle.getSlot(slot), bytes32(uint256(200)));
    }

    function testFuzzSetSlot(bytes32 slot, bytes32 value) public {
        vm.prank(systemAddress);
        oracle.setSlot(slot, value);
        assertEq(oracle.getSlot(slot), value);
    }

    function testBatchGetSlots() public {
        // Prepare data
        bytes32[] memory slots = new bytes32[](3);
        slots[0] = bytes32(uint256(0));
        slots[1] = bytes32(uint256(1));
        slots[2] = bytes32(uint256(2));

        bytes32[] memory values = new bytes32[](3);
        values[0] = bytes32(uint256(111));
        values[1] = bytes32(uint256(222));
        values[2] = bytes32(uint256(333));

        vm.startPrank(systemAddress);
        oracle.setSlots(slots, values);
        vm.stopPrank();

        // Verify all values using batch getter
        bytes32[] memory retrievedValues = oracle.getSlots(slots);
        assertEq(retrievedValues[0], bytes32(uint256(111)));
        assertEq(retrievedValues[1], bytes32(uint256(222)));
        assertEq(retrievedValues[2], bytes32(uint256(333)));
    }

    function testBatchSetAndGet() public {
        // Prepare batch data
        bytes32[] memory slots = new bytes32[](10);
        bytes32[] memory values = new bytes32[](10);
        for (uint256 i = 0; i < 10; i++) {
            slots[i] = bytes32(i);
            values[i] = bytes32(i * 100);
        }

        vm.prank(systemAddress);
        oracle.setSlots(slots, values);

        // Batch get operations
        bytes32[] memory retrievedValues = oracle.getSlots(slots);

        // Verify all values
        for (uint256 i = 0; i < 10; i++) {
            assertEq(retrievedValues[i], bytes32(i * 100));
        }
    }

    function testSetSlotsFailsWithMismatchedLengths() public {
        bytes32[] memory slots = new bytes32[](2);
        bytes32[] memory values = new bytes32[](3);

        vm.prank(systemAddress);
        vm.expectRevert(abi.encodeWithSelector(Oracle.InvalidLength.selector, 2, 3));
        oracle.setSlots(slots, values);
    }

    function testSetSlotsFailsFromNonSystem() public {
        bytes32[] memory slots = new bytes32[](1);
        bytes32[] memory values = new bytes32[](1);

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlots(slots, values);
    }

    function testGetSlotsDefaultValues() public view {
        bytes32[] memory slots = new bytes32[](3);
        slots[0] = bytes32(uint256(10));
        slots[1] = bytes32(uint256(20));
        slots[2] = bytes32(uint256(30));

        bytes32[] memory values = oracle.getSlots(slots);
        assertEq(values[0], bytes32(0));
        assertEq(values[1], bytes32(0));
        assertEq(values[2], bytes32(0));
    }

    function testSetSlotsEmptyArrays() public {
        bytes32[] memory slots = new bytes32[](0);
        bytes32[] memory values = new bytes32[](0);

        vm.prank(systemAddress);
        oracle.setSlots(slots, values);
        // Should not revert
    }

    function testGetSlotsEmptyArray() public view {
        bytes32[] memory slots = new bytes32[](0);
        bytes32[] memory values = oracle.getSlots(slots);
        assertEq(values.length, 0);
    }

    function testArbitrarySlots() public {
        // Test with non-sequential, arbitrary slot addresses
        bytes32 slot1 = keccak256("slot.one");
        bytes32 slot2 = keccak256("slot.two");
        bytes32 slot3 = bytes32(uint256(type(uint256).max));

        bytes32 value1 = bytes32(uint256(0xdeadbeef));
        bytes32 value2 = bytes32(uint256(0xcafebabe));
        bytes32 value3 = bytes32(uint256(0x12345678));

        vm.startPrank(systemAddress);
        oracle.setSlot(slot1, value1);
        oracle.setSlot(slot2, value2);
        oracle.setSlot(slot3, value3);
        vm.stopPrank();

        assertEq(oracle.getSlot(slot1), value1);
        assertEq(oracle.getSlot(slot2), value2);
        assertEq(oracle.getSlot(slot3), value3);
    }

    function testBatchWithArbitrarySlots() public {
        bytes32[] memory slots = new bytes32[](3);
        slots[0] = keccak256("custom.slot.alpha");
        slots[1] = keccak256("custom.slot.beta");
        slots[2] = bytes32(uint256(type(uint256).max - 1));

        bytes32[] memory values = new bytes32[](3);
        values[0] = bytes32(uint256(0x1111));
        values[1] = bytes32(uint256(0x2222));
        values[2] = bytes32(uint256(0x3333));

        vm.prank(systemAddress);
        oracle.setSlots(slots, values);

        bytes32[] memory retrievedValues = oracle.getSlots(slots);
        assertEq(retrievedValues[0], values[0]);
        assertEq(retrievedValues[1], values[1]);
        assertEq(retrievedValues[2], values[2]);
    }

    function testStorageCollision() public {
        // Test that writing to different slots doesn't interfere
        bytes32 slot1 = bytes32(uint256(100));
        bytes32 slot2 = bytes32(uint256(101));
        bytes32 value1 = bytes32(uint256(0xaaaa));
        bytes32 value2 = bytes32(uint256(0xbbbb));

        vm.startPrank(systemAddress);
        oracle.setSlot(slot1, value1);
        oracle.setSlot(slot2, value2);
        vm.stopPrank();

        // Verify both slots have correct values
        assertEq(oracle.getSlot(slot1), value1);
        assertEq(oracle.getSlot(slot2), value2);

        // Overwrite slot1, verify slot2 unchanged
        vm.prank(systemAddress);
        oracle.setSlot(slot1, bytes32(uint256(0xcccc)));

        assertEq(oracle.getSlot(slot1), bytes32(uint256(0xcccc)));
        assertEq(oracle.getSlot(slot2), value2);
    }
}
