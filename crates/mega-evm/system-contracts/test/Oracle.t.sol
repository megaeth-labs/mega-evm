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
        assertEq(oracle.version(), "1.1.0");
    }

    function testSystemAddress() public view {
        assertEq(oracle.MEGA_SYSTEM_ADDRESS(), systemAddress);
    }

    function testSetSlot() public {
        uint256 slot = 0;
        bytes32 value = bytes32(uint256(12345));

        vm.prank(systemAddress);
        oracle.setSlot(slot, value);
        assertEq(oracle.getSlot(slot), value);
    }

    function testSetSlotFailsFromNonSystem() public {
        uint256 slot = 0;
        bytes32 value = bytes32(uint256(12345));

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlot(slot, value);
    }

    function testGetSlotDefaultValue() public view {
        uint256 slot = 0;
        assertEq(oracle.getSlot(slot), bytes32(0));
    }

    function testSetMultipleSlots() public {
        vm.startPrank(systemAddress);
        oracle.setSlot(0, bytes32(uint256(100)));
        oracle.setSlot(1, bytes32(uint256(200)));
        oracle.setSlot(255, bytes32(uint256(300)));
        vm.stopPrank();

        assertEq(oracle.getSlot(0), bytes32(uint256(100)));
        assertEq(oracle.getSlot(1), bytes32(uint256(200)));
        assertEq(oracle.getSlot(255), bytes32(uint256(300)));
    }

    function testOverwriteSlot() public {
        uint256 slot = 0;

        vm.startPrank(systemAddress);
        oracle.setSlot(slot, bytes32(uint256(100)));
        oracle.setSlot(slot, bytes32(uint256(200)));
        vm.stopPrank();

        assertEq(oracle.getSlot(slot), bytes32(uint256(200)));
    }

    function testFuzzSetSlot(uint256 slot, bytes32 value) public {
        vm.prank(systemAddress);
        oracle.setSlot(slot, value);
        assertEq(oracle.getSlot(slot), value);
    }

    function testBatchGetSlots() public {
        // Prepare data
        uint256[] memory slots = new uint256[](3);
        slots[0] = 0;
        slots[1] = 1;
        slots[2] = 2;

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
        uint256[] memory slots = new uint256[](10);
        bytes32[] memory values = new bytes32[](10);
        for (uint256 i = 0; i < 10; i++) {
            slots[i] = i;
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
        uint256[] memory slots = new uint256[](2);
        bytes32[] memory values = new bytes32[](3);

        vm.prank(systemAddress);
        vm.expectRevert(abi.encodeWithSelector(Oracle.InvalidLength.selector, 2, 3));
        oracle.setSlots(slots, values);
    }

    function testSetSlotsFailsFromNonSystem() public {
        uint256[] memory slots = new uint256[](1);
        bytes32[] memory values = new bytes32[](1);

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlots(slots, values);
    }

    function testGetSlotsDefaultValues() public view {
        uint256[] memory slots = new uint256[](3);
        slots[0] = 10;
        slots[1] = 20;
        slots[2] = 30;

        bytes32[] memory values = oracle.getSlots(slots);
        assertEq(values[0], bytes32(0));
        assertEq(values[1], bytes32(0));
        assertEq(values[2], bytes32(0));
    }

    function testSetSlotsEmptyArrays() public {
        uint256[] memory slots = new uint256[](0);
        bytes32[] memory values = new bytes32[](0);

        vm.prank(systemAddress);
        oracle.setSlots(slots, values);
        // Should not revert
    }

    function testGetSlotsEmptyArray() public view {
        uint256[] memory slots = new uint256[](0);
        bytes32[] memory values = oracle.getSlots(slots);
        assertEq(values.length, 0);
    }

    function testArbitrarySlots() public {
        // Test with non-sequential, arbitrary slot addresses
        uint256 slot1 = uint256(keccak256("slot.one"));
        uint256 slot2 = uint256(keccak256("slot.two"));
        uint256 slot3 = type(uint256).max;

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
        uint256[] memory slots = new uint256[](3);
        slots[0] = uint256(keccak256("custom.slot.alpha"));
        slots[1] = uint256(keccak256("custom.slot.beta"));
        slots[2] = type(uint256).max - 1;

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
        uint256 slot1 = 100;
        uint256 slot2 = 101;
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

    function testSendHintIsView() public view {
        // sendHint is a view function that can be called by any address.
        // The actual hint mechanism is handled at the EVM level (in execution.rs),
        // which intercepts CALL/STATICCALL to sendHint.
        bytes32 topic = bytes32(uint256(0x1234));
        bytes memory data = hex"deadbeef";

        // This should not revert - sendHint is callable as a view function
        oracle.sendHint(topic, data);
    }

    function testSendHintFromAnyAddress() public {
        // sendHint can be called by any address, not just the system address
        bytes32 topic = bytes32(uint256(0x5678));
        bytes memory data = hex"cafebabe";

        vm.prank(user);
        // This should not revert
        oracle.sendHint(topic, data);
    }

    function testEmitLog() public {
        bytes32 topic = bytes32(uint256(0xabcd));
        bytes memory data = hex"deadbeef";

        // Expect the Log event to be emitted
        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, data);

        vm.prank(systemAddress);
        oracle.emitLog(topic, data);
    }

    function testEmitLogFailsFromNonSystem() public {
        // emitLog can only be called by the system address
        bytes32 topic = bytes32(uint256(0x1234));
        bytes memory data = hex"cafebabe";

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.emitLog(topic, data);
    }

    function testEmitLogEmptyData() public {
        bytes32 topic = bytes32(uint256(0x9999));
        bytes memory data = "";

        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, data);

        vm.prank(systemAddress);
        oracle.emitLog(topic, data);
    }

    function testEmitLogLargeData() public {
        bytes32 topic = bytes32(uint256(0x8888));
        // Create a 256-byte data payload
        bytes memory data = new bytes(256);
        for (uint256 i = 0; i < 256; i++) {
            data[i] = bytes1(uint8(i));
        }

        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, data);

        vm.prank(systemAddress);
        oracle.emitLog(topic, data);
    }

    // ============ emitLogs Tests ============

    function testEmitLogs() public {
        bytes32 topic = bytes32(uint256(0xabcd));
        bytes[] memory dataVector = new bytes[](2);
        dataVector[0] = hex"deadbeef";
        dataVector[1] = hex"cafebabe";

        // Expect two Log events with the same topic
        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, dataVector[0]);
        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, dataVector[1]);

        vm.prank(systemAddress);
        oracle.emitLogs(topic, dataVector);
    }

    function testEmitLogsFailsFromNonSystem() public {
        bytes32 topic = bytes32(uint256(0x1234));
        bytes[] memory dataVector = new bytes[](1);
        dataVector[0] = hex"deadbeef";

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.emitLogs(topic, dataVector);
    }

    function testEmitLogsEmptyArray() public {
        bytes32 topic = bytes32(uint256(0x5678));
        bytes[] memory dataVector = new bytes[](0);

        // Should not revert with empty array
        vm.prank(systemAddress);
        oracle.emitLogs(topic, dataVector);
    }

    function testEmitLogsMultipleData() public {
        bytes32 topic = bytes32(uint256(0x9999));
        bytes[] memory dataVector = new bytes[](5);
        for (uint256 i = 0; i < 5; i++) {
            dataVector[i] = abi.encodePacked(uint256(i * 100));
        }

        // Expect all 5 Log events
        for (uint256 i = 0; i < 5; i++) {
            vm.expectEmit(true, false, false, true);
            emit Oracle.Log(topic, dataVector[i]);
        }

        vm.prank(systemAddress);
        oracle.emitLogs(topic, dataVector);
    }

    // ============ multiCall Tests ============

    function testMultiCallEmptyArray() public {
        bytes[] memory data = new bytes[](0);
        bytes[] memory results = oracle.multiCall(data);
        assertEq(results.length, 0);
    }

    function testMultiCallSingleCall() public {
        // Prepare a setSlot call
        bytes[] memory data = new bytes[](1);
        data[0] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(100), bytes32(uint256(0x1234)));

        vm.prank(systemAddress);
        oracle.multiCall(data);

        // Verify the slot was set
        assertEq(oracle.getSlot(100), bytes32(uint256(0x1234)));
    }

    function testMultiCallMultipleCalls() public {
        // Prepare multiple setSlot calls
        bytes[] memory data = new bytes[](3);
        data[0] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(200), bytes32(uint256(0xaaaa)));
        data[1] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(201), bytes32(uint256(0xbbbb)));
        data[2] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(202), bytes32(uint256(0xcccc)));

        vm.prank(systemAddress);
        oracle.multiCall(data);

        // Verify all slots were set
        assertEq(oracle.getSlot(200), bytes32(uint256(0xaaaa)));
        assertEq(oracle.getSlot(201), bytes32(uint256(0xbbbb)));
        assertEq(oracle.getSlot(202), bytes32(uint256(0xcccc)));
    }

    function testMultiCallAccessControlEnforced() public {
        // Non-system caller should fail when calling restricted functions via multiCall
        bytes[] memory data = new bytes[](1);
        data[0] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(300), bytes32(uint256(0x1234)));

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.multiCall(data);
    }

    function testMultiCallRevertBubbling() public {
        // When a call fails due to access control, the revert should bubble up
        bytes[] memory data = new bytes[](2);
        // First call should succeed (getSlot is not restricted)
        data[0] = abi.encodeWithSelector(Oracle.getSlot.selector, uint256(0));
        // Second call should fail (setSlot requires system address)
        data[1] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(400), bytes32(uint256(0x5678)));

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.multiCall(data);
    }

    function testMultiCallMixedOperations() public {
        // Set a slot first
        vm.prank(systemAddress);
        oracle.setSlot(500, bytes32(uint256(0xdead)));

        // Now use multiCall to read and write
        bytes[] memory data = new bytes[](3);
        data[0] = abi.encodeWithSelector(Oracle.getSlot.selector, uint256(500));
        data[1] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(501), bytes32(uint256(0xbeef)));
        data[2] = abi.encodeWithSelector(Oracle.getSlot.selector, uint256(501));

        vm.prank(systemAddress);
        bytes[] memory results = oracle.multiCall(data);

        // Verify results
        assertEq(results.length, 3);
        assertEq(abi.decode(results[0], (bytes32)), bytes32(uint256(0xdead)));
        // setSlot returns nothing
        assertEq(results[1].length, 0);
        // getSlot should return the newly set value
        assertEq(abi.decode(results[2], (bytes32)), bytes32(uint256(0xbeef)));
    }

    function testMultiCallWithEmitLog() public {
        bytes32 topic = bytes32(uint256(0x7777));
        bytes memory logData = hex"deadbeef";

        bytes[] memory data = new bytes[](2);
        data[0] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(600), bytes32(uint256(0x1111)));
        data[1] = abi.encodeWithSelector(Oracle.emitLog.selector, topic, logData);

        // Expect the Log event
        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, logData);

        vm.prank(systemAddress);
        oracle.multiCall(data);

        // Verify slot was set
        assertEq(oracle.getSlot(600), bytes32(uint256(0x1111)));
    }

    function testMultiCallWithEmitLogs() public {
        bytes32 topic = bytes32(uint256(0x8888));
        bytes[] memory logDataVector = new bytes[](2);
        logDataVector[0] = hex"aaaa";
        logDataVector[1] = hex"bbbb";

        bytes[] memory data = new bytes[](1);
        data[0] = abi.encodeWithSelector(Oracle.emitLogs.selector, topic, logDataVector);

        // Expect both Log events
        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, logDataVector[0]);
        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, logDataVector[1]);

        vm.prank(systemAddress);
        oracle.multiCall(data);
    }
}
