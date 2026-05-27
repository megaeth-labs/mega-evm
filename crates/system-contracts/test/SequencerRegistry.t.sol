// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {SequencerRegistry} from "../contracts/SequencerRegistry.sol";
import {ISequencerRegistry} from "../contracts/interfaces/ISequencerRegistry.sol";

contract SequencerRegistryTest is Test {
    SequencerRegistry public registry;

    address public constant REGISTRY_ADDRESS = 0x6342000000000000000000000000000000000006;

    address public constant INITIAL_SYSTEM_ADDRESS = address(0xAA);
    address public constant INITIAL_SEQUENCER = address(0xBB);
    address public constant INITIAL_ADMIN = address(0xCC);
    uint256 public constant INITIAL_FROM_BLOCK = 1;

    address public nonAdmin = address(0xBEEF);
    address public newSystemAddress = address(0xCAFE);
    address public newSequencer = address(0xFACE);
    address public newAdmin = address(0xDEAD);

    function setUp() public {
        // Deploy SequencerRegistry implementation, then etch bytecode at the canonical address.
        SequencerRegistry impl = new SequencerRegistry();
        vm.etch(REGISTRY_ADDRESS, address(impl).code);
        registry = SequencerRegistry(REGISTRY_ADDRESS);

        // Seed initial storage via vm.store (no constructor).
        // Slot layout:
        //   0: _currentSystemAddress
        //   1: _currentSequencer
        //   2: _admin
        //   3: _pendingAdmin (left at zero — no pending transfer at bootstrap)
        //   4: _initialSystemAddress
        //   5: _initialSequencer
        //   6: _initialFromBlock
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(0)), bytes32(uint256(uint160(INITIAL_SYSTEM_ADDRESS))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(1)), bytes32(uint256(uint160(INITIAL_SEQUENCER))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(2)), bytes32(uint256(uint160(INITIAL_ADMIN))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(4)), bytes32(uint256(uint160(INITIAL_SYSTEM_ADDRESS))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(5)), bytes32(uint256(uint160(INITIAL_SEQUENCER))));
        vm.store(REGISTRY_ADDRESS, bytes32(uint256(6)), bytes32(uint256(INITIAL_FROM_BLOCK)));

        // Start at block >= INITIAL_FROM_BLOCK so lookups work.
        vm.roll(INITIAL_FROM_BLOCK);
    }

    // ============ version ============

    function test_version() public view {
        assertEq(registry.version(), "1.0.0");
    }

    // ============ currentSystemAddress / currentSequencer / admin ============

    function test_currentSystemAddress_returnsSeededValue() public view {
        assertEq(registry.currentSystemAddress(), INITIAL_SYSTEM_ADDRESS);
    }

    function test_currentSequencer_returnsSeededValue() public view {
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    function test_admin_returnsSeededValue() public view {
        assertEq(registry.admin(), INITIAL_ADMIN);
    }

    function test_setUp_pendingAdminIsZero() public view {
        // Slot 3 must be empty at bootstrap; `transferAdmin` is the only way to populate it.
        // This test is the symmetric guard to `OracleTest::test_setUp_seedsRegistryBootstrapStateCorrectly`:
        // it fails immediately if a future fixture or layout change accidentally writes to slot 3.
        assertEq(registry.pendingAdmin(), address(0));
    }

    // ============ transferAdmin (two-step: schedule pending) ============

    function test_transferAdmin_setsPendingButDoesNotChangeAdmin() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        // Current admin is unchanged until acceptance.
        assertEq(registry.admin(), INITIAL_ADMIN);
        assertEq(registry.pendingAdmin(), newAdmin);
    }

    function test_transferAdmin_emitsAdminTransferStarted() public {
        vm.expectEmit(true, true, false, false);
        emit ISequencerRegistry.AdminTransferStarted(INITIAL_ADMIN, newAdmin);

        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);
    }

    function test_transferAdmin_doesNotEmitAdminTransferred() public {
        // AdminTransferred is reserved for the accept step. Recording logs proves it is not
        // emitted on schedule.
        vm.recordLogs();
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        bytes32 transferredSig = keccak256("AdminTransferred(address,address)");
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            assertTrue(logs[i].topics[0] != transferredSig, "transfer must not emit AdminTransferred");
        }
    }

    function test_transferAdmin_revertsNotAdmin() public {
        vm.prank(nonAdmin);
        vm.expectRevert(ISequencerRegistry.NotAdmin.selector);
        registry.transferAdmin(newAdmin);
    }

    function test_transferAdmin_zeroCancelsPending() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);
        assertEq(registry.pendingAdmin(), newAdmin);

        vm.expectEmit(true, true, false, false);
        emit ISequencerRegistry.AdminTransferStarted(INITIAL_ADMIN, address(0));

        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(address(0));
        assertEq(registry.pendingAdmin(), address(0));
        assertEq(registry.admin(), INITIAL_ADMIN, "current admin must not change on cancel");
    }

    function test_transferAdmin_overwritesPending() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        address otherCandidate = address(0x9999);
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(otherCandidate);
        assertEq(registry.pendingAdmin(), otherCandidate);

        // Original `newAdmin` must no longer be able to accept.
        vm.prank(newAdmin);
        vm.expectRevert(ISequencerRegistry.NotPendingAdmin.selector);
        registry.acceptAdmin();
    }

    function test_transferAdmin_pendingDoesNotGrantAdminPowers() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        // pendingAdmin cannot act as admin until they accept.
        vm.prank(newAdmin);
        vm.expectRevert(ISequencerRegistry.NotAdmin.selector);
        registry.scheduleNextSystemAddressChange(address(0xDEAD), block.number + 1);
    }

    // ============ acceptAdmin ============

    function test_acceptAdmin_promotesPendingAndClearsSlot() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        vm.prank(newAdmin);
        registry.acceptAdmin();

        assertEq(registry.admin(), newAdmin);
        assertEq(registry.pendingAdmin(), address(0));
    }

    function test_acceptAdmin_emitsAdminTransferred() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        vm.expectEmit(true, true, false, false);
        emit ISequencerRegistry.AdminTransferred(INITIAL_ADMIN, newAdmin);

        vm.prank(newAdmin);
        registry.acceptAdmin();
    }

    function test_acceptAdmin_revertsNotPendingAdmin() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);

        vm.prank(nonAdmin);
        vm.expectRevert(ISequencerRegistry.NotPendingAdmin.selector);
        registry.acceptAdmin();
    }

    function test_acceptAdmin_revertsWhenNoPending() public {
        // No transfer has been started — pending is the default zero address. Even the old admin
        // must be rejected because msg.sender != address(0).
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.NotPendingAdmin.selector);
        registry.acceptAdmin();
    }

    function test_acceptAdmin_oldAdminCannotActAfterAccept() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);
        vm.prank(newAdmin);
        registry.acceptAdmin();

        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.NotAdmin.selector);
        registry.transferAdmin(address(0x1234));
    }

    function test_acceptAdmin_isOneShot() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);
        vm.prank(newAdmin);
        registry.acceptAdmin();

        // Pending is cleared; calling acceptAdmin again must revert.
        vm.prank(newAdmin);
        vm.expectRevert(ISequencerRegistry.NotPendingAdmin.selector);
        registry.acceptAdmin();
    }

    function test_fullHandoff_newAdminCanActAfterAccept() public {
        vm.prank(INITIAL_ADMIN);
        registry.transferAdmin(newAdmin);
        vm.prank(newAdmin);
        registry.acceptAdmin();

        // New admin can perform admin-only operations.
        vm.prank(newAdmin);
        registry.scheduleNextSystemAddressChange(address(0xCAFE), block.number + 1);
    }

    // ============ scheduleNextSystemAddressChange ============

    function test_scheduleNextSystemAddressChange_success() public {
        uint256 futureBlock = block.number + 100;

        vm.expectEmit(true, true, false, true);
        emit ISequencerRegistry.SystemAddressChangeScheduled(INITIAL_SYSTEM_ADDRESS, newSystemAddress, futureBlock);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, futureBlock);
    }

    function test_scheduleNextSystemAddressChange_revertsNotAdmin() public {
        vm.prank(nonAdmin);
        vm.expectRevert(ISequencerRegistry.NotAdmin.selector);
        registry.scheduleNextSystemAddressChange(newSystemAddress, block.number + 100);
    }

    function test_scheduleNextSystemAddressChange_revertsZeroAddress() public {
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.ZeroAddress.selector);
        registry.scheduleNextSystemAddressChange(address(0), block.number + 100);
    }

    function test_scheduleNextSystemAddressChange_revertsInvalidActivationBlock_current() public {
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.InvalidActivationBlock.selector);
        registry.scheduleNextSystemAddressChange(newSystemAddress, block.number);
    }

    function test_scheduleNextSystemAddressChange_revertsInvalidActivationBlock_past() public {
        vm.roll(100);
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.InvalidActivationBlock.selector);
        registry.scheduleNextSystemAddressChange(newSystemAddress, 50);
    }

    function test_scheduleNextSystemAddressChange_revertsActivationBlockTooLarge() public {
        uint256 tooLarge = uint256(type(uint96).max) + 1;
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.ActivationBlockTooLarge.selector);
        registry.scheduleNextSystemAddressChange(newSystemAddress, tooLarge);
    }

    function test_scheduleNextSystemAddressChange_overwrite() public {
        uint256 futureBlock1 = block.number + 100;
        uint256 futureBlock2 = block.number + 200;
        address addr2 = address(0xAAAA);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, futureBlock1);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(addr2, futureBlock2);

        // Roll to futureBlock1 -- first schedule was overwritten, should NOT apply.
        vm.roll(futureBlock1);
        registry.applyPendingChanges();
        assertEq(registry.currentSystemAddress(), INITIAL_SYSTEM_ADDRESS);

        // Roll to futureBlock2 -- now it should apply.
        vm.roll(futureBlock2);
        registry.applyPendingChanges();
        assertEq(registry.currentSystemAddress(), addr2);
    }

    function test_scheduleNextSystemAddressChange_cancel() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, futureBlock);

        // Cancel: activationBlock = type(uint256).max, address = 0
        vm.expectEmit(true, true, false, true);
        emit ISequencerRegistry.SystemAddressChangeScheduled(INITIAL_SYSTEM_ADDRESS, address(0), type(uint256).max);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(address(0), type(uint256).max);

        // Roll past original activation -- should be no-op.
        vm.roll(futureBlock + 1);
        registry.applyPendingChanges();
        assertEq(registry.currentSystemAddress(), INITIAL_SYSTEM_ADDRESS);
    }

    // ============ scheduleNextSequencerChange ============

    function test_scheduleNextSequencerChange_success() public {
        uint256 futureBlock = block.number + 100;

        vm.expectEmit(true, true, false, true);
        emit ISequencerRegistry.SequencerChangeScheduled(INITIAL_SEQUENCER, newSequencer, futureBlock);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);
    }

    function test_scheduleNextSequencerChange_revertsNotAdmin() public {
        vm.prank(nonAdmin);
        vm.expectRevert(ISequencerRegistry.NotAdmin.selector);
        registry.scheduleNextSequencerChange(newSequencer, block.number + 100);
    }

    function test_scheduleNextSequencerChange_revertsZeroAddress() public {
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.ZeroAddress.selector);
        registry.scheduleNextSequencerChange(address(0), block.number + 100);
    }

    function test_scheduleNextSequencerChange_revertsInvalidActivationBlock_current() public {
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.InvalidActivationBlock.selector);
        registry.scheduleNextSequencerChange(newSequencer, block.number);
    }

    function test_scheduleNextSequencerChange_revertsInvalidActivationBlock_past() public {
        vm.roll(100);
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.InvalidActivationBlock.selector);
        registry.scheduleNextSequencerChange(newSequencer, 50);
    }

    function test_scheduleNextSequencerChange_revertsActivationBlockTooLarge() public {
        uint256 tooLarge = uint256(type(uint96).max) + 1;
        vm.prank(INITIAL_ADMIN);
        vm.expectRevert(ISequencerRegistry.ActivationBlockTooLarge.selector);
        registry.scheduleNextSequencerChange(newSequencer, tooLarge);
    }

    function test_scheduleNextSequencerChange_overwrite() public {
        uint256 futureBlock1 = block.number + 100;
        uint256 futureBlock2 = block.number + 200;
        address addr2 = address(0xAAAA);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock1);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(addr2, futureBlock2);

        vm.roll(futureBlock1);
        registry.applyPendingChanges();
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);

        vm.roll(futureBlock2);
        registry.applyPendingChanges();
        assertEq(registry.currentSequencer(), addr2);
    }

    function test_scheduleNextSequencerChange_cancel() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        vm.expectEmit(true, true, false, true);
        emit ISequencerRegistry.SequencerChangeScheduled(INITIAL_SEQUENCER, address(0), type(uint256).max);

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(address(0), type(uint256).max);

        vm.roll(futureBlock + 1);
        registry.applyPendingChanges();
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    // ============ applyPendingChanges ============

    function test_applyPendingChanges_noopWhenNothingPending() public {
        registry.applyPendingChanges();
        assertEq(registry.currentSystemAddress(), INITIAL_SYSTEM_ADDRESS);
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    function test_applyPendingChanges_noopWhenNotYetDue() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, futureBlock);
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        vm.roll(futureBlock - 1);
        registry.applyPendingChanges();

        assertEq(registry.currentSystemAddress(), INITIAL_SYSTEM_ADDRESS);
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    function test_applyPendingChanges_appliesSystemAddressWhenDue() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, futureBlock);

        vm.roll(futureBlock);
        registry.applyPendingChanges();

        assertEq(registry.currentSystemAddress(), newSystemAddress);
        // Sequencer should be unchanged.
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    function test_applyPendingChanges_appliesSequencerWhenDue() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        vm.roll(futureBlock);
        registry.applyPendingChanges();

        assertEq(registry.currentSequencer(), newSequencer);
        // System address should be unchanged.
        assertEq(registry.currentSystemAddress(), INITIAL_SYSTEM_ADDRESS);
    }

    function test_applyPendingChanges_appliesBothWhenBothDue() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, futureBlock);
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        vm.roll(futureBlock);
        registry.applyPendingChanges();

        assertEq(registry.currentSystemAddress(), newSystemAddress);
        assertEq(registry.currentSequencer(), newSequencer);
    }

    function test_applyPendingChanges_clearsPendingState() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, futureBlock);

        vm.roll(futureBlock);
        registry.applyPendingChanges();

        // Calling again should be a no-op.
        registry.applyPendingChanges();
        assertEq(registry.currentSystemAddress(), newSystemAddress);
    }

    function test_applyPendingChanges_permissionless() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        vm.roll(futureBlock);
        vm.prank(nonAdmin);
        registry.applyPendingChanges();

        assertEq(registry.currentSequencer(), newSequencer);
    }

    // ============ systemAddressAt ============

    function test_systemAddressAt_revertsFutureBlock() public {
        vm.expectRevert(ISequencerRegistry.FutureBlock.selector);
        registry.systemAddressAt(block.number + 1);
    }

    function test_systemAddressAt_revertsBeforeInitialBlock() public {
        vm.roll(10);
        vm.expectRevert(ISequencerRegistry.BeforeInitialBlock.selector);
        registry.systemAddressAt(INITIAL_FROM_BLOCK - 1);
    }

    function test_systemAddressAt_returnsInitialWhenNoChanges() public view {
        assertEq(registry.systemAddressAt(block.number), INITIAL_SYSTEM_ADDRESS);
    }

    function test_systemAddressAt_correctRangesAfterChange() public {
        uint256 changeBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, changeBlock);

        vm.roll(changeBlock);
        registry.applyPendingChanges();

        // Before change
        assertEq(registry.systemAddressAt(INITIAL_FROM_BLOCK), INITIAL_SYSTEM_ADDRESS);
        assertEq(registry.systemAddressAt(changeBlock - 1), INITIAL_SYSTEM_ADDRESS);
        // At and after change
        assertEq(registry.systemAddressAt(changeBlock), newSystemAddress);
    }

    function test_systemAddressAt_multipleChanges() public {
        address addr2 = address(0xAAAA);
        address addr3 = address(0xBBBB);
        uint256 block1 = 100;
        uint256 block2 = 200;
        uint256 block3 = 300;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, block1);
        vm.roll(block1);
        registry.applyPendingChanges();

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(addr2, block2);
        vm.roll(block2);
        registry.applyPendingChanges();

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(addr3, block3);
        vm.roll(block3);
        registry.applyPendingChanges();

        assertEq(registry.systemAddressAt(INITIAL_FROM_BLOCK), INITIAL_SYSTEM_ADDRESS);
        assertEq(registry.systemAddressAt(block1 - 1), INITIAL_SYSTEM_ADDRESS);
        assertEq(registry.systemAddressAt(block1), newSystemAddress);
        assertEq(registry.systemAddressAt(block1 + 50), newSystemAddress);
        assertEq(registry.systemAddressAt(block2 - 1), newSystemAddress);
        assertEq(registry.systemAddressAt(block2), addr2);
        assertEq(registry.systemAddressAt(block2 + 50), addr2);
        assertEq(registry.systemAddressAt(block3 - 1), addr2);
        assertEq(registry.systemAddressAt(block3), addr3);
    }

    // ============ sequencerAt ============

    function test_sequencerAt_revertsFutureBlock() public {
        vm.expectRevert(ISequencerRegistry.FutureBlock.selector);
        registry.sequencerAt(block.number + 1);
    }

    function test_sequencerAt_revertsBeforeInitialBlock() public {
        vm.roll(10);
        vm.expectRevert(ISequencerRegistry.BeforeInitialBlock.selector);
        registry.sequencerAt(INITIAL_FROM_BLOCK - 1);
    }

    function test_sequencerAt_returnsInitialWhenNoChanges() public view {
        assertEq(registry.sequencerAt(block.number), INITIAL_SEQUENCER);
    }

    function test_sequencerAt_correctRangesAfterChange() public {
        uint256 changeBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, changeBlock);

        vm.roll(changeBlock);
        registry.applyPendingChanges();

        assertEq(registry.sequencerAt(INITIAL_FROM_BLOCK), INITIAL_SEQUENCER);
        assertEq(registry.sequencerAt(changeBlock - 1), INITIAL_SEQUENCER);
        assertEq(registry.sequencerAt(changeBlock), newSequencer);
    }

    function test_sequencerAt_multipleChanges() public {
        address addr2 = address(0xAAAA);
        address addr3 = address(0xBBBB);
        uint256 block1 = 100;
        uint256 block2 = 200;
        uint256 block3 = 300;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, block1);
        vm.roll(block1);
        registry.applyPendingChanges();

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(addr2, block2);
        vm.roll(block2);
        registry.applyPendingChanges();

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(addr3, block3);
        vm.roll(block3);
        registry.applyPendingChanges();

        assertEq(registry.sequencerAt(INITIAL_FROM_BLOCK), INITIAL_SEQUENCER);
        assertEq(registry.sequencerAt(block1 - 1), INITIAL_SEQUENCER);
        assertEq(registry.sequencerAt(block1), newSequencer);
        assertEq(registry.sequencerAt(block1 + 50), newSequencer);
        assertEq(registry.sequencerAt(block2 - 1), newSequencer);
        assertEq(registry.sequencerAt(block2), addr2);
        assertEq(registry.sequencerAt(block2 + 50), addr2);
        assertEq(registry.sequencerAt(block3 - 1), addr2);
        assertEq(registry.sequencerAt(block3), addr3);
    }

    // ============ Independent changes ============

    function test_changeSystemAddress_doesNotChangeSequencer() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, futureBlock);

        vm.roll(futureBlock);
        registry.applyPendingChanges();

        assertEq(registry.currentSystemAddress(), newSystemAddress);
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);
    }

    function test_changeSequencer_doesNotChangeSystemAddress() public {
        uint256 futureBlock = block.number + 100;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, futureBlock);

        vm.roll(futureBlock);
        registry.applyPendingChanges();

        assertEq(registry.currentSequencer(), newSequencer);
        assertEq(registry.currentSystemAddress(), INITIAL_SYSTEM_ADDRESS);
    }

    function test_independentChanges_differentBlocks() public {
        uint256 sysBlock = block.number + 100;
        uint256 seqBlock = block.number + 200;

        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSystemAddressChange(newSystemAddress, sysBlock);
        vm.prank(INITIAL_ADMIN);
        registry.scheduleNextSequencerChange(newSequencer, seqBlock);

        // After system address change only
        vm.roll(sysBlock);
        registry.applyPendingChanges();
        assertEq(registry.currentSystemAddress(), newSystemAddress);
        assertEq(registry.currentSequencer(), INITIAL_SEQUENCER);

        // After sequencer change
        vm.roll(seqBlock);
        registry.applyPendingChanges();
        assertEq(registry.currentSystemAddress(), newSystemAddress);
        assertEq(registry.currentSequencer(), newSequencer);
    }
}
