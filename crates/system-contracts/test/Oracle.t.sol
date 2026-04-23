// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {Oracle} from "../contracts/Oracle.sol";
import {SequencerRegistry} from "../contracts/SequencerRegistry.sol";
import {ISequencerRegistry} from "../contracts/interfaces/ISequencerRegistry.sol";

contract OracleTest is Test {
    Oracle public oracle;
    SequencerRegistry public registry;

    address public constant REGISTRY_ADDRESS = 0x6342000000000000000000000000000000000006;

    address public constant INITIAL_SYSTEM_ADDRESS = address(0xAA);
    address public constant INITIAL_SEQUENCER = address(0xBB);
    address public constant INITIAL_ADMIN = address(0xCC);
    uint256 public constant INITIAL_FROM_BLOCK = 1;

    address public user = address(0x2);
    address public newSystemAddress = address(0xCAFE);
    address public newSequencer = address(0xFACE);

    function setUp() public {
        // Deploy SequencerRegistry bytecode at the canonical address via vm.etch.
        SequencerRegistry impl = new SequencerRegistry();
        vm.etch(REGISTRY_ADDRESS, address(impl).code);
        registry = SequencerRegistry(REGISTRY_ADDRESS);

        // Seed SequencerRegistry storage (no constructor).
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(0)), bytes32(uint256(uint160(INITIAL_SYSTEM_ADDRESS))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(1)), bytes32(uint256(uint160(INITIAL_SEQUENCER))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(2)), bytes32(uint256(uint160(INITIAL_ADMIN))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(3)), bytes32(uint256(uint160(INITIAL_SYSTEM_ADDRESS))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(4)), bytes32(uint256(uint160(INITIAL_SEQUENCER))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(5)), bytes32(uint256(INITIAL_FROM_BLOCK)));

        vm.roll(INITIAL_FROM_BLOCK);

        // Deploy Oracle (v2.0.0 -- reads authority from SequencerRegistry.currentSystemAddress()).
        oracle = new Oracle();
    }

    // ============ version ============

    function test_version() public view {
        assertEq(oracle.version(), "2.0.0");
    }

    // ============ Registry link ============

    function test_sequencerRegistryAddress() public view {
        assertEq(address(oracle.SEQUENCER_REGISTRY()), REGISTRY_ADDRESS);
    }

    // ============ setSlot / getSlot: authority = currentSystemAddress ============

    function test_setSlot_initialSystemAddressCanCall() public {
        vm.prank(INITIAL_SYSTEM_ADDRESS);
        oracle.setSlot(0, bytes32(uint256(12345)));
        assertEq(oracle.getSlot(0), bytes32(uint256(12345)));
    }

    function test_setSlot_revertsFromRandomAddress() public {
        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlot(0, bytes32(uint256(12345)));
    }

    function test_setSlot_revertsFromSequencer() public {
        // Sequencer is NOT the system address -- should be rejected.
        vm.prank(INITIAL_SEQUENCER);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlot(0, bytes32(uint256(1)));
    }

    function test_getSlot_defaultValueIsZero() public view {
        assertEq(oracle.getSlot(0), bytes32(0));
    }

    // ============ Authority follows currentSystemAddress ============

    function test_afterSystemAddressChange_newSystemAddressCanCall() public {
        uint256 activationBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, activationBlock);

        vm.roll(activationBlock);
        registry.applyPendingChanges();

        assertEq(registry.currentSystemAddress(), newSystemAddress);

        vm.prank(newSystemAddress);
        oracle.setSlot(42, bytes32(uint256(0xBEEF)));
        assertEq(oracle.getSlot(42), bytes32(uint256(0xBEEF)));
    }

    function test_afterSystemAddressChange_oldSystemAddressCannotCall() public {
        uint256 activationBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, activationBlock);

        vm.roll(activationBlock);
        registry.applyPendingChanges();

        vm.prank(INITIAL_SYSTEM_ADDRESS);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlot(0, bytes32(uint256(1)));
    }

    // ============ Critical independence: sequencer change does NOT affect Oracle ============

    function test_sequencerChange_doesNotAffectOracleAuthority() public {
        uint256 activationBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, activationBlock);

        vm.roll(activationBlock);
        registry.applyPendingChanges();

        // Sequencer changed, but Oracle authority is still the original system address.
        assertEq(registry.currentSequencer(), newSequencer);
        assertEq(registry.currentSystemAddress(), INITIAL_SYSTEM_ADDRESS);

        // Original system address can still call Oracle.
        vm.prank(INITIAL_SYSTEM_ADDRESS);
        oracle.setSlot(99, bytes32(uint256(0xFEED)));
        assertEq(oracle.getSlot(99), bytes32(uint256(0xFEED)));

        // New sequencer cannot call Oracle.
        vm.prank(newSequencer);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlot(0, bytes32(uint256(1)));
    }

    // ============ sendHint ============

    function test_sendHint_isViewAndCallableByAnyone() public view {
        oracle.sendHint(bytes32(uint256(0x1234)), hex"deadbeef");
    }

    function test_sendHint_fromArbitraryAddress() public {
        vm.prank(user);
        oracle.sendHint(bytes32(uint256(0x5678)), hex"cafebabe");
    }

    // ============ setSlots / getSlots ============

    function test_setSlots_basic() public {
        uint256[] memory slots = new uint256[](3);
        slots[0] = 0;
        slots[1] = 1;
        slots[2] = 2;

        bytes32[] memory values = new bytes32[](3);
        values[0] = bytes32(uint256(111));
        values[1] = bytes32(uint256(222));
        values[2] = bytes32(uint256(333));

        vm.prank(INITIAL_SYSTEM_ADDRESS);
        oracle.setSlots(slots, values);

        bytes32[] memory retrieved = oracle.getSlots(slots);
        assertEq(retrieved[0], bytes32(uint256(111)));
        assertEq(retrieved[1], bytes32(uint256(222)));
        assertEq(retrieved[2], bytes32(uint256(333)));
    }

    function test_setSlots_revertsFromNonSystemAddress() public {
        uint256[] memory slots = new uint256[](1);
        bytes32[] memory values = new bytes32[](1);

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.setSlots(slots, values);
    }

    function test_setSlots_revertsMismatchedLengths() public {
        uint256[] memory slots = new uint256[](2);
        bytes32[] memory values = new bytes32[](3);

        vm.prank(INITIAL_SYSTEM_ADDRESS);
        vm.expectRevert(abi.encodeWithSelector(Oracle.InvalidLength.selector, 2, 3));
        oracle.setSlots(slots, values);
    }

    function test_getSlots_defaultValues() public view {
        uint256[] memory slots = new uint256[](3);
        slots[0] = 10;
        slots[1] = 20;
        slots[2] = 30;

        bytes32[] memory values = oracle.getSlots(slots);
        assertEq(values[0], bytes32(0));
        assertEq(values[1], bytes32(0));
        assertEq(values[2], bytes32(0));
    }

    // ============ emitLog / emitLogs ============

    function test_emitLog_basic() public {
        bytes32 topic = bytes32(uint256(0xabcd));
        bytes memory data = hex"deadbeef";

        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, data);

        vm.prank(INITIAL_SYSTEM_ADDRESS);
        oracle.emitLog(topic, data);
    }

    function test_emitLog_revertsFromNonSystemAddress() public {
        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.emitLog(bytes32(uint256(0x1234)), hex"cafebabe");
    }

    function test_emitLogs_basic() public {
        bytes32 topic = bytes32(uint256(0xabcd));
        bytes[] memory dataVector = new bytes[](2);
        dataVector[0] = hex"deadbeef";
        dataVector[1] = hex"cafebabe";

        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, dataVector[0]);
        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(topic, dataVector[1]);

        vm.prank(INITIAL_SYSTEM_ADDRESS);
        oracle.emitLogs(topic, dataVector);
    }

    function test_emitLogs_revertsFromNonSystemAddress() public {
        bytes[] memory dataVector = new bytes[](1);
        dataVector[0] = hex"deadbeef";

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.emitLogs(bytes32(uint256(0x1234)), dataVector);
    }

    // ============ multiCall ============

    function test_multiCall_singleCall() public {
        bytes[] memory data = new bytes[](1);
        data[0] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(100), bytes32(uint256(0x1234)));

        vm.prank(INITIAL_SYSTEM_ADDRESS);
        oracle.multiCall(data);

        assertEq(oracle.getSlot(100), bytes32(uint256(0x1234)));
    }

    function test_multiCall_revertsFromNonSystemAddress() public {
        bytes[] memory data = new bytes[](1);
        data[0] = abi.encodeWithSelector(Oracle.setSlot.selector, uint256(300), bytes32(uint256(0x1234)));

        vm.prank(user);
        vm.expectRevert(Oracle.NotSystemAddress.selector);
        oracle.multiCall(data);
    }

    // ============ After system address change: emitLog follows new authority ============

    function test_afterSystemAddressChange_newSystemAddressCanEmitLog() public {
        uint256 activationBlock = block.number + 50;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, activationBlock);

        vm.roll(activationBlock);
        registry.applyPendingChanges();

        vm.expectEmit(true, false, false, true);
        emit Oracle.Log(bytes32(uint256(0x7777)), hex"cafe");

        vm.prank(newSystemAddress);
        oracle.emitLog(bytes32(uint256(0x7777)), hex"cafe");
    }
}
