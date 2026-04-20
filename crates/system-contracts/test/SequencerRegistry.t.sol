// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {SequencerRegistry} from "../contracts/SequencerRegistry.sol";
import {ISequencerRegistry} from "../contracts/interfaces/ISequencerRegistry.sol";

contract SequencerRegistryTest is Test {
    SequencerRegistry public registry;

    address public constant INITIAL_SEQUENCER = 0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d;
    address public constant INITIAL_ADMIN = 0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d;

    address public nonAdmin = address(0xBEEF);
    address public newSequencer = address(0xCAFE);
    address public newAdmin = address(0xDEAD);

    function setUp() public {
        registry = new SequencerRegistry();
    }

    // ============ currentSequencer Tests ============

    function test_currentSequencer_returnsInitialOnFreshDeploy() public view {
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    // ============ admin Tests ============

    function test_admin_returnsInitialOnFreshDeploy() public view {
        assertEq(registry.admin(), INITIAL_ADMIN);
    }

    // ============ transferAdmin Tests ============

    function test_transferAdmin_revertsZeroAddress() public {
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.ZeroAddress.selector);
        registry.transferAdmin(address(0));
    }

    function test_transferAdmin_succeedsFromAdmin() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);
        assertEq(registry.admin(), newAdmin);
    }

    function test_transferAdmin_emitsEvent() public {
        vm.expectEmit(true, true, false, true);
        emit ISequencerRegistry.AdminTransferred(INITIAL_ADMIN, newAdmin);

        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);
    }

    function test_transferAdmin_revertsFromNonAdmin() public {
        vm.prank(nonAdmin);
        vm.expectRevert(ISequencerRegistry.NotAdmin.selector);
        registry.transferAdmin(newAdmin);
    }

    function test_transferAdmin_newAdminCanTransferAgain() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        address anotherAdmin = address(0x1234);
        vm.prank(newAdmin);
        registry.transferAdmin(anotherAdmin);
        assertEq(registry.admin(), anotherAdmin);
    }

    function test_transferAdmin_oldAdminCannotTransferAfterTransfer() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.NotAdmin.selector);
        registry.transferAdmin(address(0x9999));
    }

    // ============ scheduleNextSequencerChange Tests ============

    function test_schedule_succeedsFromAdmin() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        // Verify event was emitted (tested separately below) and state is pending
        // We cannot directly read _pendingSequencer, but we can verify via applyPendingChange
    }

    function test_schedule_emitsEvent() public {
        uint256 futureBlock = block.number + 100;

        vm.expectEmit(true, true, false, true);
        emit ISequencerRegistry.SequencerChangeScheduled(INITIAL_SEQUENCER, newSequencer, futureBlock);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);
    }

    function test_schedule_revertsFromNonAdmin() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(nonAdmin);
        vm.expectRevert(ISequencerRegistry.NotAdmin.selector);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);
    }

    function test_schedule_revertsZeroAddress() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.ZeroAddress.selector);
        registry.scheduleNextSequencerChange(address(0), futureBlock);
    }

    function test_schedule_revertsActivationBlockAtCurrentBlock() public {
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.InvalidActivationBlock.selector);
        registry.scheduleNextSequencerChange(newSequencer, block.number);
    }

    function test_schedule_revertsActivationBlockInPast() public {
        // Roll forward so we can test a past block
        vm.roll(100);

        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.InvalidActivationBlock.selector);
        registry.scheduleNextSequencerChange(newSequencer, 50);
    }

    function test_schedule_overwritePending() public {
        uint256 futureBlock1 = block.number + 100;
        uint256 futureBlock2 = block.number + 200;
        address sequencer2 = address(0xAAAA);

        // Schedule first
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock1);

        // Overwrite with second
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(sequencer2, futureBlock2);

        // Roll to futureBlock1 — applyPendingChange should NOT apply the first schedule
        vm.roll(futureBlock1);
        registry.applyPendingChange();
        // The first schedule was overwritten, so currentSequencer should still be INITIAL
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);

        // Roll to futureBlock2 — now it should apply
        vm.roll(futureBlock2);
        registry.applyPendingChange();
        assertEq(registry.currentSequencer(), sequencer2);
    }

    function test_schedule_cancelPending() public {
        uint256 futureBlock = block.number + 100;

        // Schedule a rotation
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        // Cancel by setting activationBlock to type(uint256).max
        vm.expectEmit(true, true, false, true);
        emit ISequencerRegistry.SequencerChangeScheduled(INITIAL_SEQUENCER, address(0), type(uint256).max);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(address(0), type(uint256).max);

        // Roll past the original activation block and apply — should be no-op
        vm.roll(futureBlock + 1);
        registry.applyPendingChange();
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    // ============ applyPendingChange Tests ============

    function test_applyPendingChange_noopWhenNoPending() public {
        // No schedule has been made; applyPendingChange should be a no-op
        registry.applyPendingChange();
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    function test_applyPendingChange_noopWhenNotYetDue() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        // Roll to a block before activation
        vm.roll(futureBlock - 1);
        registry.applyPendingChange();

        // Should still be the initial sequencer
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    function test_applyPendingChange_appliesWhenDue() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        // Roll to exactly the activation block
        vm.roll(futureBlock);
        registry.applyPendingChange();

        // currentSequencer should now be the new sequencer
        assertEq(registry.currentSequencer(), newSequencer);
    }

    function test_applyPendingChange_appliesWhenPastDue() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        // Roll past the activation block
        vm.roll(futureBlock + 50);
        registry.applyPendingChange();

        assertEq(registry.currentSequencer(), newSequencer);
    }

    function test_applyPendingChange_clearsPendingState() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        vm.roll(futureBlock);
        registry.applyPendingChange();

        // Calling applyPendingChange again should be a no-op (pending was cleared)
        registry.applyPendingChange();
        assertEq(registry.currentSequencer(), newSequencer);
    }

    function test_applyPendingChange_appendsRotationHistory() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        vm.roll(futureBlock);
        registry.applyPendingChange();

        // Verify rotation history via sequencerAt
        assertEq(registry.sequencerAt(futureBlock), newSequencer);
        assertEq(registry.sequencerAt(futureBlock - 1), INITIAL_SEQUENCER);
    }

    function test_applyPendingChange_permissionless() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        // Any address can call applyPendingChange
        vm.roll(futureBlock);
        vm.prank(nonAdmin);
        registry.applyPendingChange();

        assertEq(registry.currentSequencer(), newSequencer);
    }

    // ============ sequencerAt Tests ============

    function test_sequencerAt_revertsFutureBlock() public {
        vm.expectRevert(ISequencerRegistry.FutureBlock.selector);
        registry.sequencerAt(block.number + 1);
    }

    function test_sequencerAt_returnsInitialWhenNoRotations() public view {
        assertEq(registry.sequencerAt(block.number), INITIAL_SEQUENCER);
    }

    function test_sequencerAt_returnsInitialForBlockZero() public view {
        assertEq(registry.sequencerAt(0), INITIAL_SEQUENCER);
    }

    function test_sequencerAt_correctRangesAfterRotation() public {
        uint256 rotationBlock = 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, rotationBlock);

        vm.roll(rotationBlock);
        registry.applyPendingChange();

        // Before rotation: should return INITIAL_SEQUENCER
        assertEq(registry.sequencerAt(0), INITIAL_SEQUENCER);
        assertEq(registry.sequencerAt(rotationBlock - 1), INITIAL_SEQUENCER);

        // At and after rotation: should return newSequencer
        assertEq(registry.sequencerAt(rotationBlock), newSequencer);
    }

    function test_sequencerAt_multipleRotations() public {
        address sequencer2 = address(0xAAAA);
        address sequencer3 = address(0xBBBB);

        uint256 block1 = 100;
        uint256 block2 = 200;
        uint256 block3 = 300;

        // First rotation
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, block1);
        vm.roll(block1);
        registry.applyPendingChange();

        // Second rotation
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(sequencer2, block2);
        vm.roll(block2);
        registry.applyPendingChange();

        // Third rotation
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(sequencer3, block3);
        vm.roll(block3);
        registry.applyPendingChange();

        // Verify correct sequencer for each range
        assertEq(registry.sequencerAt(0), INITIAL_SEQUENCER);
        assertEq(registry.sequencerAt(block1 - 1), INITIAL_SEQUENCER);
        assertEq(registry.sequencerAt(block1), newSequencer);
        assertEq(registry.sequencerAt(block1 + 50), newSequencer);
        assertEq(registry.sequencerAt(block2 - 1), newSequencer);
        assertEq(registry.sequencerAt(block2), sequencer2);
        assertEq(registry.sequencerAt(block2 + 50), sequencer2);
        assertEq(registry.sequencerAt(block3 - 1), sequencer2);
        assertEq(registry.sequencerAt(block3), sequencer3);
    }

    // ============ version Tests ============

    function test_version() public view {
        assertEq(registry.version(), "1.0.0");
    }
}
