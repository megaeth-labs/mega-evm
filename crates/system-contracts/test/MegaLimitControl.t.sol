// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {MegaLimitControl} from "../contracts/MegaLimitControl.sol";
import {IMegaLimitControl} from "../contracts/interfaces/IMegaLimitControl.sol";

/// @notice The bytecode a node deploys, run with no engine interceptor in front of it.
///         `remainingComputeGas` reverts `NotIntercepted`. The payable fallback reverts
///         with the same error for an unknown selector, with or without value. A
///         value-bearing call to the declared method never reaches that fallback: the
///         method is not payable, so the dispatcher reverts with empty data.
contract MegaLimitControlTest is Test {
    MegaLimitControl internal control;

    bytes internal constant UNKNOWN = hex"deadbeef";

    function setUp() public {
        control = new MegaLimitControl();
    }

    function test_version() public view {
        assertEq(control.version(), "1.0.0");
    }

    function test_remainingComputeGas_revertsNotIntercepted() public {
        vm.expectRevert(IMegaLimitControl.NotIntercepted.selector);
        control.remainingComputeGas();
    }

    function test_remainingComputeGas_staticcallRevertsNotIntercepted() public view {
        (bool ok, bytes memory data) =
            address(control).staticcall(abi.encodeCall(IMegaLimitControl.remainingComputeGas, ()));
        assertFalse(ok);
        assertEq(data, notIntercepted());
    }

    function test_unknownSelector_revertsNotIntercepted() public {
        (bool ok, bytes memory data) = address(control).call(UNKNOWN);
        assertFalse(ok);
        assertEq(data, notIntercepted());
    }

    function test_unknownSelector_withValueRevertsNotIntercepted() public {
        (bool ok, bytes memory data) = address(control).call{value: 1}(UNKNOWN);
        assertFalse(ok);
        assertEq(data, notIntercepted());
    }

    function test_unknownSelector_staticcallRevertsNotIntercepted() public view {
        (bool ok, bytes memory data) = address(control).staticcall(UNKNOWN);
        assertFalse(ok);
        assertEq(data, notIntercepted());
    }

    function test_knownSelector_withValueRevertsBeforeTheBody() public {
        (bool ok, bytes memory data) =
            address(control).call{value: 1}(abi.encodeCall(IMegaLimitControl.remainingComputeGas, ()));
        assertFalse(ok);
        assertEq(data, "");
    }

    function notIntercepted() internal pure returns (bytes memory) {
        return abi.encodeWithSelector(IMegaLimitControl.NotIntercepted.selector);
    }
}
