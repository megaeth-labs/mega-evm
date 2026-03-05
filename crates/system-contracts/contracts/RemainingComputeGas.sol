// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ISemver} from "./interfaces/ISemver.sol";
import {IRemainingComputeGas} from "./interfaces/IRemainingComputeGas.sol";

/// @title RemainingComputeGas
/// @author MegaETH
/// @notice System contract for querying remaining compute gas.
/// @dev The function body is never executed on MegaETH because the call is intercepted by the EVM.
contract RemainingComputeGas is ISemver, IRemainingComputeGas {
    /// @notice Returns the semantic version of this contract.
    /// @return version string in semver format.
    function version() external pure returns (string memory) {
        return "1.0.0";
    }

    /// @inheritdoc IRemainingComputeGas
    function remainingComputeGas() external view returns (uint64) {
        // This function body is never executed - the call is intercepted by the EVM.
        revert NotIntercepted();
    }
}
