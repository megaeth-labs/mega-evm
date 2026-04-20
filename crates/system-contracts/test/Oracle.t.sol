// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {Oracle} from "../contracts/Oracle.sol";
import {SequencerRegistry} from "../contracts/SequencerRegistry.sol";
import {ISequencerRegistry} from "../contracts/interfaces/ISequencerRegistry.sol";

contract OracleTest is Test {
    Oracle public oracle;
    SequencerRegistry public registry;

    address public constant INITIAL_SEQUENCER = 0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d;
    address public constant REGISTRY_ADDRESS = 0x6342000000000000000000000000000000000006;
    address public user = address(0x2);
    address public newSequencer = address(0xCAFE);

    function setUp() public {
        // Deploy SequencerRegistry and place its bytecode at the hardcoded address
        // that Oracle's SEQUENCER_REGISTRY constant expects.
        SequencerRegistry impl = new SequencerRegistry();
        vm.etch(REGISTRY_ADDRESS, address(impl).code);
        registry = SequencerRegistry(REGISTRY_ADDRESS);

        // Deploy Oracle (v2.0.0 — no constructor params)
        oracle = new Oracle();
    }

    // ============ Version & Registry ============

    function testVersion() public view {
        assertEq(oracle.version(), "2.0.0");
    }

    function testSequencerRegistry() public view {
        assertEq(address(oracle.SEQUENCER_REGISTRY()), REGISTRY_ADDRESS);
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    // ============ setSlot / getSlot ============

    function testSetSlot() public {
        uint256 slot = 0;
        bytes32 value = bytes32(uint256(12345));

        vm.prank(INITIAL_SEQUENCER);
        oracle.setSlot(slot, value);
        assertEq(oracle.getSlot(slot), value);
    }

    function testSetSlotFailsFromNonSystem() public {
        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlot(0, bytes32(uint256(12345)));
    }

    function testGetSlotDefaultValue() public view {
        assertEq(oracle.getSlot(0), bytes32(0));
    }

    function testSetMultipleSlots() public {
        vm.startPrank(INITIAL_SEQUENCER);
        oracle.setSlot(0, bytes32(uint256(100)));
        oracle.setSlot(1, bytes32(uint256(200)));
        oracle.setSlot(255, bytes32(uint256(300)));
        vm.stopPrank();

        assertEq(oracle.getSlot(0), bytes32(uint256(100)));
        assertEq(oracle.getSlot(1), bytes32(uint256(200)));
        assertEq(oracle.getSlot(255), bytes32(uint256(300)));
    }

    function testOverwriteSlot() public {
        vm.startPrank(INITIAL_SEQUENCER);
        oracle.setSlot(0, bytes32(uint256(100)));
        oracle.setSlot(0, bytes32(uint256(200)));
        vm.stopPrank();

        assertEq(oracle.getSlot(0), bytes32(uint256(200)));
    }

    function testFuzzSetSlot(uint256 slot, bytes32 value) public {
        vm.prank(INITIAL_SEQUENCER);
        oracle.setSlot(slot, value);
        assertEq(oracle.getSlot(slot), value);
    }

    // ============ setSlots / getSlots ============

    function testBatchGetSlots() public {
        uint256[] memory slots = new uint256[](3);
        slots[0] = 0;
        slots[1] = 1;
        slots[2] = 2;

        bytes32[] memory values = new bytes32[](3);
        values[0] = bytes32(uint256(111));
        values[1] = bytes32(uint256(222));
        values[2] = bytes32(uint256(333));

        vm.prank(INITIAL_SEQUENCER);
        oracle.setSlots(slots, values);

        bytes32[] memory retrieved = oracle.getSlots(slots);
        assertEq(retrieved[0], bytes32(uint256(111)));
        assertEq(retrieved[1], bytes32(uint256(222)));
        assertEq(retrieved[2], bytes32(uint256(333)));
    }

    function testBatchSetAndGet() public {
        uint256[] memory slots = new uint256[](10);
        bytes32[] memory values = new bytes32[](10);
        for (uint256 i = 0; i < 10; i++) {
            slots[i] = i;
            values[i] = bytes32(i * 100);
        }

        vm.prank(INITIAL_SEQUENCER);
        oracle.setSlots(slots, values);

        bytes32[] memory retrieved = oracle.getSlots(slots);
        for (uint256 i = 0; i < 10; i++) {
            assertEq(retrieved[i], bytes32(i * 100));
        }
    }

    function testSetSlotsFailsWithMismatchedLengths() public {
        uint256[] memory slots = new uint256[](2);
        bytes32[] memory values = new bytes32[](3);

        vm.prank(INITIAL_SEQUENCER);
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

        vm.prank(INITIAL_SEQUENCER);
        oracle.setSlots(slots, values);
    }

    function testGetSlotsEmptyArray() public view {
        uint256[] memory slots = new uint256[](0);
        bytes32[] memory values = oracle.getSlots(slots);
        assertEq(values.length, 0);
    }

    function testArbitrarySlots() public {
        uint256 slot1 = uint256(keccak256("slot.one"));
        uint256 slot2 = uint256(keccak256("slot.two"));
        uint256 slot3 = type(uint256).max;

        vm.startPrank(INITIAL_SEQUENCER);
        oracle.setSlot(slot1, bytes32(uint256(0xdeadbeef)));
        oracle.setSlot(slot2, bytes32(uint256(0xcafebabe)));
        oracle.setSlot(slot3, bytes32(uint256(0x12345678)));
        vm.stopPrank();

        assertEq(oracle.getSlot(slot1), bytes32(uint256(0xdeadbeef)));
        assertEq(oracle.getSlot(slot2), bytes32(uint256(0xcafebabe)));
        assertEq(oracle.getSlot(slot3), bytes32(uint256(0x12345678)));
    }

    function testStorageCollision() public {
        vm.startPrank(INITIAL_SEQUENCER);
        oracle.setSlot(100, bytes32(uint256(0xaaaa)));
        oracle.setSlot(101, bytes32(uint256(0xbbbb)));
        vm.stopPrank();

        assertEq(oracle.getSlot(100), bytes32(uint256(0xaaaa)));
        assertEq(oracle.getSlot(101), bytes32(uint256(0xbbbb)));

        vm.prank(INITIAL_SEQUENCER);
        oracle.setSlot(100, bytes32(uint256(0xcccc)));

        assertEq(oracle.getSlot(100), bytes32(uint256(0xcccc)));
        assertEq(oracle.getSlot(101), bytes32(uint256(0xbbbb)));
    }

    // ============ sendHint ============

    function testSendHintIsView() public view {
        oracle.sendHint(bytes32(uint256(0x1234)), hex"deadbeef");
    }

    function testSendHintFromAnyAddress() public {
        vm.prank(user);
        oracle.sendHint(bytes32(uint256(0x5678)), hex"cafebabe");
    }

    // ============ emitLog / emitLogs ============

    function testEmitLog() public {
        bytes32 topic = bytes32(uint256(0xabcd));
        bytes memory data = hex"deadbeef";

        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, data);

        vm.prank(INITIAL_SEQUENCER);
        oracle.emitLog(topic, data);
    }

    function testEmitLogFailsFromNonSystem() public {
        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.emitLog(bytes32(uint256(0x1234)), hex"cafebabe");
    }

    function testEmitLogs() public {
        bytes32 topic = bytes32(uint256(0xabcd));
        bytes[] memory dataVector = new bytes[](2);
        dataVector[0] = hex"deadbeef";
        dataVector[1] = hex"cafebabe";

        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, dataVector[0]);
        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, dataVector[1]);

        vm.prank(INITIAL_SEQUENCER);
        oracle.emitLogs(topic, dataVector);
    }

    function testEmitLogsFailsFromNonSystem() public {
        bytes[] memory dataVector = new bytes[](1);
        dataVector[0] = hex"deadbeef";

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.emitLogs(bytes32(uint256(0x1234)), dataVector);
    }

    // ============ multiCall ============

    function testMultiCallSingleCall() public {
        bytes[] memory data = new bytes[](1);
        data[0] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(100), bytes32(uint256(0x1234)));

        vm.prank(INITIAL_SEQUENCER);
        oracle.multiCall(data);

        assertEq(oracle.getSlot(100), bytes32(uint256(0x1234)));
    }

    function testMultiCallAccessControlEnforced() public {
        bytes[] memory data = new bytes[](1);
        data[0] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(300), bytes32(uint256(0x1234)));

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.multiCall(data);
    }

    // ============ Rotation: authority follows SequencerRegistry ============

    function testAfterRotation_newSequencerCanCallSetSlot() public {
        uint256 activationBlock = block.number + 100;

        vm.prank(INITIAL_SEQUENCER);
        registry.scheduleNextSequencerChange(newSequencer, activationBlock);

        vm.roll(activationBlock);
        registry.applyPendingChange();

        assertEq(registry.currentSequencer(), newSequencer);

        vm.prank(newSequencer);
        oracle.setSlot(42, bytes32(uint256(0xBEEF)));
        assertEq(oracle.getSlot(42), bytes32(uint256(0xBEEF)));
    }

    function testAfterRotation_oldSequencerCannotCallSetSlot() public {
        uint256 activationBlock = block.number + 100;

        vm.prank(INITIAL_SEQUENCER);
        registry.scheduleNextSequencerChange(newSequencer, activationBlock);

        vm.roll(activationBlock);
        registry.applyPendingChange();

        vm.prank(INITIAL_SEQUENCER);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlot(0, bytes32(uint256(1)));
    }

    function testAfterRotation_newSequencerCanEmitLog() public {
        uint256 activationBlock = block.number + 50;

        vm.prank(INITIAL_SEQUENCER);
        registry.scheduleNextSequencerChange(newSequencer, activationBlock);

        vm.roll(activationBlock);
        registry.applyPendingChange();

        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(bytes32(uint256(0x7777)), hex"cafe");

        vm.prank(newSequencer);
        oracle.emitLog(bytes32(uint256(0x7777)), hex"cafe");
    }
}
