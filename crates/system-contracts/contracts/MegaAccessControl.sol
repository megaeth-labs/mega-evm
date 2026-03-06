// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ISemver} from "./interfaces/ISemver.sol";
import {IMegaAccessControl} from "./interfaces/IMegaAccessControl.sol";

/// @title MegaAccessControl
/// @author MegaETH
/// @notice System contract for controlling resource limits in EVM execution.
/// @dev The actual enforcement logic is intercepted by the MegaETH EVM during execution.
///      This contract serves as the ABI definition and deployment target.
contract MegaAccessControl is ISemver, IMegaAccessControl {
    /// @notice Returns the semantic version of this contract.
    /// @return version string in semver format.
    function version() external pure returns (string memory) {
        return "1.0.0";
    }

    /// @inheritdoc IMegaAccessControl
    function disableVolatileDataAccess() external view {
        // This function body is never executed - the call is intercepted by the EVM.
        revert NotIntercepted();
    }

    /// @inheritdoc IMegaAccessControl
    function enableVolatileDataAccess() external view {
        // This function body is never executed - the call is intercepted by the EVM.
        revert NotIntercepted();
    }

    /// @inheritdoc IMegaAccessControl
    function isVolatileDataAccessDisabled() external view returns (bool) {
        // This function body is never executed - the call is intercepted by the EVM.
        revert NotIntercepted();
    }

    /// @notice Fallback for unknown selectors.
    /// @dev Ensures non-intercepted calls revert with a stable custom error payload.
    fallback() external payable {
        revert NotIntercepted();
    }
}
