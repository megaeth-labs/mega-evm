// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ISemver} from "./interfaces/ISemver.sol";
import {IKeylessDeploy} from "./interfaces/IKeylessDeploy.sol";

/// @title KeylessDeploy
/// @author MegaETH
/// @notice System contract for deploying contracts using pre-EIP-155 transactions (Nick's Method).
/// @dev This contract enables keyless deployment with custom gas limits for MegaETH's gas model.
///      The actual execution logic is intercepted by the MegaETH EVM during execution.
contract KeylessDeploy is ISemver, IKeylessDeploy {
    /// @notice Returns the semantic version of this contract.
    /// @return version string in semver format.
    function version() external pure returns (string memory) {
        return "1.0.0";
    }

    /// @inheritdoc IKeylessDeploy
    function keylessDeploy(bytes calldata keylessDeploymentTransaction, uint256 gasLimitOverride)
        external
        returns (uint64 gasUsed, address deployedAddress)
    {
        // This function body is never executed - the call is intercepted by the EVM.
        // The assembly block prevents the compiler from optimizing away the function.
        revert("KeylessDeploy: not intercepted");
    }
}
