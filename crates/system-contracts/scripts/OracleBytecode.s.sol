// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {Oracle} from "../contracts/Oracle.sol";

/// @title SaveOracleBytecode
/// @notice Script to deploy Oracle contract and save its deployed bytecode to a file
/// @dev Run with: forge script scripts/OracleBytecode.s.sol:SaveOracleBytecode --sig "run(address)" <megaSystemAddress>
contract SaveOracleBytecode is Script {
    function run(address megaSystemAddress) public {
        vm.startBroadcast();

        // Deploy the Oracle contract with the provided system address
        Oracle oracle = new Oracle(megaSystemAddress);

        vm.stopBroadcast();

        // Get the deployed bytecode and version
        bytes memory deployedBytecode = address(oracle).code;
        string memory version = oracle.version();

        // Calculate code hash
        bytes32 codeHash = keccak256(deployedBytecode);
        string memory bytecodeHex = vm.toString(deployedBytecode);

        // Write to a JSON file with metadata
        string memory json = string.concat(
            "{\n",
            '  "version": "',
            version,
            '",\n',
            '  "systemAddress": "',
            vm.toString(megaSystemAddress),
            '",\n',
            '  "bytecodeLength": ',
            vm.toString(deployedBytecode.length),
            ",\n",
            '  "codeHash": "',
            vm.toString(codeHash),
            '",\n',
            '  "deployedBytecode": "',
            bytecodeHex,
            '"\n',
            "}"
        );
        vm.writeFile("artifacts/Oracle.json", json);
    }

    /// @notice Run with default system address for testing
    function run() public {
        // Default to MEGA_SYSTEM_ADDRESS
        address defaultSystemAddress = address(0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d);
        run(defaultSystemAddress);
    }
}
