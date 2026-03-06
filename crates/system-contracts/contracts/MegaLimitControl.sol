// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ISemver} from "./interfaces/ISemver.sol";
import {IMegaLimitControl} from "./interfaces/IMegaLimitControl.sol";

/// @title MegaLimitControl
/// @author MegaETH
/// @notice System contract for limit-related query and control methods.
/// @dev The function body is never executed on MegaETH because the call is intercepted by the EVM.
contract MegaLimitControl is ISemver, IMegaLimitControl {
    /// @notice Returns the semantic version of this contract.
    /// @return version string in semver format.
    function version() external pure returns (string memory) {
        return "1.0.0";
    }

    /// @inheritdoc IMegaLimitControl
    function remainingComputeGas() external view returns (uint64) {
        // This function body is never executed - the call is intercepted by the EVM.
        revert NotIntercepted();
    }

    /// @notice Fallback for unknown selectors.
    /// @dev Ensures non-intercepted calls revert with a stable custom error payload.
    fallback() external payable {
        revert NotIntercepted();
    }
}
