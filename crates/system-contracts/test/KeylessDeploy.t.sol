// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {KeylessDeploy} from "../contracts/KeylessDeploy.sol";
import {IKeylessDeploy} from "../contracts/interfaces/IKeylessDeploy.sol";

/// @notice The bytecode a node deploys, run with no engine interceptor in front of it.
///         `version` is the path that returns. `keylessDeploy` reverts `NotIntercepted`.
///         The contract has no fallback, so an unknown selector reverts with empty data,
///         and a value-bearing call does too: the method is not payable, so the dispatcher
///         reverts before the body.
contract KeylessDeployTest is Test {
    KeylessDeploy internal deploy;

    function setUp() public {
        deploy = new KeylessDeploy();
    }

    function test_version() public view {
        assertEq(deploy.version(), "1.0.0");
    }

    function test_keylessDeploy_revertsNotIntercepted() public {
        vm.expectRevert(IKeylessDeploy.NotIntercepted.selector);
        deploy.keylessDeploy("", 0);
    }

    function test_keylessDeploy_staticcallRevertsNotIntercepted() public view {
        (bool ok, bytes memory data) = address(deploy).staticcall(abi.encodeCall(IKeylessDeploy.keylessDeploy, ("", 0)));
        assertFalse(ok);
        assertEq(data, abi.encodeWithSelector(IKeylessDeploy.NotIntercepted.selector));
    }

    function test_keylessDeploy_withValueRevertsBeforeTheBody() public {
        (bool ok, bytes memory data) =
            address(deploy).call{value: 1}(abi.encodeCall(IKeylessDeploy.keylessDeploy, ("", 0)));
        assertFalse(ok);
        assertEq(data, "");
    }

    function test_unknownSelector_revertsWithEmptyData() public {
        (bool ok, bytes memory data) = address(deploy).call(hex"deadbeef");
        assertFalse(ok);
        assertEq(data, "");
    }

    function test_unknownSelector_withValueRevertsWithEmptyData() public {
        (bool ok, bytes memory data) = address(deploy).call{value: 1}(hex"deadbeef");
        assertFalse(ok);
        assertEq(data, "");
    }

    function test_unknownSelector_staticcallRevertsWithEmptyData() public view {
        (bool ok, bytes memory data) = address(deploy).staticcall(hex"deadbeef");
        assertFalse(ok);
        assertEq(data, "");
    }
}
