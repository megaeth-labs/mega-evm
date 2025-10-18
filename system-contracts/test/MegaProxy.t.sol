// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {MegaProxy} from "../src/MegaProxy.sol";
import {Oracle} from "../src/Oracle.sol";
import {ProxyAdmin} from "@openzeppelin/contracts/proxy/transparent/ProxyAdmin.sol";
import {ITransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";

contract MegaProxyTest is Test {
    MegaProxy public proxy;
    ProxyAdmin public proxyAdmin;
    Oracle public oracleImplementation;
    Oracle public proxiedOracle;

    address public systemAddress;
    address public user;

    function setUp() public {
        systemAddress = 0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d; // Oracle's MEGA_SYSTEM_ADDRESS
        user = address(0x2);

        // Deploy implementation
        oracleImplementation = new Oracle(systemAddress);

        // Deploy proxy pointing to implementation
        // Note: TransparentUpgradeableProxy creates its own ProxyAdmin internally
        proxy = new MegaProxy(
            address(oracleImplementation),
            address(this), // initialOwner for the ProxyAdmin
            ""
        );

        // Get the ProxyAdmin address from the ERC1967 admin slot
        bytes32 adminSlot = 0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103;
        address adminAddress = address(uint160(uint256(vm.load(address(proxy), adminSlot))));
        proxyAdmin = ProxyAdmin(adminAddress);

        // Create interface to interact with proxied contract
        proxiedOracle = Oracle(address(proxy));
    }

    function testProxyCanSetAndGetSlot() public {
        bytes32 slot = bytes32(uint256(0));
        bytes32 value = bytes32(uint256(12345));

        vm.prank(systemAddress);
        proxiedOracle.setSlot(slot, value);

        assertEq(proxiedOracle.getSlot(slot), value);
    }

    function testUpgradeToNewImplementation() public {
        // Deploy new implementation
        Oracle newImplementation = new Oracle(systemAddress);

        // Upgrade through ProxyAdmin using the correct interface (v5 uses upgradeAndCall)
        proxyAdmin.upgradeAndCall(
            ITransparentUpgradeableProxy(address(proxy)), address(newImplementation), new bytes(0)
        );

        // Verify new implementation is being used (system address is constant, so should remain same)
        assertEq(proxiedOracle.MEGA_SYSTEM_ADDRESS(), systemAddress);
    }

    function testUpgradeFailsFromNonAdmin() public {
        Oracle newImplementation = new Oracle(systemAddress);

        vm.prank(user);
        vm.expectRevert();
        proxyAdmin.upgradeAndCall(
            ITransparentUpgradeableProxy(address(proxy)), address(newImplementation), new bytes(0)
        );
    }

    function testChangeProxyAdmin() public {
        address newAdmin = address(0x123);

        proxyAdmin.transferOwnership(newAdmin);

        assertEq(proxyAdmin.owner(), newAdmin);
    }

    function testProxyAdminCannotCallImplementationFunctions() public {
        bytes32 slot = bytes32(uint256(0));
        bytes32 value = bytes32(uint256(12345));

        // Even as proxy admin owner, we cannot call implementation functions
        // because transparent proxy pattern separates admin and user calls
        vm.expectRevert();
        proxiedOracle.setSlot(slot, value);
    }

    function testUserCannotAccessAdminFunctions() public {
        Oracle newImplementation = new Oracle(systemAddress);

        vm.startPrank(user);

        // User cannot upgrade
        vm.expectRevert();
        proxyAdmin.upgradeAndCall(
            ITransparentUpgradeableProxy(address(proxy)), address(newImplementation), new bytes(0)
        );

        vm.stopPrank();
    }

    function testStoragePersistsAcrossUpgrades() public {
        // Set some data through the proxy
        bytes32 slot = bytes32(uint256(0));
        bytes32 value = bytes32(uint256(0xdeadbeef));

        vm.prank(systemAddress);
        proxiedOracle.setSlot(slot, value);

        // Verify data is set
        assertEq(proxiedOracle.getSlot(slot), value);

        // Deploy new implementation
        Oracle newImplementation = new Oracle(systemAddress);

        // Upgrade
        proxyAdmin.upgradeAndCall(
            ITransparentUpgradeableProxy(address(proxy)), address(newImplementation), new bytes(0)
        );

        // Verify storage persists
        assertEq(proxiedOracle.getSlot(slot), value);
    }

    function testEIP1967StorageSlots() public view {
        // Verify EIP1967 implementation slot
        bytes32 implementationSlot = 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc;

        bytes32 implSlotValue = vm.load(address(proxy), implementationSlot);
        assertEq(address(uint160(uint256(implSlotValue))), address(oracleImplementation));
    }
}
