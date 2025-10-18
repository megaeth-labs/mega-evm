// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";

/// @title MegaProxy
/// @author MegaETH
/// @notice EIP1967 transparent proxy with semantic versioning support.
/// @dev Extends OpenZeppelin's TransparentUpgradeableProxy with ISemver interface.
///      This provides a battle-tested, audited proxy implementation while maintaining
///      compatibility with the MegaETH versioning system.
contract MegaProxy is TransparentUpgradeableProxy {
    /// @notice Constructor sets the initial implementation and admin addresses.
    /// @param _logic The address of the initial implementation contract.
    /// @param _admin The address of the initial admin (typically a ProxyAdmin contract).
    /// @param _data Initialization data to be passed to the implementation's initializer.
    constructor(address _logic, address _admin, bytes memory _data)
        TransparentUpgradeableProxy(_logic, _admin, _data)
    {}

    /// @notice Receive function that delegates calls to the implementation.
    /// @dev This is used to receive ETH from the implementation.
    receive() external payable {
        _delegate(_implementation());
    }
}
